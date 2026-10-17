//! Cycle de vie du moteur : démarrage contrôlé, exécution, arrêt propre.
//!
//! Ordre de démarrage (tout échec arrête le processus **avant** toute communication industrielle) :
//! 1. configuration : lecture, migration en mémoire, surcharges `SCADA_*`, validation ;
//! 2. journaux (sortie standard + fichiers) ;
//! 3. verrou d'instance unique ;
//! 4. réservation du port web (conflit détecté immédiatement) ;
//! 5. base SQLite ;
//! 6. moteur : simulation éventuelle, état persisté, drivers, automatismes ;
//! 7. jeton de contrôle local, puis service HTTP.
//!
//! L'arrêt (API, Ctrl+C, SIGTERM, gestionnaire de services Windows) suit le même chemin :
//! arrêt des drivers, vidage du dernier lot d'historique, suppression du jeton, libération du verrou.

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use apex_config::{has_errors, store, AppConfig, Layout, LayoutKind, Severity};
use apex_core::{
    drivers::{
        build_local_modbus_plc_config, modbus_slave::spawn_embedded_modbus_plc, GtbSimulatorDriver,
    },
    EngineOptions, ScadaCore,
};
use tokio::sync::watch;
use tracing::{error, info, warn};

use crate::{
    cli::exit,
    http::{self, AppState, RuntimeInfo},
    instance::{InstanceLock, LockError},
    legacy, logging,
};

/// Mode d'exécution (affiché dans l'état et l'utilitaire desktop).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    Manual,
    Service,
}

impl RunMode {
    fn as_str(self) -> &'static str {
        match self {
            RunMode::Manual => "manual",
            RunMode::Service => "service",
        }
    }
}

/// Échec de démarrage, avec le code de sortie correspondant.
#[derive(Debug)]
pub struct StartupError {
    pub code: i32,
    pub message: String,
}

