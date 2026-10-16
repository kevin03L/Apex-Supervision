//! # apex-scada — moteur GTC/SCADA et serveur web d'Apex Supervision
//!
//! Point d'entrée : analyse de la ligne de commande puis exécution de la commande demandée.
//! Le démarrage et l'arrêt du moteur sont décrits dans [`app`] ; la ligne de commande dans [`cli`].

mod app;
mod cli;
mod db;
mod http;
mod instance;
mod legacy;
mod logging;
#[cfg(windows)]
mod service;

use std::path::Path;

use apex_config::{has_errors, store, version, AppConfig, Layout, LayoutKind, Severity};
use cli::{exit, Command, InitOptions};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match cli::parse(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Erreur : {e}\n\n{}", cli::USAGE);
            std::process::exit(exit::USAGE);
        }
    };
    let code = match cli.command {
        Command::Run => run_foreground(cli.config.as_deref()),
        Command::ConfigCheck => config_check(cli.config.as_deref()),
        Command::ConfigShow => config_show(cli.config.as_deref()),
        Command::ConfigInit(opts) => config_init(cli.config.as_deref(), opts),
        Command::ConfigMigrate => config_migrate(cli.config.as_deref()),
        Command::MigrateLegacy { from, force } => migrate_legacy(cli.config.as_deref(), &from, force),
        Command::Service(action) => manage_service(&action, cli.config.as_deref()),
        Command::Version => {
            println!("{} {}", version::PRODUCT_NAME, version::full_version());
            exit::OK
        }
        Command::Help => {
            println!("{}", cli::USAGE);
            exit::OK
        }
    };
    std::process::exit(code);
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("initialisation du runtime Tokio")
}

/// Exécution au premier plan (mode manuel ou unité systemd).
fn run_foreground(config: Option<&Path>) -> i32 {
    // Sous systemd, le moteur est considéré comme un service (journal, redémarrage)
    let mode = if std::env::var_os("INVOCATION_ID").is_some() {
        app::RunMode::Service
    } else {
        app::RunMode::Manual
    };
    let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    match runtime().block_on(app::run(config.map(Path::to_path_buf), mode, stop_rx, || {})) {
        Ok(()) => exit::OK,
        Err(e) => {
            eprintln!("Démarrage impossible : {}", e.message);
            e.code
        }
    }
}

fn config_check(config: Option<&Path>) -> i32 {
    let eff = match app::load_effective(config) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{}", e.message);
            return e.code;
        }
    };
    println!("Fichier : {}{}", eff.layout.config_file.display(), if eff.file_exists { "" } else { " (absent : valeurs par défaut)" });
    if let Some(v) = eff.migrated_from {
        println!("Format v{v} : sera migré vers v{} à l'enregistrement.", apex_config::CONFIG_VERSION);
    }
    for o in &eff.env_overrides {
        println!("Surcharge d'environnement : {o}");
    }
    let mut issues = eff.config.validate();
    // Disponibilité du port (sauf s'il est tenu par une instance Apex déjà en marche)
    if let Ok(ip) = eff.config.server.bind.trim().parse::<std::net::IpAddr>() {
        if std::net::TcpListener::bind((ip, eff.config.server.port)).is_err() {
            issues.push(apex_config::Issue {
                severity: Severity::Warning,
                field: "server.port".into(),
                message: format!(
                    "le port {} est déjà utilisé sur {} (par une instance en marche ou un autre programme)",
                    eff.config.server.port, ip
                ),
            });
        }
    }
    if issues.is_empty() {
        println!("Configuration valide.");
    }
    for i in &issues {
        println!("{i}");
    }
    if has_errors(&issues) {
        exit::CONFIG
    } else {
        exit::OK
    }
}

fn config_show(config: Option<&Path>) -> i32 {
    match app::load_effective(config) {
        Ok(eff) => {
            let l = &eff.layout;
            println!("# Mode : {}", if l.kind == LayoutKind::System { "installation système" } else { "portable" });
            println!("# Configuration : {}", l.config_file.display());
            println!("# Données       : {}", l.data_dir.display());
            println!("# Base          : {}", l.database_file(&eff.config).display());
            println!("# Journaux      : {}", l.log_dir.display());
            println!("# IHM web       : {}", l.web_dir(&eff.config).display());
            println!("# Sauvegardes   : {}", l.backup_dir.display());
            println!("# Empreinte     : {}\n", eff.config.fingerprint());
            print!("{}", store::render(&eff.config));
            exit::OK
        }
        Err(e) => {
            eprintln!("{}", e.message);
            e.code
        }
    }
}

