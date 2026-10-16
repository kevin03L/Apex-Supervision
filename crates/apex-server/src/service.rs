//! Intégration au gestionnaire de services Windows (SCM).
//!
//! - `apex-scada service run` : point d'entrée appelé par le SCM (ne pas lancer à la main) ;
//! - `apex-scada service install|uninstall|start|stop|status` : administration (droits administrateur).
//!
//! Le service s'exécute sous le compte **LocalService** (droits minimaux). L'installation
//! accorde à ce compte l'écriture sur les seuls répertoires `data`, `logs` et `run`, et réserve
//! `run` (jeton de contrôle local) et `data` (base) aux administrateurs et au service.

use std::{
    ffi::OsString,
    path::PathBuf,
    sync::OnceLock,
    time::Duration,
};

use apex_config::{version, Layout};
use tokio::sync::watch;
use windows_service::{
    define_windows_service,
    service::{
        ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
        ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
    service_manager::{ServiceManager, ServiceManagerAccess},
};

use crate::app::{self, RunMode};

const DISPLAY_NAME: &str = "Apex Supervision";
const DESCRIPTION: &str =
    "Moteur GTC/SCADA et serveur web de supervision Apex Supervision (communications industrielles, alarmes, historique).";

/// SID des comptes et groupes (indépendants de la langue de Windows).
const SID_SYSTEM: &str = "*S-1-5-18";
const SID_LOCAL_SERVICE: &str = "*S-1-5-19";
const SID_ADMINISTRATORS: &str = "*S-1-5-32-544";
const SID_USERS: &str = "*S-1-5-32-545";

static SERVICE_CONFIG: OnceLock<Option<PathBuf>> = OnceLock::new();

define_windows_service!(ffi_service_main, service_main);

/// Lance le dispatcher SCM (bloquant jusqu'à l'arrêt du service).
pub fn run_dispatcher(config: Option<PathBuf>) -> Result<(), String> {
    let _ = SERVICE_CONFIG.set(config);
    service_dispatcher::start(version::SERVICE_NAME, ffi_service_main).map_err(|e| {
        format!(
            "{e} — « service run » est réservé au gestionnaire de services Windows ; \
             pour un lancement manuel, utilisez « apex-scada run »"
        )
    })
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = run_service() {
        crate::logging::startup_error(&Layout::system().log_dir, &format!("service Windows : {e}"));
    }
}

fn run_service() -> windows_service::Result<()> {
    let (stop_tx, stop_rx) = watch::channel(false);
    let handler = move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown | ServiceControl::Preshutdown => {
            let _ = stop_tx.send(true);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status = service_control_handler::register(version::SERVICE_NAME, handler)?;
    let set = |state: ServiceState, accept: ServiceControlAccept, exit: ServiceExitCode, wait: u64| {
        let _ = status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: exit,
            checkpoint: 0,
            wait_hint: Duration::from_secs(wait),
            process_id: None,
        });
    };
    set(ServiceState::StartPending, ServiceControlAccept::empty(), ServiceExitCode::Win32(0), 60);

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            crate::logging::startup_error(&Layout::system().log_dir, &format!("runtime Tokio : {e}"));
            set(ServiceState::Stopped, ServiceControlAccept::empty(), ServiceExitCode::ServiceSpecific(1), 0);
            return Ok(());
        }
    };
    let config = SERVICE_CONFIG.get().cloned().flatten();
    let result = runtime.block_on(app::run(config, RunMode::Service, stop_rx, || {
        set(
            ServiceState::Running,
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            ServiceExitCode::Win32(0),
            0,
        );
    }));
    set(ServiceState::StopPending, ServiceControlAccept::empty(), ServiceExitCode::Win32(0), 15);
    drop(runtime);

    let exit = match result {
        Ok(()) => ServiceExitCode::Win32(0),
        // Code spécifique : visible dans le journal des événements et lu par l'utilitaire desktop
        Err(e) => ServiceExitCode::ServiceSpecific(e.code as u32),
    };
    set(ServiceState::Stopped, ServiceControlAccept::empty(), exit, 0);
    Ok(())
}

/// Administration du service : install, uninstall, start, stop, restart, status, enable, disable.
pub fn manage(action: &str, config: Option<PathBuf>) -> Result<String, String> {
    match action {
        "install" => install(config),
        "uninstall" => uninstall(),
        "start" => start(),
        "stop" => stop(),
        "restart" => restart(),
        "status" => status(),
        "enable" => set_start_type(true),
        "disable" => set_start_type(false),
        "run" => run_dispatcher(config).map(|_| String::new()),
        other => Err(format!(
            "action de service inconnue « {other} » (install, uninstall, start, stop, restart, status, enable, disable)"
        )),
    }
}

