//! Modèle de configuration (format v1), valeurs par défaut sûres et validation stricte.

use std::{net::IpAddr, path::Path};

use serde::{Deserialize, Serialize};

/// Version courante du format de fichier de configuration.
pub const CONFIG_VERSION: u32 = 1;

/// Nom du fichier de configuration.
pub const CONFIG_FILE_NAME: &str = "apex-scada.toml";

/// Niveaux de journalisation acceptés.
pub const LOG_LEVELS: [&str; 5] = ["error", "warn", "info", "debug", "trace"];

/// Configuration complète d'une instance Apex Supervision.
///
/// Tous les paramètres de ce fichier sont lus au démarrage du moteur : toute modification
/// nécessite un redémarrage. Les réglages modifiables à chaud (mise en sommeil des alarmes...)
/// sont gérés en base depuis l'IHM web.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub config_version: u32,
    #[serde(default)]
    pub instance: InstanceConfig,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub paths: PathsConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
    #[serde(default)]
    pub history: HistoryConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub safety: SafetyConfig,
    #[serde(default)]
    pub simulation: SimulationConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            config_version: CONFIG_VERSION,
            instance: InstanceConfig::default(),
            server: ServerConfig::default(),
            paths: PathsConfig::default(),
            database: DatabaseConfig::default(),
            history: HistoryConfig::default(),
            security: SecurityConfig::default(),
            logging: LoggingConfig::default(),
            safety: SafetyConfig::default(),
            simulation: SimulationConfig::default(),
        }
    }
}