impl StartupError {
    fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Configuration effective : fichier (ou valeurs par défaut) + surcharges d'environnement.
pub struct Effective {
    pub layout: Layout,
    pub config: AppConfig,
    pub file_exists: bool,
    pub migrated_from: Option<u32>,
    pub env_overrides: Vec<String>,
    /// Empreinte de la configuration du fichier, avant les surcharges d'environnement :
    /// c'est elle que l'utilitaire compare au fichier pour détecter un redémarrage nécessaire.
    pub file_fingerprint: String,
}

impl Effective {
    /// Le logiciel est-il installé ? (mode portable, ou dossier de configuration système créé
    /// par l'installateur). Sinon, rien ne doit être créé dans les emplacements système.
    pub fn installed(&self) -> bool {
        self.layout.kind == LayoutKind::Portable
            || self.layout.config_file.parent().is_some_and(|d| d.is_dir())
    }
}

/// Lit la configuration effective sans rien démarrer (utilisé aussi par `config check/show`).
pub fn load_effective(config_path: Option<&std::path::Path>) -> Result<Effective, StartupError> {
    let layout = Layout::resolve(config_path);
    let default = match layout.kind {
        LayoutKind::Portable => AppConfig::portable_default(),
        LayoutKind::System => AppConfig::default(),
    };
    let file_exists = layout.config_file.is_file();
    let loaded = store::load_or(&layout.config_file, default)
        .map_err(|e| StartupError::new(exit::CONFIG, e.to_string()))?;
    let mut config = loaded.config;
    let file_fingerprint = config.fingerprint();
    let env_overrides = legacy::apply_env_overrides(&mut config);
    let layout = layout.with_overrides(&config);
    Ok(Effective {
        layout,
        config,
        file_exists,
        migrated_from: loaded.migrated_from,
        env_overrides,
        file_fingerprint,
    })
}

/// Démarre le moteur et le serveur web, puis attend l'arrêt.
///
/// `on_ready` est appelé une fois le service HTTP opérationnel (signalement « démarré » au
/// gestionnaire de services Windows). `stop` permet un arrêt demandé de l'extérieur.
pub async fn run(
    config_path: Option<PathBuf>,
    mode: RunMode,
    mut stop: watch::Receiver<bool>,
    on_ready: impl FnOnce(),
) -> Result<(), StartupError> {
    // 1. Configuration
    let eff = match load_effective(config_path.as_deref()) {
        Ok(e) => e,
        Err(e) => {
            let layout = Layout::resolve(config_path.as_deref());
            if layout.kind == LayoutKind::Portable
                || layout.config_file.parent().is_some_and(|d| d.is_dir())
            {
                logging::startup_error(&layout.log_dir, &e.message);
            }
            return Err(e);
        }
    };
    if !eff.installed() {
        // Aucun dossier système créé par un compte quelconque : l'installateur s'en charge
        return Err(StartupError::new(
            exit::CONFIG,
            format!(
                "logiciel non installé : « {} » n'existe pas. Installez Apex Supervision, ou lancez \
                 une instance portable avec --config <fichier>.",
                eff.layout
                    .config_file
                    .parent()
                    .map(|d| d.display().to_string())
                    .unwrap_or_default()
            ),
        ));
    }
    let issues = eff.config.validate();
    if has_errors(&issues) {
        let detail = issues
            .iter()
            .filter(|i| i.severity == Severity::Error)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n  ");
        let message = format!(
            "configuration invalide ({}) :\n  {}",
            eff.layout.config_file.display(),
            detail
        );
        logging::startup_error(&eff.layout.log_dir, &message);
        return Err(StartupError::new(exit::CONFIG, message));
    }

    // 2. Journaux
    let _log_guard = logging::init(&eff.config, &eff.layout.log_dir).map_err(|e| {
        logging::startup_error(&eff.layout.log_dir, &e);
        StartupError::new(exit::FAILURE, e)
    })?;

    let cfg = &eff.config;
    let layout = &eff.layout;
    info!("================================================================");
    info!(
        "  {} {} — instance « {} »",
        apex_config::version::PRODUCT_NAME,
        apex_config::version::full_version(),
        cfg.instance.name
    );
    info!("================================================================");
    info!(
        "Configuration : {}{}",
        layout.config_file.display(),
        if eff.file_exists {
            ""
        } else {
            " (absente : valeurs par défaut)"
        }
    );
    if let Some(v) = eff.migrated_from {
        warn!(
            "Configuration au format v{v} migrée en mémoire vers v{} : exécutez « apex-scada config migrate » pour réécrire le fichier (valeurs conservées, sauvegarde préalable)",
            apex_config::CONFIG_VERSION
        );
    }
    for o in &eff.env_overrides {
        warn!("Surcharge par variable d'environnement (compatibilité) : {o}");
    }
    for i in issues.iter().filter(|i| i.severity == Severity::Warning) {
        warn!("Configuration : {i}");
    }
    info!(
        "Données : {} · Journaux : {}",
        layout.data_dir.display(),
        layout.log_dir.display()
    );

    // 3. Instance unique
    let _instance = InstanceLock::acquire(&layout.lock_file()).map_err(|e| {
        let code = match e {
            LockError::AlreadyRunning { .. } => exit::ALREADY_RUNNING,
            LockError::Io(_) => exit::FAILURE,
        };
        error!("{e}");
        StartupError::new(code, e.to_string())
    })?;

    // 4. Port web (réservé avant de démarrer les drivers)
    let ip: std::net::IpAddr = cfg.server.bind.trim().parse().map_err(|_| {
        StartupError::new(
            exit::CONFIG,
            format!("adresse d'écoute invalide « {} »", cfg.server.bind),
        )
    })?;
    let addr = SocketAddr::new(ip, cfg.server.port);
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
        let message = format!(
            "impossible d'écouter sur {addr} : {e} (port déjà utilisé par un autre programme, \
             ou adresse non attribuée à ce poste)"
        );
        error!("{message}");
        StartupError::new(exit::BIND, message)
    })?;

    // 5. Base de données
    let db_file = layout.database_file(cfg);
    let pool = crate::db::open(Some(&db_file)).await.map_err(|e| {
        let message = format!(
            "base de données « {} » inaccessible : {e}",
            db_file.display()
        );
        error!("{message}");
        StartupError::new(exit::DATABASE, message)
    })?;
    info!("Base SQLite : {} (mode WAL)", db_file.display());
    // Ancienne base à reprendre ? Seulement si la base actuelle ne contient encore rien : son
    // contenu est lu par SQLite (journal WAL compris), la taille du fichier ne dit rien.
    match legacy::is_blank_database(&pool).await {
        Ok(true) => hint_legacy_database(layout),
        Ok(false) => {}
        Err(e) => warn!("Lecture du contenu de la base impossible : {e}"),
    }

    // 6. Moteur
    if cfg.simulation.enabled {
        spawn_embedded_modbus_plc(cfg.simulation.modbus_slave_port).await;
        warn!("SIMULATION active : automate Modbus embarqué, simulateur GTB et données de démonstration disponibles");
    }
    if cfg.safety.safe_mode {
        warn!("MODE SÛR actif : aucune commande ne sera envoyée aux équipements réels");
    }
    let options = EngineOptions {
        instance_name: cfg.instance.name.trim().to_string(),
        session_ttl_hours: cfg.security.session_ttl_hours as u64,
        history_retention_days: cfg.history.retention_days,
        safe_mode: cfg.safety.safe_mode,
        simulation_enabled: cfg.simulation.enabled,
        modbus_slave_port: cfg.simulation.modbus_slave_port,
        data_dir: Some(eff.layout.data_dir.clone()),
    };
    // Le pool est partagé avec le moteur ; cette copie sert à le fermer à l'arrêt propre.
    let core = ScadaCore::new(Some(pool.clone()), &options);
    let default_drivers = if cfg.simulation.enabled {
        vec![
            GtbSimulatorDriver::new_default("SIM_GTB_01", Arc::clone(&core)).config(),
            build_local_modbus_plc_config(cfg.simulation.modbus_slave_port),
        ]
    } else {
        Vec::new()
    };
    if let Err(e) = core.load_persisted_state(default_drivers).await {
        error!("Chargement de l'état persisté impossible : {e}");
    }
    core.start_persisted_drivers().await;
    core.start_automation();

    // 7. Jeton de contrôle local, puis service HTTP
    let token_file = layout.control_token_file();
    if let Err(e) = write_control_token(&token_file, &core.local_control_token) {
        warn!(
            "Écriture du jeton de contrôle local « {} » impossible : {e}",
            token_file.display()
        );
    }

    let web_dir = layout.web_dir(cfg);
    if !web_dir.join("index.html").is_file() {
        warn!(
            "IHM web introuvable dans « {} » : renseignez paths.web_dir dans la configuration",
            web_dir.display()
        );
    }
    let runtime = RuntimeInfo {
        instance_name: cfg.instance.name.trim().to_string(),
        config_fingerprint: eff.file_fingerprint.clone(),
        env_overrides: eff.env_overrides.clone(),
        config_file: layout.config_file.display().to_string(),
        data_dir: layout.data_dir.display().to_string(),
        log_dir: layout.log_dir.display().to_string(),
        web_dir: web_dir.display().to_string(),
        run_mode: mode.as_str().to_string(),
        pid: std::process::id(),
        safe_mode: cfg.safety.safe_mode,
        simulation_enabled: cfg.simulation.enabled,
        full_version: apex_config::version::full_version(),
    };
    let state = AppState {
        core: Arc::clone(&core),
        runtime: Arc::new(runtime),
    };
    let app = http::router(state, &web_dir);

    core.record_audit(
        "Système",
        "SERVEUR",
        "STARTUP",
        &cfg.instance.name,
        &format!(
            "Démarrage ({}, mode {})",
            apex_config::version::VERSION,
            mode.as_str()
        ),
    );
    info!("IHM web : http://{}:{}", display_host(ip), cfg.server.port);
    if !ip.is_loopback() {
        warn!("Le serveur est accessible depuis le réseau ({addr}) en HTTP non chiffré");
    }

    // Arrêt demandé par l'API, par un signal du système ou par le gestionnaire de services :
    // quel que soit le déclencheur, toutes les tâches en attente (WebSocket...) sont prévenues,
    // sans quoi l'arrêt propre attendrait indéfiniment la fermeture des connexions.
    on_ready();
    let shutdown = Arc::clone(&core.shutdown_notify);
    let core_shutdown = Arc::clone(&core);
    let instance_name = cfg.instance.name.clone();
    let served = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        tokio::select! {
            _ = shutdown.notified() => info!("Arrêt demandé depuis l'IHM ou l'API"),
            _ = wait_os_signal() => {
                info!("Signal d'arrêt du système reçu");
                core_shutdown.record_audit(
                    "Système",
                    "SERVEUR",
                    "SHUTDOWN",
                    &instance_name,
                    "Signal d'arrêt du système reçu (SIGINT / SIGTERM)",
                );
            }
            _ = stop.wait_for(|s| *s) => {
                info!("Arrêt demandé par le gestionnaire de services");
                core_shutdown.record_audit(
                    "Système",
                    "SERVEUR",
                    "SHUTDOWN",
                    &instance_name,
                    "Arrêt demandé par le gestionnaire de services",
                );
            }
        }
        shutdown.notify_waiters();
        info!("Arrêt propre en cours : fermeture du serveur web...");
    })
    .await;

    // Arrêt propre
    core.shutdown();
    let _ = std::fs::remove_file(&token_file);
    // Dates d'activité des sessions à jour, pour leur reprise au prochain démarrage
    core.persist_sessions().await;
    // Ferme formellement les canaux des écrivains SQLite et attend la fin de leur vidage (S-37)
    core.drain_sqlite_writers().await;
    close_database(&pool).await;
    info!("{} arrêté proprement.", apex_config::version::PRODUCT_NAME);

    served.map_err(|e| StartupError::new(exit::FAILURE, format!("serveur web interrompu : {e}")))
}