fn manager(access: ServiceManagerAccess) -> Result<ServiceManager, String> {
    ServiceManager::local_computer(None::<&str>, access).map_err(|e| {
        format!("accès au gestionnaire de services impossible : {}", explain(&e))
    })
}

fn install(config: Option<PathBuf>) -> Result<String, String> {
    // Droits vérifiés AVANT de toucher au disque : sans élévation, aucun répertoire système
    // ne doit être créé (il appartiendrait à un utilisateur standard, maître de ses ACL).
    let mgr = manager(ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
    let layout = Layout::system();
    prepare_directories(&layout)?;

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut launch_arguments = vec![OsString::from("service"), OsString::from("run")];
    if let Some(cfg) = &config {
        launch_arguments.push("--config".into());
        launch_arguments.push(cfg.clone().into_os_string());
    }
    let info = ServiceInfo {
        name: OsString::from(version::SERVICE_NAME),
        display_name: OsString::from(DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        launch_arguments,
        dependencies: vec![],
        account_name: Some(OsString::from(r"NT AUTHORITY\LocalService")),
        account_password: None,
    };
    let service = mgr
        .create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::QUERY_STATUS)
        .map_err(|e| format!("création du service impossible : {}", explain(&e)))?;
    let _ = service.set_description(DESCRIPTION);
    // Redémarrage automatique après un échec (10 s, 3 fois, compteur remis à zéro chaque jour)
    let _ = service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(86_400)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(10) },
            ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(30) },
            ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(60) },
        ]),
    });
    let _ = service.set_failure_actions_on_non_crash_failures(true);
    Ok(format!(
        "Service « {} » installé (démarrage automatique, compte LocalService).",
        version::SERVICE_NAME
    ))
}

/// Crée les répertoires système et applique les permissions du service.
pub fn prepare_directories(layout: &Layout) -> Result<(), String> {
    let config_dir = layout
        .config_file
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    for dir in [&config_dir, &layout.backup_dir, &layout.data_dir, &layout.log_dir, &layout.run_dir] {
        std::fs::create_dir_all(dir).map_err(|e| format!("création de « {} » impossible : {e}", dir.display()))?;
    }
    let grant_service = format!("{SID_LOCAL_SERVICE}:(OI)(CI)M");
    let full = |sid: &str| format!("{sid}:(OI)(CI)F");
    // Journaux : écriture par le service, lecture par les utilisateurs (utilitaire desktop)
    icacls(&layout.log_dir, &["/grant", &grant_service])?;
    // Données et jeton : réservés au système, aux administrateurs et au service
    for dir in [&layout.data_dir, &layout.run_dir] {
        icacls(
            dir,
            &[
                "/inheritance:r",
                "/grant:r",
                &full(SID_SYSTEM),
                &full(SID_ADMINISTRATORS),
                &grant_service,
            ],
        )?;
    }
    // Configuration : lecture par le service et les utilisateurs, écriture par les administrateurs
    icacls(&config_dir, &["/grant", &format!("{SID_USERS}:(OI)(CI)RX"), &format!("{SID_LOCAL_SERVICE}:(OI)(CI)RX")])?;
    Ok(())
}