impl AppConfig {
    /// Configuration par défaut d'une installation portable (développement, démonstration) :
    /// la simulation (automate Modbus embarqué, simulateur GTB, données de démo) est active.
    pub fn portable_default() -> Self {
        let mut cfg = Self::default();
        cfg.simulation.enabled = true;
        cfg
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InstanceConfig {
    /// Nom affiché de l'instance (IHM, journaux, utilitaire desktop).
    pub name: String,
}

impl Default for InstanceConfig {
    fn default() -> Self {
        Self {
            name: "Apex Supervision".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Adresse d'écoute. `127.0.0.1` (défaut) = poste local uniquement ; `0.0.0.0` = tout le réseau.
    pub bind: String,
    /// Port HTTP de l'IHM web et de l'API.
    pub port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".to_string(),
            port: 3000,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PathsConfig {
    /// Répertoire des données (base SQLite, verrou d'instance). Vide = emplacement système.
    pub data_dir: String,
    /// Répertoire des journaux. Vide = emplacement système.
    pub log_dir: String,
    /// Répertoire de l'IHM web. Vide = fichiers installés avec le programme.
    pub web_dir: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DatabaseConfig {
    /// Fichier SQLite, relatif au répertoire des données ou absolu.
    pub file: String,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            file: "apex-scada.db".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HistoryConfig {
    /// Rétention de l'historique en jours (0 = illimitée).
    pub retention_days: u32,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self { retention_days: 30 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecurityConfig {
    /// Expiration d'une session inactive, en heures.
    pub session_ttl_hours: u32,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            session_ttl_hours: 12,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    /// Niveau : error, warn, info, debug ou trace.
    pub level: String,
    /// Écrit les journaux dans des fichiers (rotation quotidienne) en plus de la sortie standard.
    pub file_enabled: bool,
    /// Nombre de fichiers journaliers conservés.
    pub retention_files: u32,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            file_enabled: true,
            retention_files: 14,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SafetyConfig {
    /// Mode sûr (mise en service, tests) : aucune commande n'est envoyée aux équipements réels.
    /// Le planificateur et les sorties des flux sont bloqués ; la supervision reste active.
    pub safe_mode: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SimulationConfig {
    /// Active l'automate Modbus embarqué, le simulateur GTB et les drivers de protocoles simulés
    /// (OPC-UA, BACnet/IP, S7). Désactivé par défaut pour une installation.
    pub enabled: bool,
    /// Port de l'automate Modbus TCP embarqué (écoute sur 127.0.0.1 uniquement).
    pub modbus_slave_port: u16,
}

impl Default for SimulationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            modbus_slave_port: 5502,
        }
    }
}

// ============================================================================
// VALIDATION
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    /// Bloquant : la configuration est refusée.
    Error,
    /// Accepté, mais signalé (exposition réseau, port privilégié...).
    Warning,
}

/// Problème détecté par la validation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Issue {
    pub severity: Severity,
    /// Chemin du paramètre concerné (ex. `server.port`).
    pub field: String,
    pub message: String,
}

impl Issue {
    fn error(field: &str, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            field: field.to_string(),
            message: message.into(),
        }
    }

    fn warning(field: &str, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            field: field.to_string(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Issue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let level = match self.severity {
            Severity::Error => "erreur",
            Severity::Warning => "avertissement",
        };
        write!(f, "[{}] {} : {}", level, self.field, self.message)
    }
}

/// Indique si une liste de problèmes contient au moins une erreur bloquante.
pub fn has_errors(issues: &[Issue]) -> bool {
    issues.iter().any(|i| i.severity == Severity::Error)
}

impl AppConfig {
    /// Valide la configuration. Les erreurs sont bloquantes, les avertissements informatifs.
    pub fn validate(&self) -> Vec<Issue> {
        let mut issues = Vec::new();

        if self.config_version != CONFIG_VERSION {
            issues.push(Issue::error(
                "config_version",
                format!(
                    "version de format {} non prise en charge (attendue : {})",
                    self.config_version, CONFIG_VERSION
                ),
            ));
        }

        let name = self.instance.name.trim();
        if name.is_empty() || name.chars().count() > 64 {
            issues.push(Issue::error(
                "instance.name",
                "le nom de l'instance doit contenir de 1 à 64 caractères",
            ));
        } else if name.chars().any(|c| c.is_control()) {
            issues.push(Issue::error(
                "instance.name",
                "le nom de l'instance ne doit pas contenir de caractères de contrôle",
            ));
        }

        match self.server.bind.trim().parse::<IpAddr>() {
            Err(_) => issues.push(Issue::error(
                "server.bind",
                format!(
                    "« {} » n'est pas une adresse IP (exemples : 127.0.0.1, 0.0.0.0, 192.168.1.10)",
                    self.server.bind
                ),
            )),
            Ok(ip) if ip.is_unspecified() => issues.push(Issue::warning(
                "server.bind",
                "le serveur sera accessible depuis tout le réseau en HTTP non chiffré : \
                 limitez l'accès (pare-feu) ou placez-le derrière un proxy HTTPS",
            )),
            Ok(ip) if !ip.is_loopback() => issues.push(Issue::warning(
                "server.bind",
                "le serveur sera accessible depuis le réseau en HTTP non chiffré",
            )),
            Ok(_) => {}
        }

        if self.server.port == 0 {
            issues.push(Issue::error(
                "server.port",
                "le port doit être compris entre 1 et 65535",
            ));
        } else if self.server.port < 1024 && cfg!(unix) {
            issues.push(Issue::warning(
                "server.port",
                "un port inférieur à 1024 exige des privilèges particuliers sous Linux",
            ));
        }

        for (field, value) in [
            ("paths.data_dir", &self.paths.data_dir),
            ("paths.log_dir", &self.paths.log_dir),
            ("paths.web_dir", &self.paths.web_dir),
        ] {
            if !value.trim().is_empty() && !Path::new(value.trim()).is_absolute() {
                issues.push(Issue::error(
                    field,
                    format!("« {} » doit être un chemin absolu (ou rester vide)", value),
                ));
            }
        }

        let db = self.database.file.trim();
        if db.is_empty() {
            issues.push(Issue::error(
                "database.file",
                "le nom du fichier de base est requis",
            ));
        } else if Path::new(db)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            issues.push(Issue::error(
                "database.file",
                "le chemin de la base ne doit pas contenir « .. »",
            ));
        } else if !Path::new(db).is_absolute()
            && Path::new(db).components().any(|c| {
                matches!(
                    c,
                    std::path::Component::Prefix(_) | std::path::Component::RootDir
                )
            })
        {
            // Windows : « \apex.db » ou « C:apex.db » ne sont ni relatifs à data_dir ni absolus
            issues.push(Issue::error(
                "database.file",
                "indiquez un nom relatif au répertoire des données, ou un chemin absolu complet",
            ));
        }

        if self.history.retention_days > 3650 {
            issues.push(Issue::error(
                "history.retention_days",
                "la rétention est limitée à 3650 jours (0 = illimitée)",
            ));
        }

        if !(1..=720).contains(&self.security.session_ttl_hours) {
            issues.push(Issue::error(
                "security.session_ttl_hours",
                "l'expiration de session doit être comprise entre 1 et 720 heures",
            ));
        }

        if !LOG_LEVELS.contains(&self.logging.level.trim()) {
            issues.push(Issue::error(
                "logging.level",
                format!(
                    "niveau inconnu « {} » (valeurs : {})",
                    self.logging.level,
                    LOG_LEVELS.join(", ")
                ),
            ));
        }
        if !(1..=365).contains(&self.logging.retention_files) {
            issues.push(Issue::error(
                "logging.retention_files",
                "la rétention des journaux doit être comprise entre 1 et 365 fichiers",
            ));
        }

        if self.simulation.enabled {
            if self.simulation.modbus_slave_port == 0 {
                issues.push(Issue::error(
                    "simulation.modbus_slave_port",
                    "le port doit être compris entre 1 et 65535",
                ));
            } else if self.simulation.modbus_slave_port == self.server.port {
                issues.push(Issue::error(
                    "simulation.modbus_slave_port",
                    "le port de l'automate embarqué doit être différent du port web",
                ));
            }
        }

        if self.safety.safe_mode {
            issues.push(Issue::warning(
                "safety.safe_mode",
                "mode sûr actif : aucune commande ne sera envoyée aux équipements",
            ));
        }

        issues
    }

    /// Liste des paramètres modifiés entre deux configurations (chemins `section.clé`).
    /// Tous ces paramètres nécessitent un redémarrage du moteur pour être appliqués.
    pub fn changed_fields(&self, other: &AppConfig) -> Vec<String> {
        let a = flatten(&toml::Value::try_from(self).unwrap_or(toml::Value::Boolean(false)));
        let b = flatten(&toml::Value::try_from(other).unwrap_or(toml::Value::Boolean(false)));
        let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
        keys.sort();
        keys.dedup();
        keys.into_iter()
            .filter(|k| a.get(*k) != b.get(*k))
            .cloned()
            .collect()
    }

    /// Empreinte stable de la configuration (FNV-1a 64 bits sur sa forme canonique).
    /// Le moteur publie celle de la configuration chargée ; l'utilitaire la compare au fichier.
    pub fn fingerprint(&self) -> String {
        let canonical = toml::to_string(self).unwrap_or_default();
        format!("{:016x}", fnv1a64(canonical.as_bytes()))
    }
}

fn flatten(value: &toml::Value) -> std::collections::BTreeMap<String, toml::Value> {
    let mut out = std::collections::BTreeMap::new();
    fn walk(
        prefix: &str,
        v: &toml::Value,
        out: &mut std::collections::BTreeMap<String, toml::Value>,
    ) {
        match v {
            toml::Value::Table(t) => {
                for (k, child) in t {
                    let key = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{}.{}", prefix, k)
                    };
                    walk(&key, child, out);
                }
            }
            other => {
                out.insert(prefix.to_string(), other.clone());
            }
        }
    }
    walk("", value, &mut out);
    out
}

pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe_and_valid() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.server.bind, "127.0.0.1", "écoute locale par défaut");
        assert!(
            !cfg.simulation.enabled,
            "pas de simulation dans une installation"
        );
        assert!(!cfg.safety.safe_mode);
        assert!(!has_errors(&cfg.validate()), "{:?}", cfg.validate());
        assert!(AppConfig::portable_default().simulation.enabled);
    }

    #[test]
    fn database_file_must_stay_relative_or_be_fully_absolute() {
        let mut cfg = AppConfig::default();
        for ok in ["apex-scada.db", "sous-dossier/base.db"] {
            cfg.database.file = ok.into();
            assert!(!has_errors(&cfg.validate()), "{ok} accepté");
        }
        #[cfg(windows)]
        for bad in [r"\apex.db", "C:apex.db"] {
            cfg.database.file = bad.into();
            assert!(
                cfg.validate().iter().any(|i| i.field == "database.file"),
                "{bad} refusé"
            );
        }
    }

    #[test]
    fn invalid_values_are_reported_per_field() {
        let mut cfg = AppConfig::default();
        cfg.server.bind = "localhost".into();
        cfg.server.port = 0;
        cfg.logging.level = "verbose".into();
        cfg.security.session_ttl_hours = 0;
        cfg.paths.data_dir = "relatif/data".into();
        cfg.database.file = "../evasion.db".into();
        let fields: Vec<String> = cfg
            .validate()
            .into_iter()
            .filter(|i| i.severity == Severity::Error)
            .map(|i| i.field)
            .collect();
        for f in [
            "server.bind",
            "server.port",
            "logging.level",
            "security.session_ttl_hours",
            "paths.data_dir",
            "database.file",
        ] {
            assert!(
                fields.iter().any(|x| x == f),
                "erreur attendue sur {f} : {fields:?}"
            );
        }
    }

    #[test]
    fn network_exposure_is_a_warning_not_an_error() {
        let mut cfg = AppConfig::default();
        cfg.server.bind = "0.0.0.0".into();
        let issues = cfg.validate();
        assert!(!has_errors(&issues));
        assert!(issues
            .iter()
            .any(|i| i.field == "server.bind" && i.severity == Severity::Warning));
    }

    #[test]
    fn port_conflict_with_embedded_plc() {
        let mut cfg = AppConfig::portable_default();
        cfg.simulation.modbus_slave_port = cfg.server.port;
        assert!(cfg
            .validate()
            .iter()
            .any(|i| i.field == "simulation.modbus_slave_port"));
    }

    #[test]
    fn changed_fields_and_fingerprint() {
        let a = AppConfig::default();
        let mut b = a.clone();
        assert_eq!(a.fingerprint(), b.fingerprint());
        b.server.port = 8080;
        b.logging.level = "debug".into();
        assert_eq!(
            a.changed_fields(&b),
            vec!["logging.level".to_string(), "server.port".to_string()]
        );
        assert_ne!(a.fingerprint(), b.fingerprint());
    }
}