fn display_host(ip: std::net::IpAddr) -> String {
    if ip.is_unspecified() || ip.is_loopback() {
        "localhost".to_string()
    } else {
        ip.to_string()
    }
}

/// Ferme la base à l'arrêt propre : le journal WAL est reporté dans le fichier principal, qui
/// se suffit alors à lui-même (copie, sauvegarde ou reprise sans les fichiers `-wal`/`-shm`).
async fn close_database(pool: &sqlx::SqlitePool) {
    // Le PRAGMA renvoie (busy, pages du journal, pages reportées) : busy = 1 signale un report
    // incomplet (une connexion lisait encore), le journal sera alors rejoué à l'ouverture.
    match sqlx::query_as::<_, (i64, i64, i64)>("PRAGMA wal_checkpoint(TRUNCATE)")
        .fetch_one(pool)
        .await
    {
        Ok((0, _, _)) => {}
        Ok((_, log, done)) => warn!(
            "Report du journal WAL incomplet à l'arrêt ({done}/{log} pages) : il sera repris au prochain démarrage"
        ),
        Err(e) => warn!("Report du journal WAL dans la base impossible : {e}"),
    }
    // Attend la restitution des connexions. Le journal est vide après le report ; selon la
    // plateforme, des fichiers -wal (0 octet) et -shm peuvent rester : la base se suffit à
    // elle-même.
    pool.close().await;
}

