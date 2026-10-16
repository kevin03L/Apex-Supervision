//! Pilotage du moteur depuis l'utilitaire : état du service système, actions (élevées si
//! nécessaire) et lancement en mode manuel. C'est la seule partie de l'utilitaire qui connaît
//! les mécanismes propres à Windows (SCM, UAC) et à Linux (systemd, polkit).
//!
//! Les actions privilégiées ne sont jamais exécutées par l'utilitaire lui-même : il délègue au
//! moteur (`apex-scada service …`) lancé avec élévation (Windows) ou à `systemctl` (Linux,
//! autorisation par polkit).

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use apex_config::{exit, version};

/// État du service système.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceState {
    NotInstalled,
    Stopped,
    Starting,
    Stopping,
    Running,
    /// État non interprété (pause, échec systemd...) : libellé brut.
    Other(String),
    /// Impossible d'interroger le gestionnaire de services.
    Unknown(String),
}

impl ServiceState {
    pub fn label(&self) -> String {
        match self {
            ServiceState::NotInstalled => "non installé".into(),
            ServiceState::Stopped => "arrêté".into(),
            ServiceState::Starting => "démarrage en cours".into(),
            ServiceState::Stopping => "arrêt en cours".into(),
            ServiceState::Running => "démarré".into(),
            ServiceState::Other(s) => s.clone(),
            ServiceState::Unknown(e) => format!("inconnu ({e})"),
        }
    }

