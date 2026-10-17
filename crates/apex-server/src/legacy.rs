//! Compatibilité avec les versions antérieures (≤ 1.3, binaire `rust-scada-flow`) :
//!
//! - surcharges par variables d'environnement `SCADA_*` (journalisées comme telles) ;
//! - copie de l'ancienne base `rust_scada_flow.db` vers le répertoire de données
//!   (`apex-scada migrate-legacy`). La source n'est **jamais** modifiée ni supprimée.

use std::path::{Path, PathBuf};

use apex_config::{AppConfig, Layout};
use sqlx::{sqlite::SqliteConnectOptions, ConnectOptions, Connection, Row};

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

/// Tables qui portent les données d'une installation (configuration, historique, comptes...).
const CONTENT_TABLES: [&str; 11] = [
    "tags_catalog",
    "drivers_config",
    "dashboards",
    "flows",
    "schedules",
    "tag_history",
    "alarms_journal",
    "audit_logs",
    "users_accounts",
    "alarms_state",
    "symbol_libraries",
];

/// La base ouverte ne contient-elle encore aucune donnée (base tout juste créée) ?
///
/// Le contenu est lu par SQLite, journal WAL compris : la taille du fichier principal ne dit
/// rien d'une base dont les données n'ont pas encore été reportées depuis le journal.
pub async fn is_blank_database(pool: &sqlx::SqlitePool) -> Result<bool, sqlx::Error> {
    for table in CONTENT_TABLES {
        let found: Option<i64> = sqlx::query_scalar(&format!("SELECT 1 FROM {table} LIMIT 1"))
            .fetch_optional(pool)
            .await?;
        if found.is_some() {
            return Ok(false);
        }
    }
    Ok(true)
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
/// Si la destination existe : refus, sauf `force`. Elle est alors sauvegardée **en entier**
/// (journal WAL compris) sous `<base>.bak-<date>` par `VACUUM INTO` depuis la base ouverte,
/// avant d'être remplacée. Ordre des opérations, pour ne rien perdre en cas d'échec :
/// 1. copie de la source dans un fichier provisoire voisin de la destination ;
/// 2. sauvegarde complète de la base existante, vérifiée (`quick_check`) ;
/// 3. remplacement de la destination par la copie provisoire.
///
/// Les fichiers `-wal` et `-shm` de la destination ne sont retirés qu'une fois leur contenu
/// intégré à la sauvegarde ; orphelins (sans base principale), ils sont mis de côté, jamais
/// supprimés.
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

    let destination_exists = destination.exists();
    if destination_exists && !force {
        return Err(format!(
            "une base existe déjà dans « {} » : relancez avec --force pour la remplacer \
             (elle sera d'abord sauvegardée en entier sous un autre nom)",
            destination.display()
        ));
    }
    if let Some(dir) = destination.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("création de « {} » impossible : {e}", dir.display()))?;
    }

    // 1. Copie de la source dans un fichier provisoire : la destination n'est pas touchée
    //    tant que la copie n'a pas réussi.
    let staging = sidecar(&destination, ".migration");
    remove_staging(&staging);
    let mut conn = SqliteConnectOptions::new()
        .filename(source)
        .read_only(true)
        .connect()
        .await
        .map_err(|e| format!("ouverture de « {} » impossible : {e}", source.display()))?;
    if let Err(e) = sqlx::query("VACUUM INTO ?")
        .bind(staging.to_string_lossy().to_string())
        .execute(&mut conn)
        .await
    {
        let _ = conn.close().await;
        remove_staging(&staging);
        return Err(format!("copie de la base impossible : {e}"));
    }

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
    let _ = conn.close().await;

    // 2. Sauvegarde complète de la base existante (elle reste en place si la sauvegarde échoue)
    let previous_destination_backup = if destination_exists {
        match backup_database(&destination).await {
            Ok(backup) => {
                align_owner_with_data_dir(&backup);
                Some(backup)
            }
            Err(e) => {
                remove_staging(&staging);
                return Err(format!(
                    "{e} ; la base existante reste en place, rien n'a été remplacé (si elle est \
                     illisible, déplacez-la manuellement avec ses fichiers -wal et -shm)"
                ));
            }
        }
    } else {
        None
    };
    let kept = previous_destination_backup
        .as_ref()
        .map(|b| format!(" (base précédente sauvegardée en entier : {})", b.display()))
        .unwrap_or_default();

    // 3. Remplacement. Un journal laissé à côté de la nouvelle base serait rejoué sur elle et
    //    la corromprait : celui de la base existante est intégré à la sauvegarde (étape 2) et
    //    normalement déjà supprimé par SQLite à la fermeture ; un journal orphelin (sans base
    //    principale) est mis de côté.
    for suffix in ["-wal", "-shm"] {
        let file = sidecar(&destination, suffix);
        if !file.exists() {
            continue;
        }
        let outcome = if destination_exists {
            std::fs::remove_file(&file)
        } else {
            std::fs::rename(&file, free_backup_path(&file))
        };
        if let Err(e) = outcome {
            remove_staging(&staging);
            return Err(format!(
                "« {} » ne peut pas être retiré : {e}{kept}",
                file.display()
            ));
        }
    }
    if let Err(e) = std::fs::rename(&staging, &destination) {
        remove_staging(&staging);
        return Err(format!(
            "remplacement de « {} » impossible : {e}{kept}",
            destination.display()
        ));
    }
    align_owner_with_data_dir(&destination);

    Ok(MigrationReport {
        source: source.to_path_buf(),
        destination,
        previous_destination_backup,
        counts,
    })
}

