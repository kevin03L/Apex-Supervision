//! Ligne de commande de `apex-scada` (analyse manuelle, sans dépendance supplémentaire).

use std::path::PathBuf;

pub const USAGE: &str = "\
Apex Supervision — moteur GTC/SCADA et serveur web

UTILISATION :
  apex-scada [run] [--config <fichier>]
        Démarre le moteur au premier plan (Ctrl+C pour un arrêt propre).

  apex-scada config check [--config <fichier>]
        Valide la configuration et vérifie la disponibilité du port.
  apex-scada config show [--config <fichier>]
        Affiche la configuration effective et les emplacements utilisés.
  apex-scada config init [--portable] [--force] [--config <fichier>]
                         [--name <nom>] [--bind <ip>] [--port <n>] [--simulation] [--safe-mode]
        Crée un fichier de configuration (refusé s'il existe, sauf --force avec sauvegarde).
  apex-scada config migrate [--config <fichier>]
        Réécrit la configuration au format courant (valeurs conservées, sauvegarde préalable).

  apex-scada migrate-legacy --from <dossier|fichier.db> [--config <fichier>] [--force]
        Copie la base d'une version antérieure (rust_scada_flow.db). La source n'est jamais modifiée.

  apex-scada service install|uninstall|start|stop|restart|status|enable|disable  (Windows, administrateur)
        Gère le service Windows « Apex Supervision ». Sous Linux : systemctl … apex-scada.

  apex-scada version      Affiche la version.
  apex-scada help         Affiche cette aide.

CODES DE SORTIE :
  0 succès · 1 erreur · 2 usage · 3 configuration invalide · 4 instance déjà active
  5 adresse ou port indisponible · 6 base de données inaccessible
";

/// Codes de sortie documentés (exploités par l'utilitaire desktop et les scripts).
pub use apex_config::exit;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct InitOptions {
    pub portable: bool,
    pub force: bool,
    pub name: Option<String>,
    pub bind: Option<String>,
    pub port: Option<u16>,
    pub simulation: bool,
    pub safe_mode: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Run,
    ConfigCheck,
    ConfigShow,
    ConfigInit(InitOptions),
    ConfigMigrate,
    MigrateLegacy { from: PathBuf, force: bool },
    Service(String),
    Version,
    Help,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cli {
    pub command: Command,
    pub config: Option<PathBuf>,
}

pub fn parse(args: &[String]) -> Result<Cli, String> {
    let mut config: Option<PathBuf> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut init = InitOptions::default();
    let mut from: Option<PathBuf> = None;
    let mut force = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| -> Result<String, String> {
            it.next()
                .cloned()
                .ok_or_else(|| format!("l'option {flag} attend une valeur"))
        };
        match arg.as_str() {
            "--config" | "-c" => config = Some(PathBuf::from(value("--config")?)),
            "--from" => from = Some(PathBuf::from(value("--from")?)),
            "--name" => init.name = Some(value("--name")?),
            "--bind" => init.bind = Some(value("--bind")?),
            "--port" => {
                let raw = value("--port")?;
                init.port = Some(
                    raw.parse::<u16>()
                        .map_err(|_| format!("port invalide « {raw} »"))?,
                )
            }
            "--portable" => init.portable = true,
            "--simulation" => init.simulation = true,
            "--safe-mode" => init.safe_mode = true,
            "--force" => force = true,
            "-h" | "--help" => positional.push("help".into()),
            "-V" | "--version" => positional.push("version".into()),
            other if other.starts_with('-') => return Err(format!("option inconnue « {other} »")),
            other => positional.push(other.to_string()),
        }
    }
    init.force = force;

    let words: Vec<&str> = positional.iter().map(String::as_str).collect();
    let command = match words.as_slice() {
        [] | ["run"] => Command::Run,
        ["config", "check"] => Command::ConfigCheck,
        ["config", "show"] => Command::ConfigShow,
        ["config", "init"] => Command::ConfigInit(init),
        ["config", "migrate"] => Command::ConfigMigrate,
        ["migrate-legacy"] => Command::MigrateLegacy {
            from: from.ok_or("migrate-legacy exige --from <dossier|fichier.db>")?,
            force,
        },
        ["service", action] => Command::Service(action.to_string()),
        ["version"] => Command::Version,
        ["help"] => Command::Help,
        _ => return Err(format!("commande inconnue « {} »", words.join(" "))),
    };
    Ok(Cli { command, config })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Cli, String> {
        parse(&s.split_whitespace().map(String::from).collect::<Vec<_>>())
    }

    #[test]
    fn default_is_run() {
        assert_eq!(p("").unwrap().command, Command::Run);
        assert_eq!(
            p("run --config /etc/x.toml").unwrap().config,
            Some(PathBuf::from("/etc/x.toml"))
        );
    }

    #[test]
    fn config_init_options() {
        let cli =
            p("config init --portable --port 8080 --bind 0.0.0.0 --simulation --force").unwrap();
        let Command::ConfigInit(o) = cli.command else {
            panic!()
        };
        assert!(o.portable && o.simulation && o.force);
        assert_eq!(o.port, Some(8080));
        assert_eq!(o.bind.as_deref(), Some("0.0.0.0"));
    }

    #[test]
    fn errors_are_explicit() {
        assert!(p("config init --port abc")
            .unwrap_err()
            .contains("port invalide"));
        assert!(p("--inconnue").unwrap_err().contains("option inconnue"));
        assert!(p("migrate-legacy").unwrap_err().contains("--from"));
        assert!(p("demarrer").unwrap_err().contains("commande inconnue"));
    }

    #[test]
    fn service_and_version() {
        assert_eq!(
            p("service install").unwrap().command,
            Command::Service("install".into())
        );
        assert_eq!(p("--version").unwrap().command, Command::Version);
    }
}
