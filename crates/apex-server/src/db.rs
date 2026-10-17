//! Ouverture de la base SQLite et création du schéma.
//!
//! Contrairement aux versions précédentes, une base inaccessible fait **échouer le démarrage** :
//! le moteur ne bascule plus silencieusement sur une base en mémoire (toutes les données
//! auraient été perdues à l'arrêt). La base en mémoire reste disponible pour les tests.

use std::{path::Path, time::Duration};

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

/// Ouvre la base SQLite (fichier en mode WAL, ou base en mémoire si `path` est `None`)
/// et crée le schéma.
pub async fn open(path: Option<&Path>) -> Result<sqlx::SqlitePool, sqlx::Error> {
    let pool = match path {
        Some(p) => {
            let opts = SqliteConnectOptions::new()
                .filename(p)
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal)
                .synchronous(SqliteSynchronous::Normal)
                .busy_timeout(Duration::from_secs(5));
            SqlitePoolOptions::new()
                .max_connections(5)
                .connect_with(opts)
                .await?
        }
        None => {
            // Une base en mémoire n'existe que sur sa connexion : pool à connexion unique et permanente
            SqlitePoolOptions::new()
                .max_connections(1)
                .min_connections(1)
                .idle_timeout(None)
                .max_lifetime(None)
                .connect("sqlite::memory:")
                .await?
        }
    };

    for ddl in [
        "CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );",
        "CREATE TABLE IF NOT EXISTS tags_catalog (
            id TEXT PRIMARY KEY,
            tag_json TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );",
        "CREATE TABLE IF NOT EXISTS drivers_config (
            id TEXT PRIMARY KEY,
            config_json TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );",
        "CREATE TABLE IF NOT EXISTS dashboards (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            layout_json TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );",
        "CREATE TABLE IF NOT EXISTS flows (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            flow_json TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );",
        "CREATE TABLE IF NOT EXISTS schedules (
            id TEXT PRIMARY KEY,
            rule_json TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );",
        "CREATE TABLE IF NOT EXISTS tag_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tag_id TEXT NOT NULL,
            value_json TEXT NOT NULL,
            quality TEXT NOT NULL,
            recorded_at TEXT NOT NULL
        );",
        "CREATE INDEX IF NOT EXISTS idx_history_tag ON tag_history(tag_id, id DESC);",
        "CREATE INDEX IF NOT EXISTS idx_history_time ON tag_history(recorded_at);",
        "CREATE TABLE IF NOT EXISTS alarms_journal (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tag_id TEXT NOT NULL,
            severity TEXT NOT NULL,
            event_type TEXT NOT NULL,
            message TEXT NOT NULL,
            operator TEXT,
            recorded_at TEXT NOT NULL
        );",
        "CREATE TABLE IF NOT EXISTS audit_logs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            actor TEXT NOT NULL,
            category TEXT NOT NULL,
            action TEXT NOT NULL,
            target TEXT NOT NULL,
            details TEXT NOT NULL
        );",
        "CREATE TABLE IF NOT EXISTS users_accounts (
            username TEXT PRIMARY KEY,
            user_json TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );",
        // Sessions ouvertes, sous l'empreinte de leur jeton (voir `security::session_key`) :
        // elles survivent à un redémarrage du moteur jusqu'à leur expiration par inactivité.
        "CREATE TABLE IF NOT EXISTS user_sessions (
            token_hash TEXT PRIMARY KEY,
            username TEXT NOT NULL,
            created_at TEXT NOT NULL,
            last_seen TEXT NOT NULL
        );",
        "CREATE TABLE IF NOT EXISTS alarms_state (
            id TEXT PRIMARY KEY,
            alarm_json TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );",
        "CREATE TABLE IF NOT EXISTS symbol_libraries (
            id TEXT PRIMARY KEY,
            symbol_json TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );",
    ] {
        sqlx::query(ddl).execute(&pool).await?;
    }

    Ok(pool)
}