    pub fn is_installed(&self) -> bool {
        !matches!(self, ServiceState::NotInstalled | ServiceState::Unknown(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceInfo {
    pub state: ServiceState,
    /// Démarrage automatique avec le système (`None` si inconnu).
    pub automatic: Option<bool>,
    /// Dernier code de sortie spécifique du moteur (Windows), s'il est non nul.
    pub last_exit_code: Option<i32>,
}

/// Chemin du moteur : installé à côté de l'utilitaire (même dossier).
pub fn engine_path() -> PathBuf {
    let name = format!("{}{}", version::ENGINE_BIN, std::env::consts::EXE_SUFFIX);
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(&name)))
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Version du moteur installé (`apex-scada version`).
pub fn engine_version(engine: &Path) -> Result<String, String> {
    let out = hidden(Command::new(engine).arg("version"))
        .output()
        .map_err(|e| format!("moteur « {} » introuvable : {e}", engine.display()))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// ============================================================================
// SERVICE SYSTÈME
// ============================================================================

#[cfg(windows)]
pub fn service_query() -> ServiceInfo {
    use windows_service::{
        service::{ServiceAccess, ServiceExitCode, ServiceStartType, ServiceState as Ws},
        service_manager::{ServiceManager, ServiceManagerAccess},
    };
    let unknown = |e: String| ServiceInfo {
        state: ServiceState::Unknown(e),
        automatic: None,
        last_exit_code: None,
    };
    let mgr = match ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT) {
        Ok(m) => m,
        Err(e) => return unknown(e.to_string()),
    };
    let service = match mgr.open_service(
        version::SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG,
    ) {
        Ok(s) => s,
        Err(windows_service::Error::Winapi(io)) if io.raw_os_error() == Some(1060) => {
            return ServiceInfo {
                state: ServiceState::NotInstalled,
                automatic: None,
                last_exit_code: None,
            }
        }
        Err(e) => return unknown(e.to_string()),
    };
    let status = match service.query_status() {
        Ok(s) => s,
        Err(e) => return unknown(e.to_string()),
    };
    let state = match status.current_state {
        Ws::Stopped => ServiceState::Stopped,
        Ws::StartPending => ServiceState::Starting,
        Ws::StopPending => ServiceState::Stopping,
        Ws::Running => ServiceState::Running,
        other => ServiceState::Other(format!("{other:?}")),
    };
    let automatic = service
        .query_config()
        .ok()
        .map(|c| matches!(c.start_type, ServiceStartType::AutoStart));
    let last_exit_code = match status.exit_code {
        ServiceExitCode::ServiceSpecific(c) if c != 0 => Some(c as i32),
        _ => None,
    };
    ServiceInfo {
        state,
        automatic,
        last_exit_code,
    }
}

#[cfg(not(windows))]
pub fn service_query() -> ServiceInfo {
    let out = Command::new("systemctl")
        .args([
            "show",
            &format!("{}.service", version::SERVICE_NAME),
            "--property=LoadState,ActiveState,SubState,UnitFileState",
        ])
        .output();
    let out = match out {
        Ok(o) => o,
        Err(e) => {
            return ServiceInfo {
                state: ServiceState::Unknown(format!("systemctl introuvable : {e}")),
                automatic: None,
                last_exit_code: None,
            }
        }
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let get = |k: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{k}=")))
            .unwrap_or_default()
            .to_string()
    };
    if get("LoadState") == "not-found" {
        return ServiceInfo {
            state: ServiceState::NotInstalled,
            automatic: None,
            last_exit_code: None,
        };
    }
    let state = match get("ActiveState").as_str() {
        "active" => ServiceState::Running,
        "inactive" => ServiceState::Stopped,
        "activating" => ServiceState::Starting,
        "deactivating" => ServiceState::Stopping,
        "" => ServiceState::Unknown(String::from_utf8_lossy(&out.stderr).trim().to_string()),
        other => ServiceState::Other(format!("{other} ({})", get("SubState"))),
    };
    let automatic = match get("UnitFileState").as_str() {
        "enabled" => Some(true),
        "disabled" => Some(false),
        _ => None,
    };
    ServiceInfo {
        state,
        automatic,
        last_exit_code: None,
    }
}

/// Exécute une action sur le service : `start`, `stop`, `restart`, `enable`, `disable`.
#[cfg(windows)]
pub fn service_action(action: &str) -> Result<String, String> {
    let (code, output) = run_privileged(&engine_path(), &["service", action])?;
    if code == 0 {
        Ok(action_done(action))
    } else if !output.is_empty() {
        Err(output)
    } else {
        Err(format!(
            "l'action « {action} » a échoué (code {code}) : consultez l'état du service et les journaux"
        ))
    }
}

/// Indique si l'utilitaire s'exécute avec les droits administrateur (accès en création au
/// gestionnaire de services, réservé aux administrateurs).
#[cfg(windows)]
pub fn is_elevated() -> bool {
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .is_ok()
}

#[cfg(not(windows))]
pub fn is_elevated() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

/// Exécute une commande d'administration du moteur. Déjà administrateur : exécution directe,
/// sortie récupérée. Sinon (Windows) : élévation UAC, seul le code de sortie est connu.
/// Renvoie le code de sortie et la sortie du programme (vide si inconnue).
pub fn run_privileged(exe: &Path, args: &[&str]) -> Result<(i32, String), String> {
    if is_elevated() || !CAN_RELAUNCH_ELEVATED {
        let out = hidden(Command::new(exe).args(args))
            .output()
            .map_err(|e| format!("« {} » introuvable : {e}", exe.display()))?;
        let mut text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if !err.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&err);
        }
        return Ok((out.status.code().unwrap_or(exit::FAILURE), text));
    }
    #[cfg(windows)]
    {
        run_elevated(exe, args, true).map(|code| (code, String::new()))
    }
    #[cfg(not(windows))]
    unreachable!()
}

#[cfg(not(windows))]
pub fn service_action(action: &str) -> Result<String, String> {
    // polkit demande l'authentification si nécessaire (règle fournie par le paquet pour le
    // groupe apex-scada)
    let out = Command::new("systemctl")
        .arg(action)
        .arg(format!("{}.service", version::SERVICE_NAME))
        .output()
        .map_err(|e| format!("systemctl introuvable : {e}"))?;
    if out.status.success() {
        Ok(action_done(action))
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(
            if err.contains("Interactive authentication required") || err.contains("Access denied")
            {
                "action refusée : ajoutez votre compte au groupe apex-scada (sudo usermod -aG apex-scada $USER), puis reconnectez-vous"
                .into()
            } else {
                format!("systemctl {action} a échoué : {err}")
            },
        )
    }
}

fn action_done(action: &str) -> String {
    match action {
        "start" => "Démarrage du service demandé.".into(),
        "stop" => "Arrêt du service demandé.".into(),
        "restart" => "Redémarrage du service demandé.".into(),
        "enable" => "Démarrage automatique activé.".into(),
        "disable" => "Démarrage automatique désactivé.".into(),
        other => format!("Action « {other} » effectuée."),
    }
}

// ============================================================================
// MODE MANUEL (portable)
// ============================================================================

/// Lance le moteur en arrière-plan, détaché de l'utilitaire (le fermer n'arrête pas le moteur).
/// Attend brièvement pour signaler un échec de démarrage immédiat (configuration, port...).
pub fn start_manual(config_file: &Path) -> Result<String, String> {
    let engine = engine_path();
    let mut cmd = Command::new(&engine);
    cmd.arg("run")
        .arg("--config")
        .arg(config_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(dir) = config_file.parent() {
        cmd.current_dir(dir);
    }
    detach(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("lancement de « {} » impossible : {e}", engine.display()))?;

    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            let code = status.code().unwrap_or(exit::FAILURE);
            let mut stderr = String::new();
            if let Some(mut e) = child.stderr.take() {
                use std::io::Read;
                let _ = e.read_to_string(&mut stderr);
            }
            let detail = stderr
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or_default()
                .trim()
                .to_string();
            return Err(format!(
                "le moteur s'est arrêté au démarrage (code {code} : {}){}",
                exit::describe(code),
                if detail.is_empty() {
                    String::new()
                } else {
                    format!("\n{detail}")
                }
            ));
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    // Le moteur continue seul : sa sortie d'erreur n'est plus lue (les journaux font foi)
    drop(child.stderr.take());
    Ok(format!("Moteur lancé (PID {}).", child.id()))
}

#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(windows))]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

/// Masque la console des commandes auxiliaires sous Windows.
fn hidden(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd
}

// ============================================================================
// ÉLÉVATION ET OUVERTURE DE FICHIERS
// ============================================================================

/// Exécute un programme avec élévation UAC et renvoie son code de sortie.
/// Si `wait` est faux, rend la main dès le lancement (code 0).
#[cfg(windows)]
pub fn run_elevated(exe: &Path, args: &[&str], wait: bool) -> Result<i32, String> {
    let ps_quote = |s: &str| format!("'{}'", s.replace('\'', "''"));
    // Chaque argument contenant un espace est entouré de guillemets pour la ligne de commande
    let arg_list = args
        .iter()
        .map(|a| {
            if a.contains(' ') {
                ps_quote(&format!("\"{a}\""))
            } else {
                ps_quote(a)
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    let mut script = format!(
        "$ErrorActionPreference='Stop'; $p = Start-Process -FilePath {} -Verb RunAs -WindowStyle Hidden -PassThru",
        ps_quote(&exe.to_string_lossy())
    );
    if !args.is_empty() {
        script.push_str(&format!(" -ArgumentList {arg_list}"));
    }
    if wait {
        script.push_str("; $p.WaitForExit(); exit $p.ExitCode");
    }
    let out = hidden(Command::new("powershell.exe").args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        &script,
    ]))
    .output()
    .map_err(|e| format!("PowerShell introuvable : {e}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !stderr.trim().is_empty() && (stderr.contains("annul") || stderr.contains("cancel")) {
        return Err("élévation refusée : l'action n'a pas été effectuée".into());
    }
    match out.status.code() {
        Some(code) if !stderr.trim().is_empty() && code != 0 && !wait => {
            Err(stderr.trim().to_string())
        }
        Some(code) => Ok(code),
        None => Err("élévation interrompue".into()),
    }
}

/// Relance l'utilitaire avec les droits administrateur (Windows).
#[cfg(windows)]
pub fn relaunch_elevated(args: &[String]) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_elevated(&exe, &refs, false).map(|_| ())
}

/// Ouvre un dossier ou un fichier avec l'application du système.
pub fn open_path(path: &Path) -> Result<(), String> {
    #[cfg(windows)]
    let mut cmd = Command::new("explorer.exe");
    #[cfg(not(windows))]
    let mut cmd = Command::new("xdg-open");
    cmd.arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("ouverture de « {} » impossible : {e}", path.display()))
}

/// Indique si le fichier de configuration peut être écrit par l'utilisateur courant
/// (création d'un fichier témoin dans son dossier).
pub fn config_dir_writable(config_file: &Path) -> bool {
    let Some(dir) = config_file.parent() else {
        return false;
    };
    if !dir.exists() {
        // Dossier absent : on remonte au premier parent existant
        return dir
            .ancestors()
            .find(|d| d.exists())
            .is_some_and(probe_write);
    }
    probe_write(dir)
        && (!config_file.exists()
            || std::fs::OpenOptions::new()
                .append(true)
                .open(config_file)
                .is_ok())
}

fn probe_write(dir: &Path) -> bool {
    let probe = dir.join(format!(".apex-admin-write-test-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// L'élévation à la demande n'existe que sous Windows (UAC) ; sous Linux, les droits passent
/// par le groupe apex-scada.
pub const CAN_RELAUNCH_ELEVATED: bool = cfg!(windows);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_is_expected_next_to_admin() {
        let p = engine_path();
        assert!(p
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(version::ENGINE_BIN));
    }

    #[test]
    fn writable_probe_leaves_no_file() {
        let dir = std::env::temp_dir().join(format!("apex-admin-w-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(config_dir_writable(&dir.join("apex-scada.toml")));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
