//! Outillage des tests d'intégration « boîte noire » du moteur `apex-scada`.
//!
//! Chaque test lance le **binaire compilé** (`CARGO_BIN_EXE_apex-scada`) :
//! - dans un dossier temporaire neuf et unique (sous `CARGO_TARGET_TMPDIR`), en mode portable ;
//! - avec un fichier `--config` écrit par le test (simulation active, mode sûr actif) ;
//! - sur des ports libres choisis dynamiquement (jamais 3000 ni 5502) ;
//! - avec un environnement nettoyé (`RUST_LOG`, `APEX_SCADA_CONFIG`, `SCADA_*`...).
//!
//! Le dialogue passe par un client HTTP/1.1 et WebSocket minimal écrit sur `std::net` :
//! aucune dépendance supplémentaire (seul `serde_json`, déjà utilisé par le serveur, sert à
//! lire les réponses). Les gardes `Drop` tuent le processus et suppriment le dossier, y compris
//! quand une assertion échoue ; les journaux du moteur sont alors recopiés sur la sortie
//! d'erreur pour le diagnostic.

use std::{
    collections::HashSet,
    io::{self, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicU32, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

use serde_json::Value;

/// Binaire du moteur compilé par Cargo pour ces tests.
pub const EXE: &str = env!("CARGO_BIN_EXE_apex-scada");

/// Délai maximal de démarrage (build debug, machines d'intégration continue chargées).
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
/// Délai maximal d'arrêt d'un processus (arrêt propre ou échec de démarrage).
pub const EXIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Délai d'attente d'une réponse réseau.
const IO_TIMEOUT: Duration = Duration::from_secs(15);

/// Comptes d'usine créés au premier démarrage (voir `apex-core`, `FACTORY_ACCOUNTS`).
pub const ADMIN: (&str, &str) = ("admin", "admin123");
pub const VISU: (&str, &str) = ("visu", "visu123");

// ============================================================================
// DOSSIER TEMPORAIRE
// ============================================================================

/// Dossier temporaire unique, supprimé à la fin du test.
pub struct TempDir {
    pub path: PathBuf,
}

impl TempDir {
    pub fn new(name: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let path = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("apex-it")
            .join(format!("{name}-{}-{n}-{nanos}", std::process::id()));
        // Dossier neuf : un reliquat d'une exécution interrompue est d'abord supprimé
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("création du dossier temporaire");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Sous Windows, les fichiers peuvent rester ouverts quelques instants après la fin
        // du processus : quelques tentatives espacées.
        for _ in 0..20 {
            if std::fs::remove_dir_all(&self.path).is_ok() || !self.path.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

// ============================================================================
// PORTS LIBRES
// ============================================================================

/// Ports déjà attribués dans ce processus de test (les tests s'exécutent en parallèle).
static ALLOCATED_PORTS: Mutex<Option<HashSet<u16>>> = Mutex::new(None);

/// Port TCP libre sur la boucle locale, jamais attribué deux fois au cours de l'exécution,
/// et différent des ports par défaut du produit (3000 et 5502).
pub fn free_port() -> u16 {
    let mut guard = ALLOCATED_PORTS.lock().unwrap_or_else(|e| e.into_inner());
    let used = guard.get_or_insert_with(HashSet::new);
    loop {
        let port = TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("attribution d'un port libre")
            .port();
        if port != 3000 && port != 5502 && used.insert(port) {
            return port;
        }
    }
}

// ============================================================================
// CONFIGURATION
// ============================================================================

/// Paramètres d'une instance de test.
#[derive(Debug, Clone)]
pub struct Settings {
    pub port: u16,
    pub modbus_slave_port: u16,
    pub safe_mode: bool,
    pub simulation: bool,
}

impl Settings {
    /// Valeurs des tests : ports libres, simulation et mode sûr actifs.
    pub fn new() -> Self {
        Self {
            port: free_port(),
            modbus_slave_port: free_port(),
            safe_mode: true,
            simulation: true,
        }
    }

    /// Fichier de configuration complet (format v1).
    pub fn render(&self) -> String {
        let web_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("static");
        format!(
            "config_version = 1\n\
             \n\
             [instance]\n\
             name = \"Essai d'intégration\"\n\
             \n\
             [server]\n\
             bind = \"127.0.0.1\"\n\
             port = {port}\n\
             \n\
             [paths]\n\
             web_dir = '{web}'\n\
             \n\
             [logging]\n\
             level = \"info\"\n\
             file_enabled = true\n\
             retention_files = 2\n\
             \n\
             [safety]\n\
             safe_mode = {safe}\n\
             \n\
             [simulation]\n\
             enabled = {sim}\n\
             modbus_slave_port = {slave}\n",
            port = self.port,
            web = web_dir.display(),
            safe = self.safe_mode,
            sim = self.simulation,
            slave = self.modbus_slave_port,
        )
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self::new()
    }
}

/// Écrit le fichier de configuration dans `dir` et renvoie son chemin.
pub fn write_config(dir: &Path, content: &str) -> PathBuf {
    let file = dir.join("apex-scada.toml");
    std::fs::write(&file, content).expect("écriture du fichier de configuration");
    file
}

// ============================================================================
// PROCESSUS
// ============================================================================

/// Lance `apex-scada run --config <fichier>` dans `dir`, sorties redirigées vers des fichiers
/// (`<prefix>.stdout.log`, `<prefix>.stderr.log`) pour ne jamais bloquer sur un tube plein.
pub fn spawn_engine(dir: &Path, config: &Path, prefix: &str) -> Child {
    let stdout = std::fs::File::create(dir.join(format!("{prefix}.stdout.log")))
        .expect("fichier de sortie standard");
    let stderr = std::fs::File::create(dir.join(format!("{prefix}.stderr.log")))
        .expect("fichier de sortie d'erreur");
    let mut cmd = Command::new(EXE);
    cmd.arg("run")
        .arg("--config")
        .arg(config)
        // Dossier courant = dossier de test : aucune base d'une autre installation n'est vue
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    for (key, _) in std::env::vars_os() {
        let k = key.to_string_lossy().to_uppercase();
        if k.starts_with("SCADA_")
            || k.starts_with("APEX_")
            || k == "RUST_LOG"
            || k == "INVOCATION_ID"
        {
            cmd.env_remove(&key);
        }
    }
    cmd.spawn().expect("lancement du binaire apex-scada")
}

/// Attend la fin d'un processus, au plus `timeout`.
pub fn wait_timeout(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => return None,
        }
    }
}

/// Lit un fichier texte (UTF-8 tolérant), vide s'il est absent.
pub fn read_text(path: &Path) -> String {
    std::fs::read(path)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

/// Concatène les journaux fichiers du moteur (`logs/*.log`).
pub fn read_log_dir(dir: &Path) -> String {
    let mut out = String::new();
    if let Ok(entries) = std::fs::read_dir(dir.join("logs")) {
        let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        files.sort();
        for f in files {
            out.push_str(&read_text(&f));
        }
    }
    out
}

/// Résultat d'une exécution menée jusqu'à son terme.
pub struct Outcome {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Exécute le moteur jusqu'à sa fin (cas d'échec de démarrage attendu). Le processus est tué
/// s'il ne s'arrête pas de lui-même : le test échoue alors sans laisser de processus orphelin.
pub fn run_to_exit(dir: &Path, config: &Path, prefix: &str) -> Outcome {
    let mut child = ChildGuard(Some(spawn_engine(dir, config, prefix)));
    let status = wait_timeout(child.get(), EXIT_TIMEOUT);
    child.kill();
    let outcome = Outcome {
        code: status.and_then(|s| s.code()),
        stdout: read_text(&dir.join(format!("{prefix}.stdout.log"))),
        stderr: read_text(&dir.join(format!("{prefix}.stderr.log"))),
    };
    assert!(
        status.is_some(),
        "le moteur ne s'est pas arrêté en {EXIT_TIMEOUT:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        outcome.stdout,
        outcome.stderr
    );
    outcome
}

/// Garde : tue le processus s'il tourne encore quand la garde est libérée.
pub struct ChildGuard(pub Option<Child>);

impl ChildGuard {
    pub fn get(&mut self) -> &mut Child {
        self.0.as_mut().expect("processus présent")
    }

    pub fn kill(&mut self) {
        if let Some(mut c) = self.0.take() {
            if let Ok(None) = c.try_wait() {
                let _ = c.kill();
            }
            let _ = c.wait();
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

// ============================================================================
// INSTANCE EN MARCHE
// ============================================================================

/// Moteur démarré et prêt (`/api/health` répond). Champs libérés dans l'ordre de déclaration :
/// le processus est tué **avant** la suppression du dossier.
pub struct Server {
    child: ChildGuard,
    pub settings: Settings,
    pub config: PathBuf,
    dir: TempDir,
}

impl Server {
    /// Démarre une instance avec les paramètres des tests (simulation + mode sûr).
    pub fn start(name: &str) -> Self {
        Self::start_with(name, Settings::new())
    }

    pub fn start_with(name: &str, settings: Settings) -> Self {
        let dir = TempDir::new(name);
        let config = write_config(&dir.path, &settings.render());
        let child = ChildGuard(Some(spawn_engine(&dir.path, &config, "engine")));
        let mut server = Server {
            child,
            settings,
            config,
            dir,
        };
        server.wait_ready();
        server
    }

    pub fn dir(&self) -> &Path {
        &self.dir.path
    }

    pub fn port(&self) -> u16 {
        self.settings.port
    }

    pub fn pid(&mut self) -> u32 {
        self.child.get().id()
    }

    /// Attend que `/api/health` réponde ; échoue si le processus s'arrête entre-temps.
    fn wait_ready(&mut self) {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            if let Ok(Some(status)) = self.child.get().try_wait() {
                panic!(
                    "le moteur s'est arrêté au démarrage ({status})\n{}",
                    self.logs()
                );
            }
            if let Ok(r) = request(self.port(), "GET", "/api/health", None, None) {
                if r.status == 200 {
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "le moteur n'a pas répondu en {STARTUP_TIMEOUT:?}\n{}",
                self.logs()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Sorties du processus principal et journaux fichiers (diagnostic).
    pub fn logs(&self) -> String {
        format!(
            "--- stdout ---\n{}\n--- stderr ---\n{}",
            read_text(&self.dir.path.join("engine.stdout.log")),
            read_text(&self.dir.path.join("engine.stderr.log")),
        )
    }

    /// Jeton de contrôle local publié par le moteur (`run/control.token`).
    pub fn control_token(&self) -> String {
        let file = self.dir.path.join("run").join("control.token");
        let token = read_text(&file).trim().to_string();
        assert!(
            !token.is_empty(),
            "jeton de contrôle absent : {}",
            file.display()
        );
        token
    }

    /// Requête HTTP vers cette instance (panique en cas d'erreur réseau).
    pub fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> Response {
        request(self.port(), method, path, token, body).unwrap_or_else(|e| {
            panic!("{method} {path} : erreur réseau {e}\n{}", self.logs());
        })
    }

    /// Ouvre une session et renvoie le jeton (échoue si la connexion est refusée).
    pub fn login(&self, (username, password): (&str, &str)) -> String {
        let r = self.call(
            "POST",
            "/api/auth/login",
            None,
            Some(&serde_json::json!({ "username": username, "password": password })),
        );
        assert_eq!(r.status, 200, "connexion de {username} : {}", r.text());
        r.json()["token"]
            .as_str()
            .expect("champ token de la réponse de connexion")
            .to_string()
    }

    /// Attend la fin du processus (arrêt demandé) et renvoie son statut.
    pub fn wait_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        wait_timeout(self.child.get(), timeout)
    }

    /// Requête HTTP avec des en-têtes supplémentaires (simulation d'un proxy inverse...).
    pub fn call_with_headers(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        headers: &[(&str, &str)],
    ) -> Response {
        request_with_headers(self.port(), method, path, token, None, headers).unwrap_or_else(|e| {
            panic!("{method} {path} : erreur réseau {e}\n{}", self.logs());
        })
    }

    /// Relance le moteur dans le même dossier, une fois le précédent arrêté (la base est
    /// conservée).
    pub fn restart(&mut self) {
        self.child.kill();
        self.child = ChildGuard(Some(spawn_engine(&self.dir.path, &self.config, "engine")));
        self.wait_ready();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.child.kill();
        if std::thread::panicking() {
            eprintln!(
                "=== journaux du moteur ({}) ===\n{}\n--- logs/ ---\n{}",
                self.dir.path.display(),
                self.logs(),
                read_log_dir(&self.dir.path)
            );
        }
    }
}

// ============================================================================
// CLIENT HTTP/1.1 MINIMAL
// ============================================================================

/// Réponse HTTP décodée.
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    /// En-têtes, noms en minuscules.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("réponse JSON attendue ({e}) : {}", self.text()))
    }
}

fn connect(port: u16) -> io::Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), IO_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    stream.set_nodelay(true)?;
    Ok(stream)
}

/// Envoie une requête (`Connection: close`) et lit la réponse complète.
/// Le jeton éventuel est transmis dans l'en-tête `X-Scada-Token`.
pub fn request(
    port: u16,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> io::Result<Response> {
    request_with_headers(port, method, path, token, body, &[])
}

/// Comme [`request`], avec des en-têtes supplémentaires.
pub fn request_with_headers(
    port: u16,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
    headers: &[(&str, &str)],
) -> io::Result<Response> {
    let mut stream = connect(port)?;
    let payload = body.map(|b| b.to_string()).unwrap_or_default();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nAccept: application/json\r\n"
    );
    if let Some(t) = token {
        head.push_str(&format!("X-Scada-Token: {t}\r\n"));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if body.is_some() {
        head.push_str("Content-Type: application/json\r\n");
    }
    if body.is_some() || method != "GET" {
        head.push_str(&format!("Content-Length: {}\r\n", payload.len()));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(payload.as_bytes())?;
    stream.flush()?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    parse_response(&raw)
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// Code d'état, en-têtes (noms en minuscules) et index du début du corps.
type Head = (u16, Vec<(String, String)>, usize);

/// Sépare la ligne d'état et les en-têtes du reste ; renvoie aussi l'index du corps.
fn parse_head(raw: &[u8]) -> io::Result<Head> {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| invalid("en-têtes HTTP incomplets"))?;
    let head = std::str::from_utf8(&raw[..end]).map_err(|_| invalid("en-têtes non UTF-8"))?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| invalid("ligne d'état HTTP invalide"))?;
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Ok((status, headers, end + 4))
}

fn parse_response(raw: &[u8]) -> io::Result<Response> {
    let (status, headers, start) = parse_head(raw)?;
    let mut response = Response {
        status,
        headers,
        body: Vec::new(),
    };
    let rest = &raw[start..];
    response.body = if response
        .header("transfer-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
    {
        decode_chunked(rest)?
    } else if let Some(len) = response
        .header("content-length")
        .and_then(|v| v.parse::<usize>().ok())
    {
        rest.get(..len)
            .ok_or_else(|| invalid("corps HTTP tronqué"))?
            .to_vec()
    } else {
        rest.to_vec()
    };
    Ok(response)
}

fn decode_chunked(mut data: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| invalid("bloc HTTP incomplet"))?;
        let size_txt = std::str::from_utf8(&data[..line_end]).map_err(|_| invalid("taille"))?;
        let size = usize::from_str_radix(size_txt.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| invalid("taille de bloc HTTP invalide"))?;
        data = &data[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        out.extend_from_slice(data.get(..size).ok_or_else(|| invalid("bloc tronqué"))?);
        data = data.get(size + 2..).unwrap_or(&[]);
    }
}

// ============================================================================
// CLIENT WEBSOCKET MINIMAL (RFC 6455, côté client)
// ============================================================================

/// Clé et réponse d'exemple de la RFC 6455 §1.3 : la vérification de l'acceptation de la
/// poignée de main ne nécessite donc pas d'implémenter SHA-1 dans les tests.
const WS_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
const WS_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

pub struct WsClient {
    stream: TcpStream,
    buf: Vec<u8>,
}

/// Ouvre une connexion WebSocket. `Err(Response)` si le serveur refuse la mise à niveau.
pub fn ws_connect(port: u16, path: &str) -> io::Result<Result<WsClient, Response>> {
    let mut stream = connect(port)?;
    let head = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: {WS_KEY}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    stream.write_all(head.as_bytes())?;
    stream.flush()?;

    // Lecture des en-têtes ; ce qui suit appartient déjà au corps ou aux trames
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let (status, headers, start) = loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(invalid("connexion fermée pendant la poignée de main"));
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break parse_head(&buf)?;
        }
    };
    let rest = buf.split_off(start);
    let response = Response {
        status,
        headers,
        body: Vec::new(),
    };
    if status != 101 {
        // Refus : on lit le corps annoncé puis on ferme
        let len = response
            .header("content-length")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = rest;
        while body.len() < len {
            let n = stream.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        let _ = stream.shutdown(Shutdown::Both);
        return Ok(Err(Response { body, ..response }));
    }
    if response.header("sec-websocket-accept") != Some(WS_ACCEPT) {
        return Err(invalid("en-tête Sec-WebSocket-Accept incorrect"));
    }
    Ok(Ok(WsClient { stream, buf: rest }))
}

impl WsClient {
    fn fill(&mut self, n: usize, deadline: Instant) -> io::Result<()> {
        let mut chunk = [0u8; 16384];
        while self.buf.len() < n {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "délai dépassé"));
            }
            self.stream.set_read_timeout(Some(left))?;
            let read = self.stream.read(&mut chunk)?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connexion WebSocket fermée",
                ));
            }
            self.buf.extend_from_slice(&chunk[..read]);
        }
        Ok(())
    }

    fn take(&mut self, n: usize) -> Vec<u8> {
        let rest = self.buf.split_off(n);
        std::mem::replace(&mut self.buf, rest)
    }

    /// Lit une trame : (FIN, opcode, charge utile). Le serveur n'applique pas de masque.
    fn read_frame(&mut self, deadline: Instant) -> io::Result<(bool, u8, Vec<u8>)> {
        self.fill(2, deadline)?;
        let h = self.take(2);
        let fin = h[0] & 0x80 != 0;
        let opcode = h[0] & 0x0f;
        let masked = h[1] & 0x80 != 0;
        let mut len = (h[1] & 0x7f) as usize;
        if len == 126 {
            self.fill(2, deadline)?;
            let b = self.take(2);
            len = u16::from_be_bytes([b[0], b[1]]) as usize;
        } else if len == 127 {
            self.fill(8, deadline)?;
            let b = self.take(8);
            len = u64::from_be_bytes(b.try_into().expect("8 octets")) as usize;
        }
        let mask = if masked {
            self.fill(4, deadline)?;
            Some(self.take(4))
        } else {
            None
        };
        self.fill(len, deadline)?;
        let mut payload = self.take(len);
        if let Some(m) = mask {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= m[i % 4];
            }
        }
        Ok((fin, opcode, payload))
    }

    /// Prochain message texte, décodé en JSON (les trames de contrôle sont ignorées).
    /// `Ok(None)` si le serveur ferme la connexion.
    pub fn next_json(&mut self, timeout: Duration) -> io::Result<Option<Value>> {
        let deadline = Instant::now() + timeout;
        let mut message = Vec::new();
        loop {
            let (fin, opcode, payload) = self.read_frame(deadline)?;
            match opcode {
                0x8 => return Ok(None),
                0x9 => self.send_frame(0xA, &payload)?, // ping → pong
                0x1 | 0x0 => {
                    message.extend_from_slice(&payload);
                    if fin {
                        return serde_json::from_slice(&message)
                            .map(Some)
                            .map_err(|e| invalid(&format!("message JSON invalide : {e}")));
                    }
                }
                _ => {}
            }
        }
    }

    /// Envoie une trame client (masquée, comme l'exige la RFC 6455).
    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mask = [0x37u8, 0xfa, 0x21, 0x3d];
        let mut frame = vec![0x80 | opcode];
        match payload.len() {
            n if n < 126 => frame.push(0x80 | n as u8),
            n if n <= u16::MAX as usize => {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.stream.write_all(&frame)
    }

    /// Fermeture propre (trame Close, code 1000).
    pub fn close(mut self) {
        let _ = self.send_frame(0x8, &1000u16.to_be_bytes());
        let _ = self.stream.shutdown(Shutdown::Write);
    }
}
