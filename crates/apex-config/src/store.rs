//! Lecture, migration, écriture atomique, sauvegarde et restauration du fichier de configuration.
//!
//! Règles d'écriture :
//! - la configuration est **validée** avant d'être écrite (aucune erreur bloquante) ;
//! - un **verrou** (`apex-scada.toml.lock`) empêche deux écritures simultanées ;
//! - si le fichier a changé sur disque depuis sa lecture, l'écriture est **refusée** (conflit) ;
//! - le fichier existant est **sauvegardé** (horodaté) avant d'être remplacé ;
//! - l'écriture est **atomique** : fichier temporaire, synchronisation disque, puis renommage.

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use thiserror::Error;

use crate::{
    model::{fnv1a64, has_errors, AppConfig, Issue, CONFIG_VERSION},
    paths::Layout,
};

/// Nombre de sauvegardes conservées.
pub const MAX_BACKUPS: usize = 20;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("lecture de « {path} » impossible : {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("écriture de « {path} » impossible : {source}")]
    Write { path: PathBuf, source: std::io::Error },
    #[error("syntaxe TOML invalide dans « {path} » : {message}")]
    Syntax { path: PathBuf, message: String },
    #[error("paramètre invalide dans « {path} » : {message}")]
    Schema { path: PathBuf, message: String },
    #[error("format de configuration {found} plus récent que celui pris en charge ({supported}) : mettez à jour le logiciel")]
    TooNew { found: u32, supported: u32 },
    #[error("configuration refusée : {}", .0.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(" ; "))]
    Invalid(Vec<Issue>),
    #[error("le fichier de configuration est en cours de modification par un autre programme")]
    Locked,
    #[error("le fichier de configuration a été modifié par ailleurs depuis sa lecture : rechargez-le avant d'enregistrer")]
    Conflict,
}

/// Résultat d'une lecture.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: AppConfig,
    /// Version d'origine si une migration a été appliquée en mémoire.
    pub migrated_from: Option<u32>,
    /// Résultat de la validation (erreurs et avertissements).
    pub issues: Vec<Issue>,
    /// Empreinte du contenu brut du fichier (détection des modifications concurrentes).
    /// `None` si le fichier n'existait pas.
    pub file_hash: Option<u64>,
}

/// Lit et migre le fichier de configuration (sans le réécrire).
pub fn load(path: &Path) -> Result<Loaded, ConfigError> {
    let raw = fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let (config, migrated_from) = parse(&raw, path)?;
    let issues = config.validate();
    Ok(Loaded {
        config,
        migrated_from,
        issues,
        file_hash: Some(fnv1a64(raw.as_bytes())),
    })
}

/// Comme [`load`], mais renvoie `default` si le fichier n'existe pas.
pub fn load_or(path: &Path, default: AppConfig) -> Result<Loaded, ConfigError> {
    if !path.exists() {
        let issues = default.validate();
        return Ok(Loaded {
            config: default,
            migrated_from: None,
            issues,
            file_hash: None,
        });
    }
    load(path)
}

fn parse(raw: &str, path: &Path) -> Result<(AppConfig, Option<u32>), ConfigError> {
    let value: toml::Value = toml::from_str(raw).map_err(|e| ConfigError::Syntax {
        path: path.to_path_buf(),
        message: e.message().to_string(),
    })?;
    let (value, migrated_from) = crate::migrate::migrate(value)?;
    let config: AppConfig = value.try_into().map_err(|e: toml::de::Error| ConfigError::Schema {
        path: path.to_path_buf(),
        message: e.message().to_string(),
    })?;
    Ok((config, migrated_from))
}

/// Rapport d'écriture.
#[derive(Debug, Clone)]
pub struct SaveReport {
    /// Sauvegarde du fichier précédent, s'il existait.
    pub backup: Option<PathBuf>,
    /// Empreinte du nouveau contenu (à conserver pour une écriture suivante).
    pub file_hash: u64,
}