/// Sauvegarde complète d'une base SQLite (pages du journal WAL comprises) par `VACUUM INTO`,
/// depuis la base ouverte en lecture-écriture (récupération du journal possible), puis
/// vérification de la sauvegarde. À la fermeture, SQLite reporte le journal dans la base
/// principale et le supprime.
///
/// Un fichier qui n'est pas une base SQLite n'est pas ouvert : SQLite supprimerait à la
/// fermeture le journal posé à côté, alors que rien n'aurait pu en être sauvegardé.
async fn backup_database(database: &Path) -> Result<PathBuf, String> {
    if !has_sqlite_header(database) {
        return Err(format!(
            "« {} » n'est pas une base SQLite lisible : sauvegarde impossible",
            database.display()
        ));
    }
    let backup = free_backup_path(database);
    let mut conn = SqliteConnectOptions::new()
        .filename(database)
        .create_if_missing(false)
        .connect()
        .await
        .map_err(|e| {
            format!(
                "ouverture de la base existante « {} » impossible : {e}",
                database.display()
            )
        })?;
    let copied = sqlx::query("VACUUM INTO ?")
        .bind(backup.to_string_lossy().to_string())
        .execute(&mut conn)
        .await;
    if copied.is_ok() {
        // Reporte le journal dans la base principale avant la fermeture (sans effet hors WAL)
        let _ = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&mut conn)
            .await;
    }
    let _ = conn.close().await;
    if let Err(e) = copied {
        let _ = std::fs::remove_file(&backup);
        return Err(format!(
            "sauvegarde de la base existante « {} » impossible : {e}",
            database.display()
        ));
    }

    // Vérification : la sauvegarde s'ouvre et est intègre
    let verdict = async {
        let mut conn = SqliteConnectOptions::new()
            .filename(&backup)
            .read_only(true)
            .connect()
            .await?;
        let verdict: String = sqlx::query_scalar("PRAGMA quick_check")
            .fetch_one(&mut conn)
            .await?;
        let _ = conn.close().await;
        Ok::<_, sqlx::Error>(verdict)
    }
    .await;
    match verdict {
        Ok(v) if v == "ok" => Ok(backup),
        Ok(v) => Err(format!(
            "la sauvegarde « {} » est incomplète ({v})",
            backup.display()
        )),
        Err(e) => Err(format!(
            "la sauvegarde « {} » est illisible : {e}",
            backup.display()
        )),
    }
}

/// Le fichier commence-t-il par l'en-tête des bases SQLite ?
fn has_sqlite_header(file: &Path) -> bool {
    use std::io::Read;
    let mut header = [0u8; 16];
    std::fs::File::open(file)
        .and_then(|mut f| f.read_exact(&mut header))
        .is_ok()
        && &header == b"SQLite format 3\0"
}

