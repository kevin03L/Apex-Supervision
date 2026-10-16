//! Sonde du moteur : client HTTP minimal (sans dépendance) vers `/api/health` et arrêt propre
//! par le jeton de contrôle local. Toutes les requêtes visent le poste local.

use std::{
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream},
    time::Duration,
};

use apex_config::{AppConfig, Layout};

/// Informations publiées par le moteur sur `/api/health`.
#[derive(Debug, Clone, Default)]
pub struct Health {
    pub version: String,
    pub uptime_seconds: u64,
    pub running_drivers: u64,
    pub total_drivers: Option<u64>,
    pub active_alarms: Option<u64>,
    pub acquisition_paused: bool,
    /// Détails d'exécution (publiés uniquement aux connexions locales).
    pub runtime: Option<Runtime>,
}

#[derive(Debug, Clone, Default)]
pub struct Runtime {
    pub config_fingerprint: String,
    pub config_file: String,
    pub run_mode: String,
    pub pid: u64,
    pub safe_mode: bool,
    pub simulation_enabled: bool,
    pub full_version: String,
    /// Surcharges `SCADA_*` appliquées par le moteur (compatibilité 1.x).
    pub env_overrides: Vec<String>,
}

/// Adresse à laquelle joindre le moteur depuis ce poste.
pub fn target(cfg: &AppConfig) -> Result<SocketAddr, String> {
    let ip: IpAddr = cfg
        .server
        .bind
        .trim()
        .parse()
        .map_err(|_| format!("adresse d'écoute invalide « {} »", cfg.server.bind))?;
    let ip = match ip {
        IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(v6) if v6.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        other => other,
    };
    Ok(SocketAddr::new(ip, cfg.server.port))
}

/// URL de l'IHM web pour le navigateur de ce poste.
pub fn web_url(cfg: &AppConfig) -> String {
    match target(cfg) {
        Ok(addr) if addr.ip().is_loopback() => format!("http://localhost:{}/", addr.port()),
        Ok(addr) => format!("http://{addr}/"),
        Err(_) => format!("http://localhost:{}/", cfg.server.port),
    }
}

/// Interroge `/api/health`. `Err` signifie « serveur web injoignable » (avec la cause).
pub fn health(cfg: &AppConfig) -> Result<Health, String> {
    let addr = target(cfg)?;
    let (status, body) = request(addr, "GET", "/api/health", None, Duration::from_secs(2))?;
    if status != 200 {
        return Err(format!("réponse HTTP {status} inattendue de {addr}"));
    }
    let v: serde_json::Value = serde_json::from_str(&body)
        .map_err(|_| format!("réponse illisible de {addr} : est-ce bien Apex Supervision ?"))?;
    let u = |k: &str| v.get(k).and_then(|x| x.as_u64());
    let runtime = v.get("runtime").filter(|r| r.is_object()).map(|r| {
        let s = |k: &str| {
            r.get(k)
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let b = |k: &str| r.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
        Runtime {
            config_fingerprint: s("config_fingerprint"),
            config_file: s("config_file"),
            run_mode: s("run_mode"),
            pid: r.get("pid").and_then(|x| x.as_u64()).unwrap_or(0),
            safe_mode: b("safe_mode"),
            simulation_enabled: b("simulation_enabled"),
            full_version: s("full_version"),
            env_overrides: r
                .get("env_overrides")
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        }
    });
    Ok(Health {
        version: v
            .get("version")
            .and_then(|x| x.as_str())
            .unwrap_or("?")
            .to_string(),
        uptime_seconds: u("uptime_seconds").unwrap_or(0),
        running_drivers: u("running_drivers").unwrap_or(0),
        total_drivers: u("total_drivers"),
        active_alarms: u("active_alarms"),
        acquisition_paused: v
            .get("acquisition_paused")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
        runtime,
    })
}

/// Demande l'arrêt propre du moteur avec le jeton de contrôle local (accepté uniquement en
/// boucle locale par le moteur).
pub fn request_shutdown(layout: &Layout, cfg: &AppConfig) -> Result<(), String> {
    let token_file = layout.control_token_file();
    let token = std::fs::read_to_string(&token_file).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            format!(
                "jeton de contrôle « {} » réservé aux administrateurs : relancez l'utilitaire en administrateur",
                token_file.display()
            )
        } else {
            format!("jeton de contrôle « {} » introuvable : le moteur est-il démarré ?", token_file.display())
        }
    })?;
    let addr = target(cfg)?;
    if !addr.ip().is_loopback() {
        return Err(format!(
            "le moteur écoute sur {addr} et non en boucle locale : l'arrêt par jeton local est impossible"
        ));
    }
    let (status, body) = request(
        addr,
        "POST",
        "/api/system/shutdown",
        Some(token.trim()),
        Duration::from_secs(5),
    )?;
    match status {
        200 => Ok(()),
        401 | 403 => {
            Err("jeton de contrôle refusé : il ne correspond pas au moteur en marche".into())
        }
        _ => Err(format!("arrêt refusé (HTTP {status}) : {}", body.trim())),
    }
}