/// Signale la présence d'une base d'une version antérieure, sans jamais la migrer d'office.
/// Appelée seulement si la base actuelle ne contient encore aucune donnée : la reprise
/// (`--force`) remplacera donc une base vide, elle-même sauvegardée au préalable.
fn hint_legacy_database(layout: &Layout) {
    let mut candidates = vec![layout.data_dir.clone()];
    if let Some(dir) = layout.config_file.parent() {
        candidates.push(dir.to_path_buf());
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd);
    }
    if let Some(found) = candidates.iter().find_map(|d| legacy::find_legacy_db(d)) {
        warn!(
            "Base d'une version antérieure détectée ({}) alors que la base actuelle est encore vide : \
             pour reprendre ses données, arrêtez le moteur puis exécutez « {} » (la base vide créée \
             par ce démarrage est remplacée, après sauvegarde)",
            found.display(),
            legacy_command(&found, layout)
        );
    }
}

/// Commande de reprise conseillée. En mode portable, `--config` désigne l'instance : sans lui,
/// la commande viserait l'installation système.
fn legacy_command(found: &std::path::Path, layout: &Layout) -> String {
    let mut command = format!(
        "apex-scada migrate-legacy --from \"{}\" --force",
        found.display()
    );
    if layout.kind == LayoutKind::Portable {
        command.push_str(&format!(" --config \"{}\"", layout.config_file.display()));
    }
    command
}

