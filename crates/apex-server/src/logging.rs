//! Journalisation : sortie standard (journald sous systemd) et fichiers journaliers avec rotation.

use std::{io::IsTerminal, path::Path};

use apex_config::AppConfig;
use tracing_appender::{non_blocking::WorkerGuard, rolling};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

/// Préfixe des fichiers journaux : `apex-scada.AAAA-MM-JJ.log`.
pub const LOG_FILE_PREFIX: &str = "apex-scada";

/// Fichier où sont consignées les erreurs survenues avant l'initialisation des journaux
/// (configuration illisible...), pour que l'utilitaire desktop puisse les afficher.
pub const STARTUP_ERROR_FILE: &str = "apex-scada-startup.log";

/// Initialise la journalisation. `RUST_LOG` prime sur le niveau configuré.
/// Le garde renvoyé doit rester vivant jusqu'à l'arrêt (vidage des fichiers).
pub fn init(cfg: &AppConfig, log_dir: &Path) -> Result<Option<WorkerGuard>, String> {
    let level = cfg.logging.level.trim();
    // `apex_scada` : nom du binaire du serveur (crate apex-server)
    let directive = format!("apex_scada={level},apex_core={level},apex_config={level},warn");
    let filter =
        || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&directive));

    let stdout_layer = fmt::layer()
        .with_ansi(std::io::stdout().is_terminal())
        .with_filter(filter());

    if !cfg.logging.file_enabled {
        tracing_subscriber::registry()
            .with(stdout_layer)
            .try_init()
            .map_err(|e| e.to_string())?;
        return Ok(None);
    }

    std::fs::create_dir_all(log_dir).map_err(|e| {
        format!(
            "création du dossier de journaux « {} » impossible : {e}",
            log_dir.display()
        )
    })?;
    let appender = rolling::Builder::new()
        .rotation(rolling::Rotation::DAILY)
        .filename_prefix(LOG_FILE_PREFIX)
        .filename_suffix("log")
        .max_log_files(cfg.logging.retention_files as usize)
        .build(log_dir)
        .map_err(|e| {
            format!(
                "ouverture des journaux dans « {} » impossible : {e}",
                log_dir.display()
            )
        })?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_writer(writer)
        .with_filter(filter());

    tracing_subscriber::registry()
        .with(stdout_layer)
        .with(file_layer)
        .try_init()
        .map_err(|e| e.to_string())?;
    Ok(Some(guard))
}

/// Consigne une erreur de démarrage hors du système de journaux. L'affichage en console est
/// laissé à l'appelant (`main`), qui reçoit la même erreur.
pub fn startup_error(log_dir: &Path, message: &str) {
    let line = format!(
        "{} ERREUR {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        message.replace('\n', " | ")
    );
    if std::fs::create_dir_all(log_dir).is_ok() {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_dir.join(STARTUP_ERROR_FILE))
        {
            let _ = f.write_all(line.as_bytes());
        }
    }
}