/// Requête HTTP/1.1 minimale (`Connection: close`), corps renvoyé en texte.
fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    timeout: Duration,
) -> Result<(u16, String), String> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout).map_err(|e| match e.kind() {
        std::io::ErrorKind::ConnectionRefused => format!("aucun serveur n'écoute sur {addr}"),
        // Sous Windows, un port local fermé n'est signalé qu'après plusieurs tentatives :
        // le délai expire avant le refus.
        std::io::ErrorKind::TimedOut if addr.ip().is_loopback() => {
            format!("aucun serveur n'écoute sur {addr}")
        }
        std::io::ErrorKind::TimedOut => format!("{addr} ne répond pas (délai dépassé)"),
        _ => format!("connexion à {addr} impossible : {e}"),
    })?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nUser-Agent: apex-scada-admin\r\nAccept: application/json\r\nConnection: close\r\nContent-Length: 0\r\n"
    );
    if let Some(t) = token {
        head.push_str(&format!("x-scada-token: {t}\r\n"));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .map_err(|e| format!("envoi vers {addr} impossible : {e}"))?;

    let mut raw = Vec::new();
    stream
        .take(4 * 1024 * 1024)
        .read_to_end(&mut raw)
        .map_err(|e| format!("lecture de la réponse de {addr} impossible : {e}"))?;
    parse_response(&raw).ok_or_else(|| format!("réponse HTTP invalide de {addr}"))
}

fn parse_response(raw: &[u8]) -> Option<(u16, String)> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..split]).ok()?;
    let body = &raw[split + 4..];
    let status = head
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    let chunked = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    let body = if chunked {
        dechunk(body)?
    } else {
        body.to_vec()
    };
    Some((status, String::from_utf8_lossy(&body).into_owned()))
}

fn dechunk(mut data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = data.windows(2).position(|w| w == b"\r\n")?;
        let size_txt = std::str::from_utf8(&data[..line_end]).ok()?;
        let size = usize::from_str_radix(size_txt.split(';').next()?.trim(), 16).ok()?;
        data = &data[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(data.get(..size)?);
        data = data.get(size + 2..)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_chunked_responses() {
        let plain = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}";
        assert_eq!(parse_response(plain), Some((200, "{}".into())));
        let chunked = b"HTTP/1.1 401 Unauthorized\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nabcd\r\n2\r\nef\r\n0\r\n\r\n";
        assert_eq!(parse_response(chunked), Some((401, "abcdef".into())));
        assert_eq!(parse_response(b"garbage"), None);
    }

    #[test]
    fn unspecified_bind_is_probed_on_loopback() {
        let mut cfg = AppConfig::default();
        cfg.server.bind = "0.0.0.0".into();
        cfg.server.port = 3100;
        assert_eq!(target(&cfg).unwrap(), "127.0.0.1:3100".parse().unwrap());
        assert_eq!(web_url(&cfg), "http://localhost:3100/");
        cfg.server.bind = "192.168.1.20".into();
        assert_eq!(web_url(&cfg), "http://192.168.1.20:3100/");
    }

    #[test]
    fn unreachable_server_gives_readable_error() {
        let mut cfg = AppConfig::default();
        // Port réservé puis libéré : personne n'y écoute
        let free = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        cfg.server.port = free;
        let err = health(&cfg).unwrap_err();
        assert!(
            err.contains("aucun serveur") || err.contains("impossible"),
            "{err}"
        );
    }
}
