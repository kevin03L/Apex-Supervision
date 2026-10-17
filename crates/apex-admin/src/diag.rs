//! Diagnostics de l'installation : chaque vérification produit un constat lisible et, en cas
//! de problème, l'action à mener.

use std::path::Path;

use apex_config::{store, version, AppConfig, Layout, LayoutKind, Severity};

use crate::{
    control::{ServiceInfo, ServiceState},
    logs,
    probe::{self, Health},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub struct Check {
    pub level: Level,
    pub title: String,
    pub detail: String,
}

fn check(level: Level, title: &str, detail: impl Into<String>) -> Check {
    Check {
        level,
        title: title.to_string(),
        detail: detail.into(),
    }
}

/// Base d'une version antérieure (`rust_scada_flow.db`) dans les emplacements habituels.
pub fn find_legacy_db(layout: &Layout) -> Option<std::path::PathBuf> {
    let mut dirs = vec![layout.data_dir.clone()];
    if let Some(d) = layout.config_file.parent() {
        dirs.push(d.to_path_buf());
    }
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd);
    }
    dirs.into_iter()
        .map(|d| d.join("rust_scada_flow.db"))
        .find(|p| p.is_file())
}

/// Exécute toutes les vérifications (bloquant : à appeler hors du fil de l'interface).
pub fn run(layout: &Layout, service: Option<&ServiceInfo>) -> Vec<Check> {
    let mut out = Vec::new();

    // 1. Fichier de configuration
    let cfg: AppConfig = match store::load(&layout.config_file) {
        Ok(loaded) => {
            let errors: Vec<String> = loaded
                .issues
                .iter()
                .filter(|i| i.severity == Severity::Error)
                .map(|i| i.to_string())
                .collect();
            if errors.is_empty() {
                out.push(check(
                    Level::Ok,
                    "Configuration",
                    format!("« {} » est valide.", layout.config_file.display()),
                ));
            } else {
                out.push(check(
                    Level::Error,
                    "Configuration",
                    format!("Le moteur refusera de démarrer :\n{}", errors.join("\n")),
                ));
            }
            // Le mode sûr fait l'objet d'un constat dédié (état réel du moteur)
            for w in loaded
                .issues
                .iter()
                .filter(|i| i.severity == Severity::Warning && i.field != "safety.safe_mode")
            {
                out.push(check(Level::Warning, "Configuration", w.to_string()));
            }
            if let Some(v) = loaded.migrated_from {
                out.push(check(
                    Level::Info,
                    "Format de configuration",
                    format!(
                        "Fichier au format v{v}, lu comme v{} : enregistrez-le depuis l'onglet Configuration pour le mettre à jour.",
                        apex_config::CONFIG_VERSION
                    ),
                ));
            }
            loaded.config
        }
        Err(apex_config::ConfigError::Read { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            out.push(check(
                Level::Warning,
                "Configuration",
                format!(
                    "« {} » n'existe pas : le moteur utilisera les valeurs par défaut. Lancez l'assistant de configuration.",
                    layout.config_file.display()
                ),
            ));
            match layout.kind {
                LayoutKind::Portable => AppConfig::portable_default(),
                LayoutKind::System => AppConfig::default(),
            }
        }
        Err(e) => {
            out.push(check(Level::Error, "Configuration", e.to_string()));
            return out;
        }
    };
    let layout = layout.clone().with_overrides(&cfg);

    // 2. Serveur web et moteur
    let health: Option<Health> = match probe::health_detailed(&cfg, &layout) {
        Ok(h) => {
            out.push(check(
                Level::Ok,
                "Serveur web",
                format!(
                    "En ligne sur {} (version {}).",
                    probe::web_url(&cfg),
                    h.version
                ),
            ));
            if h.version != version::VERSION {
                out.push(check(
                    Level::Warning,
                    "Version",
                    format!(
                        "Le moteur en marche est en version {} et l'utilitaire en {} : redémarrez le moteur après une mise à jour.",
                        h.version,
                        version::VERSION
                    ),
                ));
            }
            Some(h)
        }
        Err(e) => {
            out.push(check(
                Level::Info,
                "Serveur web",
                format!("Injoignable : {e}."),
            ));
            None
        }
    };

    // 3. Port (si le moteur ne l'occupe pas déjà)
    if health.is_none() {
        if let Ok(ip) = cfg.server.bind.trim().parse::<std::net::IpAddr>() {
            match std::net::TcpListener::bind((ip, cfg.server.port)) {
                Ok(_) => out.push(check(
                    Level::Ok,
                    "Port web",
                    format!("Le port {} est libre sur {ip}.", cfg.server.port),
                )),
                Err(e) => out.push(check(
                    Level::Error,
                    "Port web",
                    format!(
                        "Le port {} est indisponible sur {ip} ({e}) : un autre programme l'utilise, ou l'adresse n'appartient pas à ce poste. Choisissez un autre port.",
                        cfg.server.port
                    ),
                )),
            }
        }
    }

    // 4. Configuration appliquée
    if let Some(rt) = health.as_ref().and_then(|h| h.runtime.as_ref()) {
        if !same_path(&rt.config_file, &layout.config_file) {
            out.push(check(
                Level::Warning,
                "Configuration appliquée",
                format!(
                    "Le moteur en marche utilise « {} » et non « {} ».",
                    rt.config_file,
                    layout.config_file.display()
                ),
            ));
        } else if rt.config_fingerprint != cfg.fingerprint() {
            out.push(check(
                Level::Warning,
                "Configuration appliquée",
                "Le fichier a été modifié depuis le démarrage du moteur : redémarrez-le pour appliquer les changements.",
            ));
        } else {
            out.push(check(
                Level::Ok,
                "Configuration appliquée",
                "Le moteur utilise la configuration du fichier.",
            ));
        }
        if rt.safe_mode {
            out.push(check(
                Level::Warning,
                "Mode sûr",
                "Actif : aucune commande n'est envoyée aux équipements réels.",
            ));
        }
        if rt.simulation_enabled {
            out.push(check(
                Level::Info,
                "Simulation",
                "Active : automate embarqué, simulateur GTB et données de démonstration.",
            ));
        }
    }

    if cfg.safety.safe_mode && health.as_ref().and_then(|h| h.runtime.as_ref()).is_none() {
        out.push(check(
            Level::Info,
            "Mode sûr",
            "Configuré : aucune commande ne sera envoyée aux équipements réels au prochain démarrage.",
        ));
    }

    // 5. IHM web
    let web_dir = layout.web_dir(&cfg);
    if web_dir.join("index.html").is_file() {
        out.push(check(
            Level::Ok,
            "IHM web",
            format!("Fichiers présents dans « {} ».", web_dir.display()),
        ));
    } else {
        out.push(check(
            Level::Error,
            "IHM web",
            format!(
                "« {} » ne contient pas index.html : les navigateurs recevront une erreur 404. Réinstallez le logiciel ou renseignez paths.web_dir.",
                web_dir.display()
            ),
        ));
    }

    // 6. Données
    let db = layout.database_file(&cfg);
    match std::fs::metadata(&db) {
        Ok(m) => out.push(check(
            Level::Ok,
            "Base de données",
            format!("« {} » ({}).", db.display(), human_size(m.len())),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => out.push(check(
            Level::Info,
            "Base de données",
            format!(
                "« {} » : accès réservé au service et aux administrateurs (normal pour une installation système).",
                db.display()
            ),
        )),
        Err(_) => out.push(check(
            Level::Info,
            "Base de données",
            format!("« {} » sera créée au premier démarrage.", db.display()),
        )),
    }
    if let Some(legacy) = find_legacy_db(&layout) {
        // En mode portable, --config désigne l'instance : sans lui, la commande viserait
        // l'installation système.
        let mut command = format!("apex-scada migrate-legacy --from \"{}\"", legacy.display());
        if layout.kind == LayoutKind::Portable {
            command.push_str(&format!(" --config \"{}\"", layout.config_file.display()));
        }
        out.push(check(
            Level::Info,
            "Ancienne version",
            format!(
                "Base d'une version antérieure trouvée (« {} »). Pour reprendre ses données, moteur arrêté : {}                  (ajoutez --force si une base existe déjà : elle sera sauvegardée puis remplacée).",
                legacy.display(),
                command
            ),
        ));
    }

    // 7. Journaux
    match logs::list(&layout.log_dir) {
        Ok(files) => {
            let problems = logs::recent_problems(&layout.log_dir, 1000);
            let errors = problems
                .iter()
                .filter(|l| l.contains(" ERROR ") || l.contains(" ERREUR "))
                .count();
            let level = if errors > 0 {
                Level::Warning
            } else {
                Level::Ok
            };
            out.push(check(
                level,
                "Journaux",
                format!(
                    "{} fichier(s) dans « {} » ; {} erreur(s) récente(s).",
                    files.len(),
                    layout.log_dir.display(),
                    errors
                ),
            ));
        }
        Err(e) => out.push(check(Level::Info, "Journaux", e)),
    }

    // 8. Service système
    if let Some(svc) = service {
        let (level, detail) = match (&svc.state, layout.kind) {
            (ServiceState::NotInstalled, LayoutKind::Portable) => {
                (Level::Info, "Non installé : normal en mode portable (lancement manuel).".to_string())
            }
            (ServiceState::NotInstalled, LayoutKind::System) => (
                Level::Warning,
                "Non installé : le moteur ne démarrera pas avec le système. Réinstallez le logiciel.".to_string(),
            ),
            (ServiceState::Running, _) if health.is_none() => (
                Level::Error,
                "Le service est démarré mais le serveur web ne répond pas : consultez les journaux.".to_string(),
            ),
            (state, _) => (
                Level::Ok,
                format!(
                    "{}{}",
                    capitalize(&state.label()),
                    match svc.automatic {
                        Some(true) => " — démarrage automatique",
                        Some(false) => " — démarrage manuel",
                        None => "",
                    }
                ),
            ),
        };
        out.push(check(level, "Service système", detail));
        if let Some(code) = svc.last_exit_code {
            out.push(check(
                Level::Warning,
                "Dernier arrêt du service",
                format!("Code {code} : {}.", apex_config::exit::describe(code)),
            ));
        }
    }

    // Les erreurs en tête, l'ordre logique des vérifications étant conservé (tri stable)
    out.sort_by_key(|c| c.level != Level::Error);
    out
}

fn same_path(a: &str, b: &Path) -> bool {
    let norm = |s: &str| s.replace('\\', "/").trim_end_matches('/').to_lowercase();
    if cfg!(windows) {
        norm(a) == norm(&b.to_string_lossy())
    } else {
        Path::new(a) == b
    }
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["o", "Ko", "Mo", "Go"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} o")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Rapport texte (copie dans le presse-papiers pour un ticket de support).
pub fn report(checks: &[Check], layout: &Layout) -> String {
    let mut s = format!(
        "Diagnostic {} {} — {}\nConfiguration : {}\n\n",
        version::PRODUCT_NAME,
        version::full_version(),
        chrono::Local::now().format("%d/%m/%Y %H:%M:%S"),
        layout.config_file.display()
    );
    for c in checks {
        let tag = match c.level {
            Level::Ok => "OK",
            Level::Info => "INFO",
            Level::Warning => "ATTENTION",
            Level::Error => "ERREUR",
        };
        s.push_str(&format!("[{tag}] {} : {}\n", c.title, c.detail));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_on_fresh_portable_layout() {
        let base = std::env::temp_dir().join(format!("apex-admin-diag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let layout = Layout::portable(&base);
        let checks = run(&layout, None);
        let cfg = checks.iter().find(|c| c.title == "Configuration").unwrap();
        assert_eq!(
            cfg.level,
            Level::Warning,
            "fichier absent signalé : {cfg:?}"
        );
        assert!(checks.iter().any(|c| c.title == "Serveur web"));
        assert!(checks
            .iter()
            .any(|c| c.title == "Base de données" && c.detail.contains("sera créée")));

        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(
            &layout.config_file,
            "config_version = 1\n[server]\nport = 0\n",
        )
        .unwrap();
        let checks = run(&layout, None);
        assert_eq!(
            checks[0].level,
            Level::Error,
            "les erreurs passent en tête : {checks:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(human_size(512), "512 o");
        assert_eq!(human_size(2048), "2.0 Ko");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 Mo");
    }
}
