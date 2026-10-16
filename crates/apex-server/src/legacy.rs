//! Compatibilité avec les versions antérieures (≤ 1.3, binaire `rust-scada-flow`) :
//!
//! - surcharges par variables d'environnement `SCADA_*` (journalisées comme telles) ;
//! - copie de l'ancienne base `rust_scada_flow.db` vers le répertoire de données
//!   (`apex-scada migrate-legacy`). La source n'est **jamais** modifiée ni supprimée.

use std::path::{Path, PathBuf};

use apex_config::{AppConfig, Layout};
use sqlx::{sqlite::SqliteConnectOptions, ConnectOptions, Row};

/// Nom de la base des versions antérieures.
pub const LEGACY_DB_FILE: &str = "rust_scada_flow.db";

/// Applique les anciennes variables d'environnement. Renvoie la liste des surcharges appliquées.
pub fn apply_env_overrides(cfg: &mut AppConfig) -> Vec<String> {
    let mut applied = Vec::new();
    let var = |k: &str| {
        std::env::var(k)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };

    if let Some(v) = var("SCADA_BIND") {
        cfg.server.bind = v.clone();
        applied.push(format!("SCADA_BIND={v}"));
    }
    if let Some(v) = var("SCADA_PORT") {
        match v.parse() {
            Ok(p) => {
                cfg.server.port = p;
                applied.push(format!("SCADA_PORT={v}"));
            }
            Err(_) => applied.push(format!("SCADA_PORT={v} ignorée (port invalide)")),
        }
    }
    if let Some(v) = var("SCADA_DB") {
        let path = std::path::absolute(&v).unwrap_or_else(|_| PathBuf::from(&v));
        cfg.database.file = path.to_string_lossy().to_string();
        applied.push(format!("SCADA_DB={v}"));
    }
    if let Some(v) = var("SCADA_MODBUS_SLAVE_PORT") {
        if let Ok(p) = v.parse() {
            cfg.simulation.modbus_slave_port = p;
            applied.push(format!("SCADA_MODBUS_SLAVE_PORT={v}"));
        }
    }
    if let Some(v) = var("SCADA_HISTORY_RETENTION_DAYS") {
        if let Ok(d) = v.parse() {
            cfg.history.retention_days = d;
            applied.push(format!("SCADA_HISTORY_RETENTION_DAYS={v}"));
        }
    }
    if let Some(v) = var("SCADA_SESSION_TTL_HOURS") {
        if let Ok(h) = v.parse() {
            cfg.security.session_ttl_hours = h;
            applied.push(format!("SCADA_SESSION_TTL_HOURS={v}"));
        }
    }
    if let Some(v) = var("SCADA_STATIC_DIR") {
        let path = std::path::absolute(&v).unwrap_or_else(|_| PathBuf::from(&v));
        cfg.paths.web_dir = path.to_string_lossy().to_string();
        applied.push(format!("SCADA_STATIC_DIR={v}"));
    }
    applied
}

/// Recherche une ancienne base dans un dossier (ou accepte directement un fichier `.db`).
pub fn find_legacy_db(from: &Path) -> Option<PathBuf> {
    if from.is_file() {
        return Some(from.to_path_buf());
    }
    let candidate = from.join(LEGACY_DB_FILE);
    candidate.is_file().then_some(candidate)
}

/// Rapport de migration.
#[derive(Debug)]
pub struct MigrationReport {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub previous_destination_backup: Option<PathBuf>,
    pub counts: Vec<(String, i64)>,
}

/// Copie cohérente de l'ancienne base (y compris son journal WAL) par `VACUUM INTO`.
///
/// Le moteur doit être arrêté (le verrou d'instance est vérifié par l'appelant).
/// Si la destination existe : refus, sauf `force` (elle est alors renommée en `.bak-<date>`).
pub async fn migrate_database(
    source: &Path,
    layout: &Layout,
    cfg: &AppConfig,
    force: bool,
) -> Result<MigrationReport, String> {
    let destination = layout.database_file(cfg);
    if std::fs::canonicalize(source).ok() == std::fs::canonicalize(&destination).ok()
        && destination.exists()
    {
        return Err("la source et la destination sont le même fichier".into());
    }

    let mut previous_destination_backup = None;
    if destination.exists() {
        if !force {
            return Err(format!(
                "une base existe déjà dans « {} » : relancez avec --force pour la remplacer \
                 (elle sera conservée sous un autre nom)",
                destination.display()
            ));
        }
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let backup = destination.with_extension(format!("db.bak-{stamp}"));
        std::fs::rename(&destination, &backup)
            .map_err(|e| format!("mise de côté de la base existante impossible : {e}"))?;
        for ext in ["db-wal", "db-shm"] {
            let _ = std::fs::remove_file(destination.with_extension(ext));
        }
        previous_destination_backup = Some(backup);
    }
    if let Some(dir) = destination.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("création de « {} » impossible : {e}", dir.display()))?;
    }

    let mut conn = SqliteConnectOptions::new()
        .filename(source)
        .read_only(true)
        .connect()
        .await
        .map_err(|e| format!("ouverture de « {} » impossible : {e}", source.display()))?;
    sqlx::query("VACUUM INTO ?")
        .bind(destination.to_string_lossy().to_string())
        .execute(&mut conn)
        .await
        .map_err(|e| format!("copie de la base impossible : {e}"))?;

    let mut counts = Vec::new();
    for table in [
        "tags_catalog",
        "drivers_config",
        "dashboards",
        "flows",
        "schedules",
        "users_accounts",
        "tag_history",
    ] {
        if let Ok(row) = sqlx::query(&format!("SELECT COUNT(*) AS n FROM {table}"))
            .fetch_one(&mut conn)
            .await
        {
            counts.push((table.to_string(), row.try_get::<i64, _>("n").unwrap_or(0)));
        }
    }

    Ok(MigrationReport {
        source: source.to_path_buf(),
        destination,
        previous_destination_backup,
        counts,
    })
}