/// Écrit la configuration de manière sûre (voir les règles en tête de module).
///
/// `expected_hash` : empreinte du fichier au moment de sa lecture (`Loaded::file_hash`).
/// Si le fichier a changé depuis, l'écriture est refusée avec [`ConfigError::Conflict`].
pub fn save(
    layout: &Layout,
    config: &AppConfig,
    expected_hash: Option<u64>,
) -> Result<SaveReport, ConfigError> {
    let issues = config.validate();
    if has_errors(&issues) {
        return Err(ConfigError::Invalid(
            issues
                .into_iter()
                .filter(|i| i.severity == crate::model::Severity::Error)
                .collect(),
        ));
    }

    let path = &layout.config_file;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).map_err(|source| ConfigError::Write {
        path: dir.to_path_buf(),
        source,
    })?;

    let lock_path = layout.config_lock_file();
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|source| ConfigError::Write {
            path: lock_path.clone(),
            source,
        })?;
    lock.try_lock().map_err(|_| ConfigError::Locked)?;

    // Détection des modifications concurrentes (dans la section protégée par le verrou)
    let current_raw = fs::read_to_string(path).ok();
    let current_hash = current_raw.as_deref().map(|r| fnv1a64(r.as_bytes()));
    if current_hash != expected_hash {
        return Err(ConfigError::Conflict);
    }

    let backup = match &current_raw {
        Some(raw) => Some(write_backup(&layout.backup_dir, raw)?),
        None => None,
    };

    let content = render(config);
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("config"),
        std::process::id()
    ));
    let write_tmp = || -> std::io::Result<()> {
        let mut f = File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()
    };
    if let Err(source) = write_tmp() {
        let _ = fs::remove_file(&tmp);
        return Err(ConfigError::Write {
            path: tmp.clone(),
            source,
        });
    }
    fs::rename(&tmp, path).map_err(|source| {
        let _ = fs::remove_file(&tmp);
        ConfigError::Write {
            path: path.clone(),
            source,
        }
    })?;
    prune_backups(&layout.backup_dir);
    drop(lock);

    Ok(SaveReport {
        backup,
        file_hash: fnv1a64(content.as_bytes()),
    })
}

fn write_backup(backup_dir: &Path, raw: &str) -> Result<PathBuf, ConfigError> {
    fs::create_dir_all(backup_dir).map_err(|source| ConfigError::Write {
        path: backup_dir.to_path_buf(),
        source,
    })?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S%.3f");
    let target = backup_dir.join(format!("apex-scada-{}.toml", stamp));
    fs::write(&target, raw).map_err(|source| ConfigError::Write {
        path: target.clone(),
        source,
    })?;
    Ok(target)
}

/// Sauvegardes disponibles, de la plus récente à la plus ancienne.
pub fn list_backups(backup_dir: &Path) -> Vec<PathBuf> {
    let mut list: Vec<PathBuf> = fs::read_dir(backup_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("apex-scada-") && n.ends_with(".toml"))
                })
                .collect()
        })
        .unwrap_or_default();
    list.sort();
    list.reverse();
    list
}

fn prune_backups(backup_dir: &Path) {
    for old in list_backups(backup_dir).into_iter().skip(MAX_BACKUPS) {
        let _ = fs::remove_file(old);
    }
}

/// Restaure une sauvegarde : elle est validée, puis écrite comme une nouvelle configuration
/// (la configuration courante est elle-même sauvegardée avant d'être remplacée).
pub fn restore_backup(
    layout: &Layout,
    backup: &Path,
    expected_hash: Option<u64>,
) -> Result<SaveReport, ConfigError> {
    let loaded = load(backup)?;
    save(layout, &loaded.config, expected_hash)
}

/// Empreinte du fichier sur disque (ou `None` s'il n'existe pas).
pub fn file_hash(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok().map(|r| fnv1a64(r.as_bytes()))
}

