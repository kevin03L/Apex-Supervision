//! # Module `config` — Paramètres d'exploitation de RustScadaFlow
//!
//! Tous les paramètres sont lus depuis les variables d'environnement au démarrage,
//! avec des valeurs par défaut adaptées à un poste de supervision local :
//!
//! | Variable                        | Défaut               | Rôle                                            |
//! |---------------------------------|----------------------|-------------------------------------------------|
//! | `SCADA_BIND`                    | `0.0.0.0`            | Adresse d'écoute du serveur HTTP                |
//! | `SCADA_PORT`                    | `3000`               | Port HTTP de l'IHM et de l'API                  |
//! | `SCADA_DB`                      | `rust_scada_flow.db` | Chemin de la base SQLite                        |
//! | `SCADA_MODBUS_SLAVE_PORT`       | `5502`               | Port de l'automate Modbus TCP embarqué          |
//! | `SCADA_HISTORY_RETENTION_DAYS`  | `30`                 | Rétention de l'historique (0 = illimitée)       |
//! | `SCADA_SESSION_TTL_HOURS`       | `12`                 | Expiration d'une session inactive               |
//! | `SCADA_STATIC_DIR`              | auto                 | Dossier de l'IHM (`static/` du dossier courant ou de l'exécutable) |

use std::{net::IpAddr, path::PathBuf, str::FromStr};

use tracing::warn;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub bind: IpAddr,
    pub port: u16,
    pub db_path: String,
    pub modbus_slave_port: u16,
    pub history_retention_days: u32,
    pub session_ttl_hours: u64,
    pub static_dir: PathBuf,
}

impl AppConfig {
    pub fn from_env() -> Self {
        Self {
            bind: env_or("SCADA_BIND", IpAddr::from([0, 0, 0, 0])),
            port: env_or("SCADA_PORT", 3000),
            db_path: std::env::var("SCADA_DB")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "rust_scada_flow.db".to_string()),
            modbus_slave_port: env_or("SCADA_MODBUS_SLAVE_PORT", 5502),
            history_retention_days: env_or("SCADA_HISTORY_RETENTION_DAYS", 30),
            session_ttl_hours: env_or::<u64>("SCADA_SESSION_TTL_HOURS", 12).max(1),
            static_dir: resolve_static_dir(),
        }
    }
}

fn env_or<T: FromStr>(key: &str, default: T) -> T {
    match std::env::var(key) {
        Ok(raw) if !raw.trim().is_empty() => match raw.trim().parse::<T>() {
            Ok(v) => v,
            Err(_) => {
                warn!("Variable {} invalide ('{}') : valeur par défaut utilisée", key, raw);
                default
            }
        },
        _ => default,
    }
}

/// Dossier de l'IHM : `SCADA_STATIC_DIR`, sinon `static/` du dossier courant, sinon le premier
/// `static/` trouvé en remontant depuis l'exécutable (ex. `target/release/../../static`).
fn resolve_static_dir() -> PathBuf {
    if let Some(dir) = std::env::var("SCADA_STATIC_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        return PathBuf::from(dir);
    }
    let local = PathBuf::from("static");
    if local.join("index.html").is_file() {
        return local;
    }
    if let Ok(exe) = std::env::current_exe() {
        for dir in exe.ancestors().skip(1).take(4) {
            let candidate = dir.join("static");
            if candidate.join("index.html").is_file() {
                return candidate;
            }
        }
    }
    local
}