/// Écrit le jeton de contrôle local avec des permissions restreintes.
fn write_control_token(path: &std::path::Path, token: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        // S-22: Création atomique sans fenêtre TOCTOU avec mode 0o640
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o640)
            .open(path)?;
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o640))?;
        file.write_all(token.as_bytes())?;
    }

    #[cfg(windows)]
    {
        // Recréer un fichier vide retire les ACL explicites d'un ancien jeton.
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        // S-22: Sous Windows (mode manuel/portable), restreindre les ACL à SYSTEM, Administrateurs et compte courant
        let path_str = path.display().to_string();
        let username = std::env::var("USERNAME").unwrap_or_default();
        let mut grant_args = vec![
            path_str,
            "/inheritance:r".to_string(),
            "/grant:r".to_string(),
            "*S-1-5-18:(F)".to_string(),     // NT AUTHORITY\SYSTEM
            "*S-1-5-32-544:(F)".to_string(), // Builtin\Administrators
        ];
        if !username.is_empty() {
            grant_args.push(format!("{}:(F)", username));
        }
        let result = std::process::Command::new("icacls")
            .args(&grant_args)
            .output()?;
        if !result.status.success() {
            return Err(std::io::Error::other(
                "Restriction des permissions du jeton impossible",
            ));
        }
        use std::io::Write;
        file.write_all(token.as_bytes())?;
    }
    Ok(())
}

/// Attend Ctrl+C (et SIGTERM sous Linux, fermeture de console sous Windows).
async fn wait_os_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_break, ctrl_close, ctrl_shutdown};
        match (ctrl_break(), ctrl_close(), ctrl_shutdown()) {
            (Ok(mut brk), Ok(mut close), Ok(mut shut)) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = brk.recv() => {}
                    _ = close.recv() => {}
                    _ = shut.recv() => {}
                }
            }
            _ => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reprise_conseillee_designe_l_instance_portable() {
        let base = std::env::temp_dir().join("apex-app-hint");
        let found = base.join("v1").join(legacy::LEGACY_DB_FILE);
        let portable = Layout::portable(&base);
        let command = legacy_command(&found, &portable);
        assert!(
            command.starts_with("apex-scada migrate-legacy --from"),
            "{command}"
        );
        assert!(
            command.ends_with(&format!(
                "--force --config \"{}\"",
                portable.config_file.display()
            )),
            "{command}"
        );

        let system = Layout::system();
        let command = legacy_command(&found, &system);
        assert!(!command.contains("--config"), "{command}");
    }

    #[test]
    fn test_s22_write_control_token() {
        let temp_dir = std::env::temp_dir().join(format!("apex-s22-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let token_path = temp_dir.join("control.token");
        let token_value = "secret-token-test-1234567890";
        write_control_token(&token_path, token_value).expect("écriture du jeton");
        let read = std::fs::read_to_string(&token_path).expect("lecture du jeton");
        assert_eq!(read, token_value);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&token_path).unwrap().permissions().mode() & 0o777,
                0o640
            );
        }
        #[cfg(windows)]
        {
            let result = std::process::Command::new("powershell")
                .args(["-NoProfile", "-Command", "$allowed = @('S-1-5-18', 'S-1-5-32-544', [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value); $acl = Get-Acl -LiteralPath $env:APEX_TOKEN_TEST_PATH; if (!$acl.AreAccessRulesProtected) { exit 1 }; foreach ($ace in $acl.Access) { if ($ace.IsInherited -or $allowed -notcontains $ace.IdentityReference.Translate([System.Security.Principal.SecurityIdentifier]).Value) { exit 2 } }; exit 0"])
                .env("APEX_TOKEN_TEST_PATH", &token_path).output().unwrap();
            assert!(
                result.status.success(),
                "ACL du jeton trop larges : {:?}",
                result
            );
        }
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