fn config_init(config: Option<&Path>, opts: InitOptions) -> i32 {
    let layout = match (config, opts.portable) {
        (Some(f), _) => Layout::for_config_file(f),
        (None, true) => Layout::portable(&std::env::current_dir().unwrap_or_else(|_| ".".into())),
        (None, false) => Layout::system(),
    };
    let existing_hash = store::file_hash(&layout.config_file);
    if existing_hash.is_some() && !opts.force {
        eprintln!(
            "« {} » existe déjà : utilisez --force pour le remplacer (une sauvegarde sera faite).",
            layout.config_file.display()
        );
        return exit::FAILURE;
    }
    let mut cfg = if layout.kind == LayoutKind::Portable || opts.simulation {
        AppConfig::portable_default()
    } else {
        AppConfig::default()
    };
    if let Some(n) = opts.name {
        cfg.instance.name = n;
    }
    if let Some(b) = opts.bind {
        cfg.server.bind = b;
    }
    if let Some(p) = opts.port {
        cfg.server.port = p;
    }
    cfg.safety.safe_mode = opts.safe_mode;
    match store::save(&layout, &cfg, existing_hash) {
        Ok(report) => {
            println!("Configuration écrite : {}", layout.config_file.display());
            if let Some(b) = report.backup {
                println!("Ancienne configuration sauvegardée : {}", b.display());
            }
            for i in cfg.validate() {
                println!("{i}");
            }
            exit::OK
        }
        Err(e) => {
            eprintln!("Écriture impossible : {e}");
            exit::CONFIG
        }
    }
}

/// Réécrit le fichier au format courant : valeurs du fichier conservées (sans les surcharges
/// d'environnement), ancienne version sauvegardée.
fn config_migrate(config: Option<&Path>) -> i32 {
    let layout = apex_config::Layout::resolve(config);
    let loaded = match store::load(&layout.config_file) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{e}");
            return exit::CONFIG;
        }
    };
    let Some(from) = loaded.migrated_from else {
        println!(
            "« {} » est déjà au format v{} : rien à faire.",
            layout.config_file.display(),
            apex_config::CONFIG_VERSION
        );
        return exit::OK;
    };
    match store::save(&layout, &loaded.config, loaded.file_hash) {
        Ok(report) => {
            println!(
                "Configuration migrée du format v{from} vers v{} : {}",
                apex_config::CONFIG_VERSION,
                layout.config_file.display()
            );
            if let Some(b) = report.backup {
                println!("Version précédente sauvegardée : {}", b.display());
            }
            exit::OK
        }
        Err(e) => {
            eprintln!("Migration impossible : {e}");
            exit::CONFIG
        }
    }
}

fn migrate_legacy(config: Option<&Path>, from: &Path, force: bool) -> i32 {
    let eff = match app::load_effective(config) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{}", e.message);
            return e.code;
        }
    };
    let Some(source) = legacy::find_legacy_db(from) else {
        eprintln!(
            "Aucune base « {} » trouvée dans « {} ».",
            legacy::LEGACY_DB_FILE,
            from.display()
        );
        return exit::FAILURE;
    };
    // Le moteur doit être arrêté : on prend le verrou d'instance le temps de la copie
    let _lock = match instance::InstanceLock::acquire(&eff.layout.lock_file()) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{e} : arrêtez le moteur avant la migration.");
            return exit::ALREADY_RUNNING;
        }
    };
    match runtime().block_on(legacy::migrate_database(&source, &eff.layout, &eff.config, force)) {
        Ok(r) => {
            println!("Base copiée : {} → {}", r.source.display(), r.destination.display());
            if let Some(b) = r.previous_destination_backup {
                println!("Base précédente conservée sous : {}", b.display());
            }
            for (table, n) in r.counts {
                println!("  {table:<16} {n} ligne(s)");
            }
            println!("La base d'origine n'a pas été modifiée.");
            exit::OK
        }
        Err(e) => {
            eprintln!("Migration impossible : {e}");
            exit::FAILURE
        }
    }
}

#[cfg(windows)]
fn manage_service(action: &str, config: Option<&Path>) -> i32 {
    match service::manage(action, config.map(Path::to_path_buf)) {
        Ok(msg) => {
            if !msg.is_empty() {
                println!("{msg}");
            }
            exit::OK
        }
        Err(e) => {
            eprintln!("{e}");
            exit::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn manage_service(action: &str, _config: Option<&Path>) -> i32 {
    let systemctl = match action {
        "start" | "stop" | "status" | "restart" | "enable" | "disable" => {
            format!("systemctl {action} {}", version::SERVICE_NAME)
        }
        "install" | "uninstall" => {
            "le service est installé et supprimé par le paquet (apt install / apt remove apex-supervision)".into()
        }
        _ => format!("action inconnue « {action} »"),
    };
    eprintln!("Sous Linux, le service est géré par systemd : {systemctl}");
    exit::USAGE
}