fn s(v: &str) -> String {
    toml::Value::String(v.to_string()).to_string()
}

/// Rendu TOML commenté, lisible et modifiable à la main par un administrateur.
pub fn render(c: &AppConfig) -> String {
    format!(
        r#"# Configuration d'Apex Supervision — généré par {product} {version}
# Toute modification de ce fichier nécessite un redémarrage du moteur.
# Les réglages modifiables à chaud (mise en sommeil des alarmes...) se gèrent dans l'IHM web.

# Version du format de ce fichier (ne pas modifier : gérée par les migrations).
config_version = {config_version}

[instance]
# Nom affiché de l'instance (IHM, journaux, utilitaire desktop).
name = {name}

[server]
# Adresse d'écoute : "127.0.0.1" = poste local uniquement ; "0.0.0.0" = tout le réseau.
# Attention : le serveur parle HTTP non chiffré. Pour un accès réseau, utilisez un proxy HTTPS.
bind = {bind}
# Port HTTP de l'IHM web et de l'API.
port = {port}

[paths]
# Répertoires personnalisés (chemins absolus). Vide = emplacement standard du système.
data_dir = {data_dir}
log_dir = {log_dir}
web_dir = {web_dir}

[database]
# Fichier SQLite (configuration métier, historique, audit, comptes), relatif à data_dir ou absolu.
file = {db_file}

[history]
# Rétention de l'historique des mesures, en jours (0 = illimitée).
retention_days = {retention_days}

[security]
# Expiration d'une session inactive, en heures (1 à 720).
session_ttl_hours = {session_ttl}

[logging]
# Niveau : "error", "warn", "info", "debug" ou "trace".
level = {level}
# Journaux dans des fichiers (rotation quotidienne) en plus de la sortie standard.
file_enabled = {file_enabled}
# Nombre de fichiers journaliers conservés (1 à 365).
retention_files = {retention_files}

[safety]
# Mode sûr (mise en service, tests) : aucune commande n'est envoyée aux équipements réels.
# La supervision reste active ; le planificateur et les sorties des flux sont bloqués.
safe_mode = {safe_mode}

[simulation]
# Automate Modbus embarqué, simulateur GTB et données de démonstration.
# Les drivers OPC-UA, BACnet/IP et Siemens S7 sont SIMULÉS : ils ne fonctionnent qu'avec la simulation active.
enabled = {sim_enabled}
# Port de l'automate Modbus TCP embarqué (écoute sur 127.0.0.1 uniquement).
modbus_slave_port = {sim_port}
"#,
        product = crate::version::PRODUCT_NAME,
        version = crate::version::VERSION,
        config_version = CONFIG_VERSION,
        name = s(&c.instance.name),
        bind = s(&c.server.bind),
        port = c.server.port,
        data_dir = s(&c.paths.data_dir),
        log_dir = s(&c.paths.log_dir),
        web_dir = s(&c.paths.web_dir),
        db_file = s(&c.database.file),
        retention_days = c.history.retention_days,
        session_ttl = c.security.session_ttl_hours,
        level = s(&c.logging.level),
        file_enabled = c.logging.file_enabled,
        retention_files = c.logging.retention_files,
        safe_mode = c.safety.safe_mode,
        sim_enabled = c.simulation.enabled,
        sim_port = c.simulation.modbus_slave_port,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Severity;

    fn temp_layout(name: &str) -> Layout {
        let base = std::env::temp_dir().join(format!("apex-store-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&base);
        Layout::portable(&base)
    }

    #[test]
    fn render_roundtrip_is_lossless() {
        let mut cfg = AppConfig::portable_default();
        cfg.instance.name = "Site \"Nord\" — CTA".into();
        cfg.server.port = 8443;
        cfg.paths.data_dir = r"D:\données\apex".into();
        let raw = render(&cfg);
        let (parsed, migrated) = parse(&raw, Path::new("test.toml")).unwrap();
        assert_eq!(parsed, cfg);
        assert!(migrated.is_none());
    }

    #[test]
    fn save_creates_backup_and_detects_conflicts() {
        let layout = temp_layout("save");
        let cfg = AppConfig::default();
        let first = save(&layout, &cfg, None).unwrap();
        assert!(first.backup.is_none(), "pas de sauvegarde à la création");

        let mut cfg2 = cfg.clone();
        cfg2.server.port = 3100;
        let second = save(&layout, &cfg2, Some(first.file_hash)).unwrap();
        assert!(second.backup.as_ref().unwrap().is_file());
        assert_eq!(load(&layout.config_file).unwrap().config.server.port, 3100);

        // Écriture basée sur une lecture périmée → conflit, fichier inchangé
        let mut cfg3 = cfg.clone();
        cfg3.server.port = 3200;
        assert!(matches!(save(&layout, &cfg3, Some(first.file_hash)), Err(ConfigError::Conflict)));
        assert_eq!(load(&layout.config_file).unwrap().config.server.port, 3100);
        let _ = fs::remove_dir_all(layout.config_file.parent().unwrap());
    }

    #[test]
    fn invalid_config_is_never_written() {
        let layout = temp_layout("invalid");
        let mut cfg = AppConfig::default();
        cfg.server.bind = "pas-une-ip".into();
        assert!(matches!(save(&layout, &cfg, None), Err(ConfigError::Invalid(_))));
        assert!(!layout.config_file.exists());
    }

    #[test]
    fn restore_previous_backup() {
        let layout = temp_layout("restore");
        let a = save(&layout, &AppConfig::default(), None).unwrap();
        let mut cfg = AppConfig::default();
        cfg.instance.name = "Modifiée".into();
        let b = save(&layout, &cfg, Some(a.file_hash)).unwrap();
        let backups = list_backups(&layout.backup_dir);
        assert_eq!(backups.len(), 1);
        restore_backup(&layout, &backups[0], Some(b.file_hash)).unwrap();
        assert_eq!(load(&layout.config_file).unwrap().config.instance.name, "Apex Supervision");
        assert_eq!(list_backups(&layout.backup_dir).len(), 2, "la version remplacée est sauvegardée");
        let _ = fs::remove_dir_all(layout.config_file.parent().unwrap());
    }

    #[test]
    fn concurrent_writer_gets_locked_error() {
        let layout = temp_layout("lock");
        fs::create_dir_all(layout.config_file.parent().unwrap()).unwrap();
        let holder = OpenOptions::new().create(true).truncate(false).write(true).open(layout.config_lock_file()).unwrap();
        holder.lock().unwrap();
        assert!(matches!(save(&layout, &AppConfig::default(), None), Err(ConfigError::Locked)));
        drop(holder);
        assert!(save(&layout, &AppConfig::default(), None).is_ok());
        let _ = fs::remove_dir_all(layout.config_file.parent().unwrap());
    }

    #[test]
    fn readable_errors_for_bad_files() {
        let layout = temp_layout("bad");
        fs::create_dir_all(layout.config_file.parent().unwrap()).unwrap();
        fs::write(&layout.config_file, "config_version = 1\n[server]\nport = \"trois mille\"\n").unwrap();
        let err = load(&layout.config_file).unwrap_err().to_string();
        assert!(err.contains("paramètre invalide"), "{err}");
        fs::write(&layout.config_file, "config_version = 1\n[serveur]\nport = 3000\n").unwrap();
        let err = load(&layout.config_file).unwrap_err().to_string();
        assert!(err.contains("serveur"), "une section inconnue est signalée : {err}");
        fs::write(&layout.config_file, "config_version = 1\n[server]\nbind = \"0.0.0.0\"\n").unwrap();
        let loaded = load(&layout.config_file).unwrap();
        assert!(loaded.issues.iter().any(|i| i.severity == Severity::Warning));
        let _ = fs::remove_dir_all(layout.config_file.parent().unwrap());
    }
}
