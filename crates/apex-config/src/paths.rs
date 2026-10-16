//! Couche système : emplacements des fichiers selon le système d'exploitation et le mode
//! d'installation. C'est le **seul** endroit du code qui connaît les conventions Windows/Linux.
//!
//! | Élément        | Windows (installé)                              | Linux (installé)                    | Portable          |
//! |----------------|-------------------------------------------------|-------------------------------------|-------------------|
//! | Configuration  | `%ProgramData%\Apex Supervision\config\`        | `/etc/apex-supervision/`            | `<base>/`         |
//! | Sauvegardes    | `…\config\backups\`                             | `/etc/apex-supervision/backups/`    | `<base>/backups/` |
//! | Données        | `%ProgramData%\Apex Supervision\data\`          | `/var/lib/apex-supervision/`        | `<base>/data/`    |
//! | Journaux       | `%ProgramData%\Apex Supervision\logs\`          | `/var/log/apex-supervision/`        | `<base>/logs/`    |
//! | Exécution      | `%ProgramData%\Apex Supervision\run\`           | `/run/apex-supervision/`            | `<base>/run/`     |
//! | IHM web        | `<dossier du programme>\web\`                   | `/usr/share/apex-supervision/web/`  | `static/` trouvé  |

use std::path::{Path, PathBuf};

use crate::model::{AppConfig, CONFIG_FILE_NAME};

/// Variable d'environnement désignant explicitement le fichier de configuration.
pub const CONFIG_ENV: &str = "APEX_SCADA_CONFIG";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutKind {
    /// Installation système (installateur Windows, paquet Linux).
    System,
    /// Tout dans un dossier unique (développement, démonstration, clé USB).
    Portable,
}

/// Emplacements résolus d'une instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub kind: LayoutKind,
    pub config_file: PathBuf,
    pub backup_dir: PathBuf,
    pub data_dir: PathBuf,
    pub log_dir: PathBuf,
    pub run_dir: PathBuf,
    /// IHM web installée avec le programme (mode système).
    pub default_web_dir: Option<PathBuf>,
}

impl Layout {
    /// Emplacements standard d'une installation système.
    pub fn system() -> Self {
        #[cfg(windows)]
        {
            let program_data = std::env::var_os("ProgramData")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
            let root = program_data.join(crate::version::PRODUCT_NAME);
            let config_dir = root.join("config");
            Self {
                kind: LayoutKind::System,
                config_file: config_dir.join(CONFIG_FILE_NAME),
                backup_dir: config_dir.join("backups"),
                data_dir: root.join("data"),
                log_dir: root.join("logs"),
                run_dir: root.join("run"),
                default_web_dir: exe_dir().map(|d| d.join("web")),
            }
        }
        #[cfg(not(windows))]
        {
            let config_dir = PathBuf::from("/etc/apex-supervision");
            Self {
                kind: LayoutKind::System,
                config_file: config_dir.join(CONFIG_FILE_NAME),
                backup_dir: config_dir.join("backups"),
                data_dir: PathBuf::from("/var/lib/apex-supervision"),
                log_dir: PathBuf::from("/var/log/apex-supervision"),
                run_dir: PathBuf::from("/run/apex-supervision"),
                default_web_dir: Some(PathBuf::from("/usr/share/apex-supervision/web")),
            }
        }
    }

    /// Tout est rangé sous `base` (configuration à la racine).
    pub fn portable(base: &Path) -> Self {
        Self {
            kind: LayoutKind::Portable,
            config_file: base.join(CONFIG_FILE_NAME),
            backup_dir: base.join("backups"),
            data_dir: base.join("data"),
            log_dir: base.join("logs"),
            run_dir: base.join("run"),
            default_web_dir: None,
        }
    }