/// Fichier annexe de SQLite : nom complet de la base suivi du suffixe (`-wal`, `-shm`...).
fn sidecar(database: &Path, suffix: &str) -> PathBuf {
    let mut name = database.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Nom de sauvegarde libre : `<fichier>.bak-<date>`, complété d'un numéro s'il est déjà pris.
fn free_backup_path(file: &Path) -> PathBuf {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let base = sidecar(file, &format!(".bak-{stamp}"));
    if !base.exists() {
        return base;
    }
    (2u32..)
        .map(|n| sidecar(&base, &format!("-{n}")))
        .find(|p| !p.exists())
        .unwrap_or(base)
}

/// Supprime la copie provisoire d'une migration (et son éventuel journal de transaction).
fn remove_staging(staging: &Path) {
    for file in [
        staging.to_path_buf(),
        sidecar(staging, "-journal"),
        sidecar(staging, "-wal"),
        sidecar(staging, "-shm"),
    ] {
        let _ = std::fs::remove_file(file);
    }
}

/// Linux : une reprise lancée en root (sudo) produirait une base appartenant à root, que le
/// service (utilisateur apex-scada) ne pourrait plus écrire. La copie prend donc le
/// propriétaire du répertoire des données.
#[cfg(unix)]
fn align_owner_with_data_dir(destination: &Path) {
    use std::os::unix::fs::MetadataExt;
    let Some(dir_meta) = destination.parent().and_then(|d| std::fs::metadata(d).ok()) else {
        return;
    };
    for file in [
        destination.to_path_buf(),
        sidecar(destination, "-wal"),
        sidecar(destination, "-shm"),
    ] {
        if file.exists() {
            let _ = std::os::unix::fs::chown(&file, Some(dir_meta.uid()), Some(dir_meta.gid()));
        }
    }
}

#[cfg(not(unix))]
fn align_owner_with_data_dir(_destination: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteJournalMode, SqlitePoolOptions};

    /// Dossier temporaire neuf et unique, supprimé à la fin du test.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir()
                .join(format!("apex-legacy-{name}-{}-{nanos}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("création du dossier temporaire");
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Base en mode WAL, report automatique du journal désactivé, avec un compte `marker`.
    /// Le pool reste ouvert : tant qu'il l'est, les données ne sont que dans le journal.
    async fn wal_database(file: &Path, marker: &str) -> sqlx::SqlitePool {
        let opts = SqliteConnectOptions::new()
            .filename(file)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .pragma("wal_autocheckpoint", "0");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .expect("ouverture de la base d'essai");
        sqlx::query(
            "CREATE TABLE users_accounts (username TEXT PRIMARY KEY, user_json TEXT NOT NULL, \
             updated_at TEXT NOT NULL)",
        )
        .execute(&pool)
        .await
        .expect("schéma");
        sqlx::query("INSERT INTO users_accounts VALUES (?, '{}', '2026-01-01')")
            .bind(marker)
            .execute(&pool)
            .await
            .expect("compte témoin");
        pool
    }

    /// Ancienne base (source de la migration), fermée proprement.
    async fn legacy_database(dir: &Path, marker: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("dossier de l'ancienne base");
        let file = dir.join(LEGACY_DB_FILE);
        wal_database(&file, marker).await.close().await;
        file
    }

    async fn usernames(file: &Path) -> Vec<String> {
        let mut conn = SqliteConnectOptions::new()
            .filename(file)
            .read_only(true)
            .connect()
            .await
            .expect("ouverture en lecture");
        let names = sqlx::query_scalar("SELECT username FROM users_accounts ORDER BY username")
            .fetch_all(&mut conn)
            .await
            .expect("lecture des comptes");
        let _ = conn.close().await;
        names
    }

    /// Le fichier contient-il ces octets ? (une donnée texte est écrite telle quelle par SQLite)
    fn contains(file: &Path, needle: &str) -> bool {
        let bytes = std::fs::read(file).unwrap_or_default();
        bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
    }

    /// Sauvegardes `<fichier>.bak-*` présentes à côté du fichier.
    fn backups_of(file: &Path) -> Vec<PathBuf> {
        let prefix = format!("{}.bak-", file.file_name().unwrap().to_string_lossy());
        let mut found: Vec<PathBuf> = std::fs::read_dir(file.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(&prefix)
            })
            .collect();
        found.sort();
        found
    }

    /// Reproduit l'anomalie : base du moteur arrêté brutalement (données seulement dans le
    /// journal WAL), remplacée par `migrate-legacy --force`. La sauvegarde doit tout contenir.
    #[tokio::test]
    async fn force_sauvegarde_aussi_le_journal_wal_de_la_base_remplacee() {
        let scratch = Scratch::new("wal");
        let layout = Layout::portable(&scratch.0.join("instance"));
        let cfg = AppConfig::portable_default();
        let destination = layout.database_file(&cfg);
        std::fs::create_dir_all(&layout.data_dir).unwrap();

        // Base en service : son état sur disque (fichier principal + journal) est recopié,
        // comme après un arrêt brutal du moteur. Le fichier -shm (verrouillé) est reconstruit.
        let live_dir = scratch.0.join("live");
        std::fs::create_dir_all(&live_dir).unwrap();
        let live = live_dir.join("apex-scada.db");
        let pool = wal_database(&live, "compte_en_wal").await;
        std::fs::copy(&live, &destination).unwrap();
        std::fs::copy(sidecar(&live, "-wal"), sidecar(&destination, "-wal")).unwrap();
        pool.close().await;
        assert!(
            !contains(&destination, "compte_en_wal")
                && contains(&sidecar(&destination, "-wal"), "compte_en_wal"),
            "préalable : la donnée doit se trouver dans le journal seulement"
        );

        let source = legacy_database(&scratch.0.join("v1"), "compte_v1").await;
        let source_before = std::fs::read(&source).unwrap();

        let report = migrate_database(&source, &layout, &cfg, true)
            .await
            .expect("migration --force");

        let backup = report
            .previous_destination_backup
            .expect("sauvegarde de la base remplacée");
        assert_eq!(backups_of(&destination), vec![backup.clone()]);
        assert_eq!(usernames(&backup).await, vec!["compte_en_wal".to_string()]);
        // La nouvelle base est la copie de la source, sans le journal de l'ancienne
        assert_eq!(usernames(&destination).await, vec!["compte_v1".to_string()]);
        assert!(!contains(&sidecar(&destination, "-wal"), "compte_en_wal"));
        assert!(!sidecar(&destination, ".migration").exists());
        assert_eq!(report.counts, vec![("users_accounts".to_string(), 1)]);
        // La source n'est jamais modifiée
        assert_eq!(std::fs::read(&source).unwrap(), source_before);
    }

    #[tokio::test]
    async fn sans_force_la_base_existante_est_intacte() {
        let scratch = Scratch::new("noforce");
        let layout = Layout::portable(&scratch.0.join("instance"));
        let cfg = AppConfig::portable_default();
        let destination = layout.database_file(&cfg);
        std::fs::create_dir_all(&layout.data_dir).unwrap();
        wal_database(&destination, "compte_actuel")
            .await
            .close()
            .await;
        let before = std::fs::read(&destination).unwrap();
        let source = legacy_database(&scratch.0.join("v1"), "compte_v1").await;

        let err = migrate_database(&source, &layout, &cfg, false)
            .await
            .unwrap_err();
        assert!(err.contains("--force"), "{err}");
        assert_eq!(std::fs::read(&destination).unwrap(), before);
        assert!(backups_of(&destination).is_empty());
        assert!(!sidecar(&destination, ".migration").exists());
    }

    /// Base existante illisible : refus, rien n'est modifié ni supprimé.
    #[tokio::test]
    async fn base_existante_illisible_refus_sans_rien_modifier() {
        let scratch = Scratch::new("illisible");
        let layout = Layout::portable(&scratch.0.join("instance"));
        let cfg = AppConfig::portable_default();
        let destination = layout.database_file(&cfg);
        std::fs::create_dir_all(&layout.data_dir).unwrap();
        let garbage = vec![0x5Au8; 8192];
        std::fs::write(&destination, &garbage).unwrap();
        std::fs::write(sidecar(&destination, "-wal"), b"journal inconnu").unwrap();
        let source = legacy_database(&scratch.0.join("v1"), "compte_v1").await;

        let err = migrate_database(&source, &layout, &cfg, true)
            .await
            .unwrap_err();
        assert!(err.contains("reste en place"), "{err}");
        assert_eq!(std::fs::read(&destination).unwrap(), garbage);
        assert_eq!(
            std::fs::read(sidecar(&destination, "-wal")).unwrap(),
            b"journal inconnu"
        );
        assert!(backups_of(&destination).is_empty());
        assert!(!sidecar(&destination, ".migration").exists());
    }

    /// Journal orphelin (sans base principale) : mis de côté, jamais supprimé ni rejoué.
    #[tokio::test]
    async fn journal_orphelin_mis_de_cote() {
        let scratch = Scratch::new("orphelin");
        let layout = Layout::portable(&scratch.0.join("instance"));
        let cfg = AppConfig::portable_default();
        let destination = layout.database_file(&cfg);
        std::fs::create_dir_all(&layout.data_dir).unwrap();
        std::fs::write(sidecar(&destination, "-wal"), b"journal orphelin").unwrap();
        let source = legacy_database(&scratch.0.join("v1"), "compte_v1").await;

        let report = migrate_database(&source, &layout, &cfg, false)
            .await
            .expect("migration");
        assert!(report.previous_destination_backup.is_none());
        assert_eq!(usernames(&destination).await, vec!["compte_v1".to_string()]);
        let kept = backups_of(&sidecar(&destination, "-wal"));
        assert_eq!(kept.len(), 1, "journal orphelin conservé");
        assert_eq!(std::fs::read(&kept[0]).unwrap(), b"journal orphelin");
    }

    #[tokio::test]
    async fn base_vierge_reconnue_par_son_contenu() {
        let pool = crate::db::open(None).await.unwrap();
        assert!(is_blank_database(&pool).await.unwrap());
        sqlx::query("INSERT INTO tags_catalog (id, tag_json, updated_at) VALUES ('T', '{}', '')")
            .execute(&pool)
            .await
            .unwrap();
        assert!(!is_blank_database(&pool).await.unwrap());
    }

    #[test]
    fn noms_des_fichiers_annexes() {
        let db = Path::new("donnees").join("base.sqlite");
        assert_eq!(
            sidecar(&db, "-wal"),
            Path::new("donnees").join("base.sqlite-wal")
        );
        let bak = free_backup_path(&db);
        assert!(bak
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("base.sqlite.bak-"));
    }
}