fn icacls(dir: &std::path::Path, args: &[&str]) -> Result<(), String> {
    let out = std::process::Command::new("icacls")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("icacls introuvable : {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "permissions de « {} » non appliquées : {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn uninstall() -> Result<String, String> {
    let mgr = manager(ServiceManagerAccess::CONNECT)?;
    let service = mgr
        .open_service(
            version::SERVICE_NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
        )
        .map_err(|e| explain(&e))?;
    if service.query_status().map(|s| s.current_state != ServiceState::Stopped).unwrap_or(false) {
        let _ = service.stop();
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(500));
            if service.query_status().map(|s| s.current_state == ServiceState::Stopped).unwrap_or(true) {
                break;
            }
        }
    }
    service.delete().map_err(|e| format!("suppression du service impossible : {}", explain(&e)))?;
    Ok(format!(
        "Service « {} » supprimé. La configuration, les données et les journaux sont conservés.",
        version::SERVICE_NAME
    ))
}

fn start() -> Result<String, String> {
    let mgr = manager(ServiceManagerAccess::CONNECT)?;
    let service = mgr
        .open_service(version::SERVICE_NAME, ServiceAccess::START)
        .map_err(|e| explain(&e))?;
    service
        .start::<&str>(&[])
        .map_err(|e| format!("démarrage impossible : {}", explain(&e)))?;
    Ok("Démarrage du service demandé.".into())
}

fn stop() -> Result<String, String> {
    let mgr = manager(ServiceManagerAccess::CONNECT)?;
    let service = mgr
        .open_service(version::SERVICE_NAME, ServiceAccess::STOP)
        .map_err(|e| explain(&e))?;
    service.stop().map_err(|e| format!("arrêt impossible : {}", explain(&e)))?;
    Ok("Arrêt du service demandé.".into())
}

fn status() -> Result<String, String> {
    let mgr = manager(ServiceManagerAccess::CONNECT)?;
    let service = mgr
        .open_service(version::SERVICE_NAME, ServiceAccess::QUERY_STATUS)
        .map_err(|e| explain(&e))?;
    let st = service.query_status().map_err(|e| explain(&e))?;
    let exit = match st.exit_code {
        ServiceExitCode::Win32(0) => String::new(),
        ServiceExitCode::Win32(c) => format!(" — code Windows {c}"),
        ServiceExitCode::ServiceSpecific(c) => format!(" — code de sortie {c}"),
    };
    Ok(format!("Service « {} » : {:?}{exit}", version::SERVICE_NAME, st.current_state))
}

/// Traduit les erreurs du gestionnaire de services en message exploitable.
fn explain(e: &windows_service::Error) -> String {
    let windows_service::Error::Winapi(io) = e else {
        return e.to_string();
    };
    match io.raw_os_error() {
        Some(5) => "accès refusé : cette action exige les droits administrateur".into(),
        Some(1056) => "le service est déjà démarré".into(),
        Some(1060) => format!("le service « {} » n'est pas installé", version::SERVICE_NAME),
        Some(1062) => "le service n'est pas démarré".into(),
        Some(1072) => "le service est marqué pour suppression : fermez la console Services puis réessayez".into(),
        Some(1073) => format!("le service « {} » existe déjà", version::SERVICE_NAME),
        _ => io.to_string(),
    }
}

/// Arrête puis redémarre le service (une seule élévation pour l'utilitaire desktop).
fn restart() -> Result<String, String> {
    let mgr = manager(ServiceManagerAccess::CONNECT)?;
    let service = mgr
        .open_service(
            version::SERVICE_NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::START,
        )
        .map_err(|e| explain(&e))?;
    let is_stopped = || {
        service
            .query_status()
            .map(|s| s.current_state == ServiceState::Stopped)
            .unwrap_or(false)
    };
    if !is_stopped() {
        service.stop().map_err(|e| format!("arrêt impossible : {}", explain(&e)))?;
        // L'arrêt propre (drivers, historique) prend quelques secondes
        let mut stopped = false;
        for _ in 0..60 {
            std::thread::sleep(Duration::from_millis(500));
            if is_stopped() {
                stopped = true;
                break;
            }
        }
        if !stopped {
            return Err("le service ne s'est pas arrêté en 30 s : redémarrage abandonné".into());
        }
    }
    service
        .start::<&str>(&[])
        .map_err(|e| format!("démarrage impossible : {}", explain(&e)))?;
    Ok("Redémarrage du service demandé.".into())
}

/// Démarrage automatique avec Windows (`enable`) ou manuel (`disable`).
/// `sc.exe` est utilisé car il modifie le seul type de démarrage, sans réécrire la ligne de
/// commande du service.
fn set_start_type(automatic: bool) -> Result<String, String> {
    let out = std::process::Command::new("sc.exe")
        .args([
            "config",
            version::SERVICE_NAME,
            "start=",
            if automatic { "auto" } else { "demand" },
        ])
        .output()
        .map_err(|e| format!("sc.exe introuvable : {e}"))?;
    if out.status.success() {
        Ok(if automatic {
            "Démarrage automatique du service activé.".into()
        } else {
            "Démarrage automatique du service désactivé (démarrage manuel).".into()
        })
    } else {
        Err(match out.status.code() {
            Some(5) => "accès refusé : cette action exige les droits administrateur".into(),
            Some(1060) => format!("le service « {} » n'est pas installé", version::SERVICE_NAME),
            _ => format!(
                "modification du démarrage impossible : {}",
                String::from_utf8_lossy(&out.stdout).trim()
            ),
        })
    }
}