    /// Détermine les emplacements, dans cet ordre :
    /// 1. fichier explicite (`--config`), puis variable `APEX_SCADA_CONFIG` ;
    /// 2. `apex-scada.toml` dans le dossier courant, puis à côté de l'exécutable → mode portable ;
    /// 3. sinon, installation système.
    pub fn resolve(explicit_config: Option<&Path>) -> Self {
        let explicit = explicit_config
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os(CONFIG_ENV).map(PathBuf::from));
        if let Some(file) = explicit {
            return Self::for_config_file(&file);
        }
        // Un fichier trouvé dans le dossier courant ou à côté de l'exécutable désigne une
        // instance portable, sauf s'il s'agit du fichier système lui-même (commande lancée
        // depuis le dossier de configuration système).
        if let Ok(cwd) = std::env::current_dir() {
            if cwd.join(CONFIG_FILE_NAME).is_file() {
                return Self::for_config_file(&cwd.join(CONFIG_FILE_NAME));
            }
        }
        if let Some(dir) = exe_dir() {
            if dir.join(CONFIG_FILE_NAME).is_file() {
                return Self::for_config_file(&dir.join(CONFIG_FILE_NAME));
            }
        }
        Self::system()
    }

    /// Emplacements associés à un fichier de configuration donné.
    pub fn for_config_file(file: &Path) -> Self {
        let system = Self::system();
        let file = absolutize(file);
        if same_file(&file, &system.config_file) {
            return system;
        }
        let base = file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let mut layout = Self::portable(&base);
        layout.config_file = file;
        layout
    }

    /// Applique les répertoires personnalisés de la configuration (`[paths]`).
    pub fn with_overrides(mut self, cfg: &AppConfig) -> Self {
        if !cfg.paths.data_dir.trim().is_empty() {
            self.data_dir = PathBuf::from(cfg.paths.data_dir.trim());
        }
        if !cfg.paths.log_dir.trim().is_empty() {
            self.log_dir = PathBuf::from(cfg.paths.log_dir.trim());
        }
        self
    }

    pub fn database_file(&self, cfg: &AppConfig) -> PathBuf {
        let file = Path::new(cfg.database.file.trim());
        if file.is_absolute() {
            file.to_path_buf()
        } else {
            self.data_dir.join(file)
        }
    }

    /// Verrou d'instance unique (dans le répertoire des données).
    pub fn lock_file(&self) -> PathBuf {
        self.data_dir.join("apex-scada.lock")
    }

    /// Jeton de contrôle local (arrêt propre par l'utilitaire desktop).
    pub fn control_token_file(&self) -> PathBuf {
        self.run_dir.join("control.token")
    }

    /// Verrou d'écriture du fichier de configuration.
    pub fn config_lock_file(&self) -> PathBuf {
        let mut name = self
            .config_file
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        name.push(".lock");
        self.config_file.with_file_name(name)
    }

    /// Répertoire de l'IHM web : paramètre `paths.web_dir`, sinon IHM installée, sinon recherche
    /// d'un dossier `static/` ou `web/` (dossier de configuration, dossier courant, ou en remontant
    /// depuis l'exécutable — cas `target/release` en développement).
    pub fn web_dir(&self, cfg: &AppConfig) -> PathBuf {
        if !cfg.paths.web_dir.trim().is_empty() {
            return PathBuf::from(cfg.paths.web_dir.trim());
        }
        if let Some(dir) = self.default_web_dir.as_ref().filter(|d| has_index(d)) {
            return dir.clone();
        }
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(base) = self.config_file.parent() {
            candidates.push(base.join("static"));
            candidates.push(base.join("web"));
        }
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join("static"));
            candidates.push(cwd.join("web"));
        }
        if let Ok(exe) = std::env::current_exe() {
            for dir in exe.ancestors().skip(1).take(4) {
                candidates.push(dir.join("web"));
                candidates.push(dir.join("static"));
            }
        }
        candidates
            .into_iter()
            .find(|d| has_index(d))
            .or_else(|| self.default_web_dir.clone())
            .unwrap_or_else(|| PathBuf::from("static"))
    }
}

fn has_index(dir: &Path) -> bool {
    dir.join("index.html").is_file()
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

/// Deux chemins désignent-ils le même fichier ? Chemins canoniques quand ils existent (noms
/// courts 8.3, liens), sinon comparaison textuelle, insensible à la casse sous Windows.
fn same_file(a: &Path, b: &Path) -> bool {
    if let (Ok(ca), Ok(cb)) = (a.canonicalize(), b.canonicalize()) {
        return ca == cb;
    }
    if cfg!(windows) {
        let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").to_lowercase();
        norm(a) == norm(b)
    } else {
        a == b
    }
}

fn absolutize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_layout_is_self_contained() {
        let base = std::env::temp_dir().join("apex-layout-test");
        let l = Layout::portable(&base);
        assert_eq!(l.kind, LayoutKind::Portable);
        assert_eq!(l.config_file, base.join("apex-scada.toml"));
        for d in [&l.data_dir, &l.log_dir, &l.run_dir, &l.backup_dir] {
            assert!(d.starts_with(&base));
        }
        assert_eq!(l.config_lock_file(), base.join("apex-scada.toml.lock"));
    }

    #[test]
    fn system_layout_separates_program_and_data() {
        let l = Layout::system();
        assert_eq!(l.kind, LayoutKind::System);
        assert!(l.config_file.ends_with("apex-scada.toml"));
        assert_ne!(l.data_dir, l.log_dir);
        #[cfg(not(windows))]
        assert_eq!(l.data_dir, PathBuf::from("/var/lib/apex-supervision"));
        #[cfg(windows)]
        assert!(l.data_dir.to_string_lossy().contains("Apex Supervision"));
    }

    #[test]
    fn explicit_config_file_drives_layout() {
        let file = std::env::temp_dir()
            .join("apex-explicit")
            .join("site-a.toml");
        let l = Layout::for_config_file(&file);
        assert_eq!(l.config_file, file);
        assert_eq!(l.data_dir, file.parent().unwrap().join("data"));
    }

    #[test]
    fn system_config_file_is_recognised_whatever_the_case() {
        let sys = Layout::system().config_file;
        assert_eq!(Layout::for_config_file(&sys).kind, LayoutKind::System);
        #[cfg(windows)]
        {
            let lower = PathBuf::from(sys.to_string_lossy().to_lowercase());
            assert_eq!(Layout::for_config_file(&lower).kind, LayoutKind::System);
        }
    }

    #[test]
    fn overrides_and_database_path() {
        let base = std::env::temp_dir().join("apex-ovr");
        let mut cfg = AppConfig::default();
        let data = std::env::temp_dir().join("apex-ovr-data");
        cfg.paths.data_dir = data.to_string_lossy().to_string();
        let l = Layout::portable(&base).with_overrides(&cfg);
        assert_eq!(l.data_dir, data);
        assert_eq!(l.database_file(&cfg), data.join("apex-scada.db"));
        assert_eq!(l.lock_file(), data.join("apex-scada.lock"));
    }
}
