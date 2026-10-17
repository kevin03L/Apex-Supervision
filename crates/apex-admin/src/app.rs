//! Fenêtre principale de l'utilitaire : état du moteur, configuration, journaux, diagnostics,
//! sauvegardes et informations de version.
//!
//! Toute opération potentiellement lente (sonde HTTP, gestionnaire de services, diagnostics,
//! démarrage / arrêt) s'exécute dans un fil séparé ; l'interface reste réactive et reçoit les
//! résultats par un canal.

use std::{
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

use apex_config::{store, version, AppConfig, ConfigError, Layout, LayoutKind};
use eframe::egui::{self, RichText};

use crate::{
    control::{self, ServiceInfo, ServiceState},
    diag, form, logs,
    probe::{self, Health},
    wizard::{self, Wizard},
};

/// Intervalle de rafraîchissement de l'état du moteur.
const POLL_EVERY: Duration = Duration::from_secs(3);

/// Taille maximale lue à la fin d'un journal.
const LOG_TAIL_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Default)]
pub struct Args {
    pub config: Option<PathBuf>,
    pub wizard: bool,
}

/// Arguments d'une relance avec élévation : toujours le chemin ABSOLU du fichier administré
/// (un processus élevé démarre en général dans System32).
fn relaunch_args(config_file: &std::path::Path, wizard: bool) -> Vec<String> {
    let mut v = vec!["--config".to_string(), config_file.display().to_string()];
    if wizard {
        v.push("--wizard".into());
    }
    v
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Status,
    Config,
    Logs,
    Diagnostics,
    Backups,
    About,
}

impl Tab {
    const ALL: [Tab; 6] = [
        Tab::Status,
        Tab::Config,
        Tab::Logs,
        Tab::Diagnostics,
        Tab::Backups,
        Tab::About,
    ];

    fn label(self) -> &'static str {
        match self {
            Tab::Status => "État du moteur",
            Tab::Config => "Configuration",
            Tab::Logs => "Journaux",
            Tab::Diagnostics => "Diagnostics",
            Tab::Backups => "Sauvegardes",
            Tab::About => "À propos",
        }
    }
}

struct Snapshot {
    health: Result<Health, String>,
    /// Configuration et emplacements sur lesquels le moteur a répondu (adresse réellement
    /// utilisée par le moteur en marche, qui peut différer du fichier après une modification).
    target: Option<(AppConfig, Layout)>,
    service: ServiceInfo,
    problems: Vec<String>,
    at: Instant,
}

enum Msg {
    Status(Box<Snapshot>),
    Done {
        what: String,
        result: Result<String, String>,
    },
    Diag(Vec<diag::Check>),
    EngineVersion(Result<String, String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Ok,
    Info,
    Warn,
    Error,
}

#[derive(Clone)]
struct Notice {
    kind: Kind,
    text: String,
    at: chrono::DateTime<chrono::Local>,
}

/// État du fichier de configuration sur disque.
struct Disk {
    config: AppConfig,
    hash: Option<u64>,
    exists: bool,
    migrated_from: Option<u32>,
    error: Option<String>,
}

impl Disk {
    fn load(layout: &Layout) -> Self {
        let default = match layout.kind {
            LayoutKind::Portable => AppConfig::portable_default(),
            LayoutKind::System => AppConfig::default(),
        };
        let exists = layout.config_file.exists();
        match store::load_or(&layout.config_file, default.clone()) {
            Ok(l) => Self {
                config: l.config,
                hash: l.file_hash,
                exists,
                migrated_from: l.migrated_from,
                error: None,
            },
            // Fichier illisible : valeurs par défaut affichées ; un enregistrement le remplacera
            // (après sauvegarde) grâce à l'empreinte du contenu actuel.
            Err(e) => Self {
                config: default,
                hash: store::file_hash(&layout.config_file),
                exists,
                migrated_from: None,
                error: Some(e.to_string()),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EngineState {
    Online,
    ServiceNoWeb,
    Starting,
    Stopping,
    Stopped,
    Unknown,
}

enum Confirm {
    Stop,
    Restart,
    Restore(PathBuf),
    QuitUnsaved,
    /// Fermeture demandée pendant une action (arrêt, redémarrage...) encore en cours.
    QuitBusy(String),
}

#[derive(Default)]
struct LogsView {
    files: Vec<logs::LogFile>,
    selected: Option<PathBuf>,
    content: String,
    error: Option<String>,
    problems_only: bool,
    follow: bool,
    loaded_at: Option<Instant>,
}

#[derive(Default)]
struct BackupsView {
    list: Vec<PathBuf>,
    selected: Option<PathBuf>,
    preview: Option<Result<(String, Vec<String>), String>>,
}

pub struct AdminApp {
    args: Args,
    /// Emplacements résolus (fichier de configuration, sauvegardes).
    base_layout: Layout,
    /// Emplacements effectifs (avec les répertoires personnalisés du fichier).
    layout: Layout,
    disk: Disk,
    edit: AppConfig,
    writable: bool,
    elevated: bool,
    tab: Tab,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    snapshot: Option<Snapshot>,
    probing: bool,
    /// Dernière cible sur laquelle le moteur a répondu (voir `Snapshot::target`).
    running: Option<(AppConfig, Layout)>,
    last_probe: Option<Instant>,
    busy: Option<String>,
    notices: Vec<Notice>,
    alert: Option<Notice>,
    port_check: Option<(String, u16, Result<(), String>)>,
    logs: LogsView,
    diag: Option<Vec<diag::Check>>,
    diag_running: bool,
    backups: BackupsView,
    engine_version: Option<Result<String, String>>,
    wizard: Option<Wizard>,
    confirm: Option<Confirm>,
    allow_close: bool,
}

impl AdminApp {
    pub fn new(cc: &eframe::CreationContext<'_>, args: Args) -> Self {
        let base_layout = Layout::resolve(args.config.as_deref());
        let disk = Disk::load(&base_layout);
        let layout = base_layout.clone().with_overrides(&disk.config);
        let (tx, rx) = mpsc::channel();
        let writable = can_write_config(&base_layout);
        let mut app = Self {
            edit: disk.config.clone(),
            args,
            base_layout,
            layout,
            disk,
            writable,
            elevated: control::is_elevated(),
            tab: Tab::Status,
            tx,
            rx,
            snapshot: None,
            probing: false,
            running: None,
            last_probe: None,
            busy: None,
            notices: Vec::new(),
            alert: None,
            port_check: None,
            logs: LogsView {
                problems_only: false,
                follow: true,
                ..Default::default()
            },
            diag: None,
            diag_running: false,
            backups: BackupsView::default(),
            engine_version: None,
            wizard: None,
            confirm: None,
            allow_close: false,
        };
        if !app.installed() {
            app.notify(
                Kind::Warn,
                format!(
                    "Aucune installation détectée (« {} » absent) : installez Apex Supervision, \
                     ou administrez une instance portable avec --config <fichier>.",
                    app.base_layout
                        .config_file
                        .parent()
                        .map(|d| d.display().to_string())
                        .unwrap_or_default()
                ),
            );
            app.alert = app.notices.last().cloned();
        } else if app.args.wizard || (!app.disk.exists && app.disk.error.is_none()) {
            app.open_wizard();
        }
        let ctx = cc.egui_ctx.clone();
        app.spawn(&ctx, || {
            Msg::EngineVersion(control::engine_version(&control::engine_path()))
        });
        app.refresh_backups();
        app
    }

    /// Instance installée (mode portable, ou dossier de configuration système présent).
    fn installed(&self) -> bool {
        self.base_layout.kind == LayoutKind::Portable
            || self
                .base_layout
                .config_file
                .parent()
                .is_some_and(|d| d.is_dir())
    }

    // ------------------------------------------------------------------------
    // Tâches de fond
    // ------------------------------------------------------------------------

    fn spawn(&self, ctx: &egui::Context, job: impl FnOnce() -> Msg + Send + 'static) {
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(job());
            ctx.request_repaint();
        });
    }

    fn action(
        &mut self,
        ctx: &egui::Context,
        what: &str,
        job: impl FnOnce() -> Result<String, String> + Send + 'static,
    ) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(what.to_string());
        let what = what.to_string();
        self.spawn(ctx, move || Msg::Done {
            what,
            result: job(),
        });
    }

    fn probe(&mut self, ctx: &egui::Context) {
        if self.probing {
            return;
        }
        self.probing = true;
        self.last_probe = Some(Instant::now());
        // Adresse du moteur en marche d'abord, puis celle du fichier (moteur redémarré)
        let mut candidates: Vec<(AppConfig, Layout)> = self.running.iter().cloned().collect();
        candidates.push((self.disk.config.clone(), self.layout.clone()));
        let file_log_dir = self.layout.log_dir.clone();
        self.spawn(ctx, move || {
            let mut health = Err(String::new());
            let mut target = None;
            let mut tried = Vec::new();
            for (cfg, layout) in candidates {
                let addr = probe::target(&cfg).ok();
                if tried.contains(&addr) {
                    continue;
                }
                tried.push(addr);
                match probe::health_detailed(&cfg, &layout) {
                    Ok(h) => {
                        health = Ok(h);
                        target = Some((cfg, layout));
                        break;
                    }
                    Err(e) => health = Err(e),
                }
            }
            let log_dir = target
                .as_ref()
                .map(|(_, l): &(AppConfig, Layout)| l.log_dir.clone())
                .unwrap_or(file_log_dir);
            Msg::Status(Box::new(Snapshot {
                health,
                problems: logs::recent_problems(&log_dir, 8),
                target,
                service: control::service_query(),
                at: Instant::now(),
            }))
        });
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Status(s) => {
                    self.running = s.target.clone();
                    self.snapshot = Some(*s);
                    self.probing = false;
                }
                Msg::Done { what, result } => {
                    self.busy = None;
                    match result {
                        Ok(text) => {
                            self.notify(Kind::Ok, if text.is_empty() { what } else { text })
                        }
                        Err(e) => self.notify(Kind::Error, format!("{what} : {e}")),
                    }
                    // État rafraîchi tout de suite après une action
                    self.last_probe = None;
                    self.refresh_backups();
                }
                Msg::Diag(checks) => {
                    self.diag = Some(checks);
                    self.diag_running = false;
                }
                Msg::EngineVersion(v) => self.engine_version = Some(v),
            }
        }
        let due = self.last_probe.is_none_or(|t| t.elapsed() >= POLL_EVERY);
        if due {
            self.probe(ctx);
        }
        if self.tab == Tab::Logs
            && self.logs.follow
            && self
                .logs
                .loaded_at
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(2))
        {
            self.reload_logs();
        }
    }

    fn notify(&mut self, kind: Kind, text: impl Into<String>) {
        let n = Notice {
            kind,
            text: text.into(),
            at: chrono::Local::now(),
        };
        if kind == Kind::Error {
            self.alert = Some(Notice {
                kind,
                text: n.text.clone(),
                at: n.at,
            });
        }
        self.notices.push(n);
        if self.notices.len() > 50 {
            self.notices.remove(0);
        }
    }

    // ------------------------------------------------------------------------
    // État du moteur
    // ------------------------------------------------------------------------

    fn engine_state(&self) -> EngineState {
        let Some(s) = &self.snapshot else {
            return EngineState::Unknown;
        };
        if s.health.is_ok() {
            return EngineState::Online;
        }
        match s.service.state {
            ServiceState::Running => EngineState::ServiceNoWeb,
            ServiceState::Starting => EngineState::Starting,
            ServiceState::Stopping => EngineState::Stopping,
            _ => EngineState::Stopped,
        }
    }

    /// Le moteur est-il piloté par le service système (sinon : lancement manuel) ?
    fn uses_service(&self) -> bool {
        self.layout.kind == LayoutKind::System
            && self
                .snapshot
                .as_ref()
                .is_some_and(|s| s.service.state.is_installed())
    }

    /// Configuration et emplacements du moteur en marche (adresse réelle, verrou, jeton), ou
    /// ceux du fichier si le moteur ne répond pas.
    fn running_target(&self) -> (AppConfig, Layout) {
        self.running
            .clone()
            .unwrap_or_else(|| (self.disk.config.clone(), self.layout.clone()))
    }

    fn health(&self) -> Option<&Health> {
        self.snapshot.as_ref().and_then(|s| s.health.as_ref().ok())
    }

    /// Le moteur en marche utilise-t-il une autre configuration que celle du fichier ?
    fn restart_needed(&self) -> bool {
        self.health()
            .and_then(|h| h.runtime.as_ref())
            .is_some_and(|rt| rt.config_fingerprint != self.disk.config.fingerprint())
    }

    fn start_engine(&mut self, ctx: &egui::Context) {
        if self.uses_service() {
            self.action(ctx, "Démarrage du service", || {
                control::service_action("start")
            });
        } else if !self.disk.exists {
            self.notify(
                Kind::Warn,
                "Enregistrez d'abord la configuration (onglet Configuration ou assistant).",
            );
        } else {
            let file = self.base_layout.config_file.clone();
            self.action(ctx, "Démarrage du moteur", move || {
                control::start_manual(&file)
            });
        }
    }

    fn stop_engine(&mut self, ctx: &egui::Context) {
        if self.uses_service() {
            self.action(ctx, "Arrêt du service", || control::service_action("stop"));
        } else {
            let (cfg, layout) = self.running_target();
            self.action(ctx, "Arrêt du moteur", move || {
                stop_manual_and_wait(&layout, &cfg)?;
                Ok("Moteur arrêté proprement.".into())
            });
        }
    }

    fn restart_engine(&mut self, ctx: &egui::Context) {
        if self.uses_service() {
            self.action(ctx, "Redémarrage du service", || {
                control::service_action("restart")
            });
        } else {
            let (cfg, layout) = self.running_target();
            let file = self.base_layout.config_file.clone();
            self.action(ctx, "Redémarrage du moteur", move || {
                if probe::health(&cfg).is_ok() {
                    stop_manual_and_wait(&layout, &cfg)?;
                }
                control::start_manual(&file)
            });
        }
    }

    // ------------------------------------------------------------------------
    // Configuration
    // ------------------------------------------------------------------------

    fn reload_disk(&mut self) {
        self.disk = Disk::load(&self.base_layout);
        self.layout = self.base_layout.clone().with_overrides(&self.disk.config);
        self.edit = self.disk.config.clone();
        self.writable = can_write_config(&self.base_layout);
    }

    /// Enregistre `config` ; renvoie la liste des paramètres modifiés en cas de succès.
    fn save(&mut self, config: &AppConfig) -> Option<Vec<String>> {
        let before = self.disk.config.clone();
        match store::save(&self.base_layout, config, self.disk.hash) {
            Ok(report) => {
                let changed = before.changed_fields(config);
                self.reload_disk();
                self.refresh_backups();
                let mut text = format!(
                    "Configuration enregistrée dans « {} ».",
                    self.base_layout.config_file.display()
                );
                if let Some(b) = report.backup {
                    text.push_str(&format!(
                        " Version précédente sauvegardée ({}).",
                        file_label(&b)
                    ));
                }
                self.notify(Kind::Ok, text);
                if self.engine_state() == EngineState::Online && !changed.is_empty() {
                    self.notify(
                        Kind::Warn,
                        "Le moteur en marche utilise encore l'ancienne configuration : redémarrez-le pour l'appliquer.",
                    );
                }
                Some(changed)
            }
            Err(e) => {
                let text = explain_save_error(&e);
                self.notify(Kind::Error, text);
                None
            }
        }
    }

    fn open_wizard(&mut self) {
        let service = control::service_query();
        let has_service =
            self.base_layout.kind == LayoutKind::System && service.state.is_installed();
        // Reprise proposée seulement si la base actuelle n'existe pas encore. Sous Linux, en
        // installation système, elle exige root puis l'utilisateur apex-scada : elle passe par
        // la procédure documentée et non par l'utilitaire.
        let legacy_possible = cfg!(windows) || self.base_layout.kind == LayoutKind::Portable;
        // Proposée même si la base actuelle existe : le moteur la crée dès son premier démarrage.
        // Elle est alors décochée par défaut, et la reprise la sauvegarde avant de la remplacer.
        let legacy = diag::find_legacy_db(&self.layout).filter(|_| legacy_possible);
        let current_db = Some(self.layout.database_file(&self.disk.config)).filter(|f| f.exists());
        // L'assistant part des valeurs en cours d'édition : rien n'est perdu
        let mut w = Wizard::new(
            self.edit.clone(),
            &self.base_layout,
            has_service,
            legacy,
            current_db,
        );
        if let (true, Some(auto)) = (has_service, service.automatic) {
            w.set_autostart(auto);
        }
        self.wizard = Some(w);
    }

    fn finish_wizard(&mut self, ctx: &egui::Context, plan: wizard::Plan) {
        // Moteur en marche AVANT l'enregistrement (son adresse peut changer avec le fichier)
        let (running_cfg, running_layout) = self.running_target();
        if self.save(&plan.config).is_none() {
            return; // l'assistant reste ouvert, l'erreur est affichée
        }
        self.wizard = None;
        self.tab = Tab::Status;
        let uses_service = self.base_layout.kind == LayoutKind::System
            && control::service_query().state.is_installed();
        let file = self.base_layout.config_file.clone();
        let privileged = self.base_layout.kind == LayoutKind::System;
        self.action(ctx, "Configuration initiale", move || {
            // Étapes indépendantes : un échec est signalé sans empêcher les suivantes,
            // et le moteur est toujours relancé s'il a été arrêté ici.
            let mut done = vec!["Configuration enregistrée.".to_string()];
            let mut errors: Vec<String> = Vec::new();
            let mut stopped_service = false;
            if let Some(src) = &plan.migrate_legacy {
                // La copie exige le moteur arrêté (verrou d'instance)
                let stopped = if uses_service {
                    if control::service_query().state != ServiceState::Stopped {
                        let r =
                            control::service_action("stop").and_then(|_| wait_service_stopped());
                        stopped_service = r.is_ok();
                        r
                    } else {
                        Ok(())
                    }
                } else if probe::health(&running_cfg).is_ok() {
                    stop_manual_and_wait(&running_layout, &running_cfg)
                } else {
                    Ok(())
                };
                match stopped {
                    Err(e) => errors.push(format!("reprise des données impossible : {e}")),
                    Ok(()) => {
                        let src = src.display().to_string();
                        let file_s = file.display().to_string();
                        let args = [
                            "migrate-legacy",
                            "--from",
                            &src,
                            "--force",
                            "--config",
                            &file_s,
                        ];
                        let result = if privileged {
                            control::run_privileged(&control::engine_path(), &args)
                        } else {
                            run_engine(&args)
                        };
                        match result {
                            Ok((0, _)) => {
                                done.push("Données de la version antérieure copiées.".into())
                            }
                            Ok((code, out)) => errors.push(format!(
                                "reprise des données impossible (code {code} : {}){}",
                                apex_config::exit::describe(code),
                                if out.is_empty() {
                                    String::new()
                                } else {
                                    format!(
                                        "
{out}"
                                    )
                                }
                            )),
                            Err(e) => errors.push(format!("reprise des données impossible : {e}")),
                        }
                    }
                }
            }
            if let Some(auto) = plan.autostart {
                match control::service_action(if auto { "enable" } else { "disable" }) {
                    Ok(_) => done.push(
                        if auto {
                            "Démarrage automatique activé."
                        } else {
                            "Démarrage automatique désactivé."
                        }
                        .into(),
                    ),
                    Err(e) => errors.push(format!("démarrage automatique non modifié : {e}")),
                }
            }
            if plan.start_engine || stopped_service {
                let started = if uses_service {
                    control::service_action("restart").map(|_| "Service démarré.".to_string())
                } else {
                    let stop = if probe::health(&running_cfg).is_ok() {
                        stop_manual_and_wait(&running_layout, &running_cfg)
                    } else {
                        Ok(())
                    };
                    stop.and_then(|_| control::start_manual(&file))
                };
                match started {
                    Ok(m) => done.push(m),
                    Err(e) => errors.push(format!("démarrage du moteur impossible : {e}")),
                }
            }
            if errors.is_empty() {
                Ok(done.join(" "))
            } else {
                Err(format!(
                    "{}
{}",
                    done.join(" "),
                    errors.join(
                        "
"
                    )
                ))
            }
        });
    }

    // ------------------------------------------------------------------------
    // Journaux et sauvegardes
    // ------------------------------------------------------------------------

    fn reload_logs(&mut self) {
        self.logs.loaded_at = Some(Instant::now());
        match logs::list(&self.layout.log_dir) {
            Ok(files) => {
                self.logs.error = None;
                let still_there = self
                    .logs
                    .selected
                    .as_ref()
                    .is_some_and(|s| files.iter().any(|f| &f.path == s));
                if !still_there {
                    self.logs.selected = logs::latest_daily(&files)
                        .or(files.first())
                        .map(|f| f.path.clone());
                }
                self.logs.files = files;
            }
            Err(e) => {
                self.logs.files.clear();
                self.logs.selected = None;
                self.logs.error = Some(e);
            }
        }
        self.logs.content = match &self.logs.selected {
            Some(p) => match logs::tail(p, LOG_TAIL_BYTES) {
                Ok(t) if self.logs.problems_only => t
                    .lines()
                    .filter(|l| logs::is_problem(l) || p.ends_with(logs::STARTUP_ERROR_FILE))
                    .collect::<Vec<_>>()
                    .join("\n"),
                Ok(t) => t,
                Err(e) => {
                    self.logs.error = Some(e);
                    String::new()
                }
            },
            None => String::new(),
        };
    }

    fn refresh_backups(&mut self) {
        self.backups.list = store::list_backups(&self.base_layout.backup_dir);
        if let Some(sel) = self.backups.selected.clone() {
            if self.backups.list.contains(&sel) {
                self.select_backup(sel);
            } else {
                self.backups.selected = None;
            }
        }
    }

    fn select_backup(&mut self, path: PathBuf) {
        self.backups.preview = Some(match (std::fs::read_to_string(&path), store::load(&path)) {
            (Ok(raw), Ok(loaded)) => Ok((raw, self.disk.config.changed_fields(&loaded.config))),
            (Err(e), _) => Err(format!("lecture impossible : {e}")),
            (_, Err(e)) => Err(e.to_string()),
        });
        self.backups.selected = Some(path);
    }

    fn restore(&mut self, path: &std::path::Path) {
        match store::restore_backup(&self.base_layout, path, self.disk.hash) {
            Ok(_) => {
                self.reload_disk();
                self.refresh_backups();
                self.notify(
                    Kind::Ok,
                    format!(
                        "Configuration du {} restaurée (la version remplacée a été sauvegardée). Redémarrez le moteur pour l'appliquer.",
                        file_label(path)
                    ),
                );
            }
            Err(e) => {
                let text = explain_save_error(&e);
                self.notify(Kind::Error, text);
            }
        }
    }

    fn relaunch_elevated(&mut self, ctx: &egui::Context, wizard: bool) {
        let args = relaunch_args(&self.base_layout.config_file, wizard);
        #[cfg(windows)]
        match control::relaunch_elevated(&args) {
            Ok(()) => {
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Err(e) => self.notify(
                Kind::Error,
                format!("Relance en administrateur impossible : {e}"),
            ),
        }
        #[cfg(not(windows))]
        {
            let _ = (ctx, args);
        }
    }
}

// ============================================================================
// INTERFACE
// ============================================================================

impl eframe::App for AdminApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        self.handle_close(&ctx);

        self.header(ui, &ctx);

        if let Some(mut w) = self.wizard.take() {
            match w.ui(ui, &self.base_layout, self.writable) {
                wizard::Outcome::Continue => self.wizard = Some(w),
                wizard::Outcome::Cancel => {
                    if !self.disk.exists {
                        self.notify(
                            Kind::Info,
                            "Assistant annulé : aucune configuration enregistrée, le moteur utilisera les valeurs par défaut.",
                        );
                    }
                }
                wizard::Outcome::Finish(plan) => {
                    self.wizard = Some(w);
                    self.finish_wizard(&ctx, *plan);
                }
            }
        } else {
            self.navigation(ui);
            self.status_bar(ui);
            egui::CentralPanel::default_margins().show(ui, |ui| {
                self.alert_banner(ui);
                match self.tab {
                    Tab::Status => self.status_tab(ui, &ctx),
                    Tab::Config => self.config_tab(ui, &ctx),
                    Tab::Logs => self.logs_tab(ui),
                    Tab::Diagnostics => self.diag_tab(ui, &ctx),
                    Tab::Backups => self.backups_tab(ui),
                    Tab::About => self.about_tab(ui),
                }
            });
        }

        self.confirm_modal(&ctx);
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

impl AdminApp {
    fn handle_close(&mut self, ctx: &egui::Context) {
        let requested = ctx.input(|i| i.viewport().close_requested());
        if !requested || self.allow_close {
            return;
        }
        if let Some(what) = &self.busy {
            // Fermer interromprait l'action (moteur arrêté mais jamais relancé, par exemple)
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm = Some(Confirm::QuitBusy(what.clone()));
        } else if self.edit != self.disk.config && self.wizard.is_none() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm = Some(Confirm::QuitUnsaved);
        }
    }

    fn header(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let name = self.disk.config.instance.name.trim();
                ui.heading(RichText::new(name).strong());
                // Le nom du produit n'est rappelé que si l'instance porte un autre nom
                let subtitle = if name == version::PRODUCT_NAME {
                    version::VERSION.to_string()
                } else {
                    format!("— {} {}", version::PRODUCT_NAME, version::VERSION)
                };
                ui.label(RichText::new(subtitle).weak());
                if version::is_prerelease() {
                    ui.label(RichText::new("ALPHA").small().color(form::warn_color(ui)));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !self.writable {
                        if control::CAN_RELAUNCH_ELEVATED
                            && !self.elevated
                            && self.installed()
                            && ui.button("Relancer en administrateur").clicked()
                        {
                            let wizard = self.wizard.is_some();
                            self.relaunch_elevated(ctx, wizard);
                        }
                        ui.label(RichText::new("Lecture seule").color(form::warn_color(ui)))
                            .on_hover_text(
                                "Votre compte ne peut pas modifier le fichier de configuration.",
                            );
                    } else if self.elevated {
                        ui.label(RichText::new("Administrateur").weak());
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(match self.base_layout.kind {
                        LayoutKind::System => "Installation système",
                        LayoutKind::Portable => "Mode portable",
                    })
                    .small(),
                );
                form::truncated(
                    ui,
                    RichText::new(self.base_layout.config_file.display().to_string())
                        .small()
                        .monospace()
                        .weak(),
                );
            });
            ui.add_space(4.0);
        });
    }

    fn navigation(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("nav")
            .resizable(false)
            .exact_size(180.0)
            .show(ui, |ui| {
                ui.add_space(8.0);
                for tab in Tab::ALL {
                    let mut text = RichText::new(tab.label());
                    if tab == Tab::Config && self.edit != self.disk.config {
                        text = RichText::new(format!("{} •", tab.label()));
                    }
                    if ui.selectable_label(self.tab == tab, text).clicked() {
                        self.tab = tab;
                        if tab == Tab::Logs {
                            self.reload_logs();
                        }
                    }
                }
                ui.add_space(16.0);
                ui.separator();
                if ui.button("Assistant de configuration…").clicked() {
                    self.open_wizard();
                }
            });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                if let Some(b) = &self.busy {
                    ui.spinner();
                    ui.label(format!("{b}…"));
                    ui.separator();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(s) = &self.snapshot {
                        ui.label(
                            RichText::new(format!(
                                "État actualisé il y a {} s",
                                s.at.elapsed().as_secs()
                            ))
                            .small()
                            .weak(),
                        );
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        if let Some(n) = self.notices.last() {
                            let color = kind_color(ui, n.kind);
                            // Première ligne seulement : le détail est dans le bandeau ou l'historique
                            let first = n.text.lines().next().unwrap_or_default();
                            form::truncated(
                                ui,
                                RichText::new(format!("{} {first}", n.at.format("%H:%M:%S")))
                                    .color(color),
                            );
                        }
                    });
                });
            });
        });
    }

    fn alert_banner(&mut self, ui: &mut egui::Ui) {
        let mut dismiss = false;
        if let Some(a) = &self.alert {
            egui::Frame::group(ui.style())
                .stroke(egui::Stroke::new(1.0, form::error_color(ui)))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal_top(|ui| {
                        form::dot(ui, form::error_color(ui));
                        form::wrapped(ui, a.text.as_str());
                    });
                    ui.horizontal(|ui| {
                        if ui.small_button("Fermer").clicked() {
                            dismiss = true;
                        }
                        if ui.small_button("Copier le message").clicked() {
                            ui.ctx().copy_text(a.text.clone());
                        }
                    });
                });
            ui.add_space(6.0);
        }
        if dismiss {
            self.alert = None;
        }
    }

    // ------------------------------------------------------------------------
    // Onglet « État du moteur »
    // ------------------------------------------------------------------------

    fn status_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let state = self.engine_state();
        let (color, title) = match &state {
            EngineState::Online => (form::ok_color(ui), "En ligne".to_string()),
            EngineState::ServiceNoWeb => (
                form::error_color(ui),
                "Service démarré, mais le serveur web ne répond pas".to_string(),
            ),
            EngineState::Starting => (form::warn_color(ui), "Démarrage en cours".to_string()),
            EngineState::Stopping => (form::warn_color(ui), "Arrêt en cours".to_string()),
            EngineState::Stopped => (ui.visuals().weak_text_color(), "Arrêté".to_string()),
            EngineState::Unknown => (ui.visuals().weak_text_color(), "Interrogation…".to_string()),
        };

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal(|ui| {
                form::dot(ui, color);
                ui.heading(RichText::new(&title).strong());
            });
            if let Some(Err(e)) = self.snapshot.as_ref().map(|s| &s.health) {
                if state != EngineState::Online {
                    form::hint(ui, format!("Serveur web : {e}."));
                }
            }
            ui.add_space(8.0);

            let busy = self.busy.is_some();
            let uses_service = self.uses_service();
            ui.horizontal(|ui| {
                let can_start = matches!(state, EngineState::Stopped);
                let can_stop = matches!(state, EngineState::Online | EngineState::ServiceNoWeb | EngineState::Starting);
                if ui.add_enabled(!busy && can_start, egui::Button::new("Démarrer")).clicked() {
                    self.start_engine(ctx);
                }
                if ui.add_enabled(!busy && can_stop, egui::Button::new("Arrêter…")).clicked() {
                    self.confirm = Some(Confirm::Stop);
                }
                if ui.add_enabled(!busy && can_stop, egui::Button::new("Redémarrer…")).clicked() {
                    self.confirm = Some(Confirm::Restart);
                }
                ui.separator();
                let url = probe::web_url(&self.running_target().0);
                if ui
                    .add_enabled(state == EngineState::Online, egui::Button::new("Ouvrir l'IHM web"))
                    .on_hover_text(&url)
                    .clicked()
                {
                    ctx.open_url(egui::OpenUrl::new_tab(url));
                }
                if ui.button("Actualiser").clicked() {
                    self.last_probe = None;
                }
            });
            if cfg!(windows) && uses_service && !self.elevated {
                form::hint(ui, "Les actions sur le service demandent une confirmation administrateur (UAC).");
            }
            ui.add_space(8.0);

            if self.restart_needed() {
                form::banner(
                    ui,
                    form::warn_color(ui),
                    "La configuration a été modifiée depuis le démarrage du moteur : redémarrez-le pour l'appliquer.",
                );
                ui.add_space(6.0);
            }

            let mut autostart_toggle: Option<&'static str> = None;
            egui::Grid::new("status_grid").num_columns(2).spacing([24.0, 6.0]).striped(true).show(ui, |ui| {
                ui.label("Adresse de l'IHM");
                ui.monospace(probe::web_url(&self.running_target().0));
                ui.end_row();

                ui.label("Lancement");
                ui.label(if uses_service {
                    "service système"
                } else if self.layout.kind == LayoutKind::System {
                    "manuel (service non installé)"
                } else {
                    "manuel (mode portable)"
                });
                ui.end_row();

                if let Some(s) = &self.snapshot {
                    if self.layout.kind == LayoutKind::System {
                        ui.label("Service");
                        ui.horizontal(|ui| {
                            ui.label(s.service.state.label());
                            if let Some(auto) = s.service.automatic {
                                let mut on = auto;
                                if ui
                                    .add_enabled(!busy, egui::Checkbox::new(&mut on, "démarrage automatique"))
                                    .changed()
                                {
                                    autostart_toggle = Some(if on { "enable" } else { "disable" });
                                }
                            }
                        });
                        ui.end_row();
                        if let Some(code) = s.service.last_exit_code {
                            ui.label("Dernier arrêt");
                            ui.colored_label(
                                form::warn_color(ui),
                                format!("code {code} : {}", apex_config::exit::describe(code)),
                            );
                            ui.end_row();
                        }
                    }
                }

                if let Some(h) = self.health().cloned() {
                    ui.label("Version du moteur");
                    if h.version == version::VERSION {
                        ui.label(h.runtime.as_ref().map(|r| r.full_version.clone()).unwrap_or(h.version.clone()));
                    } else {
                        ui.colored_label(
                            form::warn_color(ui),
                            format!("{} (utilitaire : {}) — redémarrez le moteur", h.version, version::VERSION),
                        );
                    }
                    ui.end_row();
                    ui.label("En marche depuis");
                    ui.label(human_duration(h.uptime_seconds));
                    ui.end_row();
                    ui.label("Drivers actifs");
                    ui.label(match h.total_drivers {
                        Some(t) => format!("{} / {t}", h.running_drivers),
                        None => h.running_drivers.to_string(),
                    });
                    ui.end_row();
                    if let Some(a) = h.active_alarms {
                        ui.label("Alarmes actives");
                        ui.label(a.to_string());
                        ui.end_row();
                    }
                    if h.acquisition_paused {
                        ui.label("Acquisition");
                        ui.colored_label(form::warn_color(ui), "en pause (depuis l'IHM web)");
                        ui.end_row();
                    }
                    if let Some(rt) = &h.runtime {
                        ui.label("Processus");
                        ui.label(format!("PID {} — mode {}", rt.pid, if rt.run_mode == "service" { "service" } else { "manuel" }));
                        ui.end_row();
                        if rt.safe_mode {
                            ui.label("Mode sûr");
                            ui.colored_label(form::warn_color(ui), "actif : aucune commande vers les équipements réels");
                            ui.end_row();
                        }
                        if !rt.env_overrides.is_empty() {
                            ui.label("Surcharges");
                            ui.colored_label(
                                form::warn_color(ui),
                                format!(
                                    "variables d'environnement actives : {}",
                                    rt.env_overrides.join(", ")
                                ),
                            );
                            ui.end_row();
                        }
                        if rt.simulation_enabled {
                            ui.label("Simulation");
                            ui.colored_label(form::info_color(ui), "active (automate embarqué, protocoles simulés)");
                            ui.end_row();
                        }
                    } else {
                        ui.label("Détails");
                        form::hint(ui, if cfg!(windows) {
                            "réservés au poste local et aux administrateurs : relancez l'utilitaire en administrateur"
                        } else {
                            "réservés au poste local et au groupe apex-scada"
                        });
                        ui.end_row();
                    }
                }
            });
            if let Some(action) = autostart_toggle {
                self.action(ctx, "Démarrage automatique", move || control::service_action(action));
            }

            ui.add_space(14.0);
            ui.strong("Dernières erreurs et avertissements");
            match self.snapshot.as_ref().map(|s| &s.problems) {
                Some(p) if !p.is_empty() => {
                    for line in p.iter().rev() {
                        let color = if line.contains(" ERROR ") || line.contains(" ERREUR ") {
                            form::error_color(ui)
                        } else {
                            form::warn_color(ui)
                        };
                        form::wrapped(ui, RichText::new(line).small().monospace().color(color));
                    }
                    if ui.small_button("Voir les journaux").clicked() {
                        self.tab = Tab::Logs;
                        self.logs.problems_only = true;
                        self.reload_logs();
                    }
                }
                Some(_) => form::hint(ui, "Aucune dans le journal du jour."),
                None => form::hint(ui, "…"),
            }
        });
    }

    // ------------------------------------------------------------------------
    // Onglet « Configuration »
    // ------------------------------------------------------------------------

    fn config_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let issues = self.edit.validate();
        let changed = self.disk.config.changed_fields(&self.edit);
        let has_errors = apex_config::has_errors(&issues);
        let needs_write =
            !self.disk.exists || self.disk.migrated_from.is_some() || self.disk.error.is_some();

        if let Some(e) = &self.disk.error {
            form::banner(
                ui,
                form::error_color(ui),
                &format!(
                    "Fichier illisible : {e}\nLes valeurs affichées sont les valeurs par défaut ; l'enregistrement remplacera le fichier (l'actuel sera sauvegardé)."
                ),
            );
        } else if !self.disk.exists {
            form::banner(
                ui,
                form::info_color(ui),
                "Aucun fichier de configuration : valeurs par défaut affichées. Enregistrez pour créer le fichier.",
            );
        } else if let Some(v) = self.disk.migrated_from {
            form::banner(
                ui,
                form::info_color(ui),
                &format!(
                    "Fichier au format v{v} : il sera réécrit au format v{} à l'enregistrement (avec sauvegarde).",
                    apex_config::CONFIG_VERSION
                ),
            );
        }
        if !self.writable {
            form::banner(
                ui,
                form::warn_color(ui),
                if !self.installed() {
                    "Lecture seule : aucune installation système détectée. Installez le logiciel, ou ouvrez une instance portable avec --config <fichier>."
                } else if control::CAN_RELAUNCH_ELEVATED {
                    "Lecture seule : la modification de la configuration est réservée aux administrateurs (bouton « Relancer en administrateur »)."
                } else {
                    "Lecture seule : ajoutez votre compte au groupe apex-scada (sudo usermod -aG apex-scada $USER), puis reconnectez-vous."
                },
            );
        }

        ui.horizontal(|ui| {
            let can_save = self.writable && !has_errors && (!changed.is_empty() || needs_write);
            if ui
                .add_enabled(
                    can_save,
                    egui::Button::new(RichText::new("Enregistrer").strong()),
                )
                .clicked()
            {
                let cfg = self.edit.clone();
                self.save(&cfg);
            }
            if ui
                .add_enabled(
                    !changed.is_empty(),
                    egui::Button::new("Annuler les modifications"),
                )
                .clicked()
            {
                self.edit = self.disk.config.clone();
            }
            if ui.button("Recharger depuis le disque").clicked() {
                self.reload_disk();
                self.notify(Kind::Info, "Configuration rechargée depuis le disque.");
            }
            if self.engine_state() == EngineState::Online
                && self.restart_needed()
                && ui.button("Redémarrer le moteur…").clicked()
            {
                self.confirm = Some(Confirm::Restart);
            }
            let _ = ctx;
        });
        if has_errors {
            ui.colored_label(
                form::error_color(ui),
                "Corrigez les erreurs signalées pour pouvoir enregistrer.",
            );
        } else if !changed.is_empty() {
            form::hint(
                ui,
                format!(
                    "Modifié : {} — appliqué au prochain démarrage du moteur.",
                    changed.join(", ")
                ),
            );
        } else {
            form::hint(
                ui,
                "Tous les paramètres de ce fichier sont lus au démarrage du moteur.",
            );
        }
        ui.add_space(6.0);

        let (def_data, def_logs, def_web) = form::default_locations(&self.base_layout, &self.edit);
        let writable = self.writable;
        let edit = &mut self.edit;
        let port_check = &mut self.port_check;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add_enabled_ui(writable, |ui| {
                section(ui, "Instance", |ui| {
                    ui.label("Nom de l'instance");
                    ui.add(egui::TextEdit::singleline(&mut edit.instance.name).desired_width(320.0));
                    form::show_issues(ui, &issues, "instance.name");
                });

                section(ui, "Réseau", |ui| {
                    ui.label("Adresse d'écoute");
                    form::bind_editor(ui, &mut edit.server.bind);
                    form::show_issues(ui, &issues, "server.bind");
                    ui.add_space(6.0);
                    ui.label("Port web");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut edit.server.port).range(1..=65535u16));
                        if ui.button("Vérifier la disponibilité").clicked() {
                            let r = form::port_available(&edit.server.bind, edit.server.port);
                            *port_check = Some((edit.server.bind.clone(), edit.server.port, r));
                        }
                        if let Some((b, p, r)) = port_check.as_ref() {
                            if *b == edit.server.bind && *p == edit.server.port {
                                match r {
                                    Ok(()) => ui.colored_label(form::ok_color(ui), "Port libre"),
                                    Err(e) => ui.colored_label(
                                        form::error_color(ui),
                                        format!("{e} (normal si le moteur est en marche sur ce port)"),
                                    ),
                                };
                            }
                        }
                    });
                    form::show_issues(ui, &issues, "server.port");
                    form::hint(ui, format!("IHM : {}", probe::web_url(edit)));
                });

                section(ui, "Exploitation", |ui| {
                    ui.checkbox(&mut edit.safety.safe_mode, "Mode sûr : aucune commande vers les équipements réels");
                    form::hint(ui, "Pour la mise en service et les tests ; la supervision reste active.");
                    form::show_issues(ui, &issues, "safety.safe_mode");
                    ui.add_space(6.0);
                    ui.checkbox(&mut edit.simulation.enabled, "Simulation et données de démonstration");
                    form::hint(
                        ui,
                        "Automate Modbus embarqué, simulateur GTB et drivers OPC-UA / BACnet / S7 SIMULÉS : \
                         à désactiver en production.",
                    );
                    if edit.simulation.enabled {
                        ui.horizontal(|ui| {
                            ui.label("Port de l'automate embarqué");
                            ui.add(egui::DragValue::new(&mut edit.simulation.modbus_slave_port).range(1..=65535u16));
                        });
                        form::show_issues(ui, &issues, "simulation.modbus_slave_port");
                    }
                });

                section(ui, "Historique et sécurité", |ui| {
                    egui::Grid::new("cfg_hist").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                        ui.label("Rétention de l'historique");
                        ui.add(egui::DragValue::new(&mut edit.history.retention_days).range(0..=3650u32).suffix(" jours"));
                        ui.end_row();
                        ui.label("Expiration des sessions");
                        ui.add(egui::DragValue::new(&mut edit.security.session_ttl_hours).range(1..=720u32).suffix(" h"));
                        ui.end_row();
                    });
                    form::hint(ui, "Rétention 0 = illimitée. Les comptes et mots de passe se gèrent dans l'IHM web.");
                    form::show_issues(ui, &issues, "history.retention_days");
                    form::show_issues(ui, &issues, "security.session_ttl_hours");
                });

                section(ui, "Journalisation", |ui| {
                    egui::Grid::new("cfg_log").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                        ui.label("Niveau");
                        egui::ComboBox::from_id_salt("log_level")
                            .selected_text(format!("{} — {}", edit.logging.level, form::level_label(&edit.logging.level)))
                            .show_ui(ui, |ui| {
                                for l in apex_config::model::LOG_LEVELS {
                                    ui.selectable_value(
                                        &mut edit.logging.level,
                                        l.to_string(),
                                        format!("{l} — {}", form::level_label(l)),
                                    );
                                }
                            });
                        ui.end_row();
                        ui.label("Fichiers journaux");
                        ui.checkbox(&mut edit.logging.file_enabled, "écrire dans des fichiers quotidiens");
                        ui.end_row();
                        ui.label("Fichiers conservés");
                        ui.add(egui::DragValue::new(&mut edit.logging.retention_files).range(1..=365u32));
                        ui.end_row();
                    });
                    if !edit.logging.file_enabled {
                        form::hint(ui, "Sans fichiers, cet utilitaire ne pourra afficher que les erreurs de démarrage.");
                    }
                    form::show_issues(ui, &issues, "logging.level");
                    form::show_issues(ui, &issues, "logging.retention_files");
                });

                section(ui, "Emplacements", |ui| {
                    form::hint(ui, "Chemins absolus ; laisser vide pour l'emplacement standard (indiqué en grisé).");
                    egui::Grid::new("cfg_paths").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                        ui.label("Données");
                        form::path_field(ui, &mut edit.paths.data_dir, &def_data);
                        ui.end_row();
                        ui.label("Journaux");
                        form::path_field(ui, &mut edit.paths.log_dir, &def_logs);
                        ui.end_row();
                        ui.label("IHM web");
                        form::path_field(ui, &mut edit.paths.web_dir, &def_web);
                        ui.end_row();
                        ui.label("Fichier de base");
                        ui.add(egui::TextEdit::singleline(&mut edit.database.file).desired_width(f32::INFINITY));
                        ui.end_row();
                    });
                    for f in ["paths.data_dir", "paths.log_dir", "paths.web_dir", "database.file"] {
                        form::show_issues(ui, &issues, f);
                    }
                    form::hint(
                        ui,
                        "Déplacer les données ne copie pas la base existante : arrêtez le moteur et déplacez les fichiers vous-même.",
                    );
                });
            });
        });
    }

    // ------------------------------------------------------------------------
    // Onglet « Journaux »
    // ------------------------------------------------------------------------

    fn logs_tab(&mut self, ui: &mut egui::Ui) {
        let mut reload = false;
        ui.horizontal(|ui| {
            let current = self
                .logs
                .selected
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "aucun journal".into());
            egui::ComboBox::from_id_salt("log_file")
                .selected_text(current)
                .width(260.0)
                .show_ui(ui, |ui| {
                    for f in &self.logs.files {
                        let label = format!("{} ({})", f.name, diag::human_size(f.size));
                        if ui
                            .selectable_label(self.logs.selected.as_ref() == Some(&f.path), label)
                            .clicked()
                        {
                            self.logs.selected = Some(f.path.clone());
                            reload = true;
                        }
                    }
                });
            if ui.button("Actualiser").clicked() {
                reload = true;
            }
            if ui
                .checkbox(&mut self.logs.problems_only, "Erreurs et avertissements")
                .changed()
            {
                reload = true;
            }
            ui.checkbox(&mut self.logs.follow, "Suivre");
            if ui.button("Copier").clicked() {
                ui.ctx().copy_text(self.logs.content.clone());
            }
            if ui.button("Ouvrir le dossier").clicked() {
                if let Err(e) = control::open_path(&self.layout.log_dir) {
                    self.notify(Kind::Error, e);
                }
            }
        });
        if reload {
            self.reload_logs();
        }
        if let Some(e) = &self.logs.error {
            form::banner(ui, form::warn_color(ui), e);
        }
        ui.add_space(4.0);
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .stick_to_bottom(self.logs.follow)
            .show(ui, |ui| {
                let mut text = self.logs.content.as_str();
                ui.add(
                    egui::TextEdit::multiline(&mut text)
                        .font(egui::TextStyle::Monospace)
                        .desired_width(f32::INFINITY)
                        .interactive(true),
                );
            });
    }

    // ------------------------------------------------------------------------
    // Onglet « Diagnostics »
    // ------------------------------------------------------------------------

    fn diag_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !self.diag_running,
                    egui::Button::new(RichText::new("Lancer les diagnostics").strong()),
                )
                .clicked()
            {
                self.diag_running = true;
                let layout = self.base_layout.clone();
                let service = self.snapshot.as_ref().map(|s| s.service.clone());
                self.spawn(ctx, move || Msg::Diag(diag::run(&layout, service.as_ref())));
            }
            if self.diag_running {
                ui.spinner();
            }
            if let Some(checks) = &self.diag {
                if ui.button("Copier le rapport").clicked() {
                    ui.ctx().copy_text(diag::report(checks, &self.base_layout));
                    self.notify(
                        Kind::Info,
                        "Rapport de diagnostic copié dans le presse-papiers.",
                    );
                }
            }
        });
        form::hint(
            ui,
            "Configuration, port, serveur web, configuration appliquée, IHM, données, journaux et service système.",
        );
        ui.add_space(8.0);
        let Some(checks) = &self.diag else {
            return;
        };
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for c in checks {
                    let color = match c.level {
                        diag::Level::Ok => form::ok_color(ui),
                        diag::Level::Info => form::info_color(ui),
                        diag::Level::Warning => form::warn_color(ui),
                        diag::Level::Error => form::error_color(ui),
                    };
                    ui.horizontal_top(|ui| {
                        form::dot(ui, color);
                        ui.vertical(|ui| {
                            ui.strong(&c.title);
                            ui.label(&c.detail);
                        });
                    });
                    ui.add_space(4.0);
                }
            });
    }

    // ------------------------------------------------------------------------
    // Onglet « Sauvegardes »
    // ------------------------------------------------------------------------

    fn backups_tab(&mut self, ui: &mut egui::Ui) {
        form::hint(
            ui,
            format!(
                "Une sauvegarde est créée à chaque modification ({} conservées) dans « {} ».",
                store::MAX_BACKUPS,
                self.base_layout.backup_dir.display()
            ),
        );
        ui.add_space(6.0);
        if self.backups.list.is_empty() {
            ui.label("Aucune sauvegarde pour le moment.");
            return;
        }
        let mut select = None;
        egui::Panel::left("backup_list")
            .resizable(true)
            .default_size(240.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for b in &self.backups.list {
                            if ui
                                .selectable_label(
                                    self.backups.selected.as_ref() == Some(b),
                                    file_label(b),
                                )
                                .clicked()
                            {
                                select = Some(b.clone());
                            }
                        }
                    });
            });
        if let Some(b) = select {
            self.select_backup(b);
        }
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let Some(sel) = self.backups.selected.clone() else {
                ui.label(
                    "Sélectionnez une sauvegarde pour la comparer à la configuration actuelle.",
                );
                return;
            };
            match &self.backups.preview {
                Some(Ok((raw, diff))) => {
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                self.writable,
                                egui::Button::new("Restaurer cette version…"),
                            )
                            .clicked()
                        {
                            self.confirm = Some(Confirm::Restore(sel.clone()));
                        }
                        if diff.is_empty() {
                            ui.label("Identique à la configuration actuelle.");
                        } else {
                            ui.label(format!("Diffère sur : {}", diff.join(", ")));
                        }
                    });
                    ui.add_space(4.0);
                    egui::ScrollArea::both()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            let mut text = raw.as_str();
                            ui.add(
                                egui::TextEdit::multiline(&mut text)
                                    .font(egui::TextStyle::Monospace)
                                    .desired_width(f32::INFINITY),
                            );
                        });
                }
                Some(Err(e)) => {
                    ui.colored_label(
                        form::error_color(ui),
                        format!("Sauvegarde inutilisable : {e}"),
                    );
                }
                None => {}
            }
        });
    }

    // ------------------------------------------------------------------------
    // Onglet « À propos »
    // ------------------------------------------------------------------------

    fn about_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading(version::PRODUCT_NAME);
        ui.label(format!(
            "Utilitaire d'administration {}",
            version::full_version()
        ));
        if version::is_prerelease() {
            ui.colored_label(
                form::warn_color(ui),
                "Version de pré-production (alpha) : ne pas utiliser en production sans validation.",
            );
        }
        ui.add_space(10.0);
        let engine = control::engine_path();
        let mut open: Option<PathBuf> = None;
        egui::Grid::new("about")
            .num_columns(3)
            .spacing([16.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label("Moteur");
                ui.monospace(engine.display().to_string());
                ui.label(match &self.engine_version {
                    Some(Ok(v)) => v.clone(),
                    Some(Err(e)) => e.clone(),
                    None => "…".into(),
                });
                ui.end_row();
                let rows: [(&str, PathBuf); 5] = [
                    (
                        "Configuration",
                        self.base_layout
                            .config_file
                            .parent()
                            .map(|p| p.to_path_buf())
                            .unwrap_or_default(),
                    ),
                    ("Sauvegardes", self.base_layout.backup_dir.clone()),
                    ("Données", self.layout.data_dir.clone()),
                    ("Journaux", self.layout.log_dir.clone()),
                    ("IHM web", self.layout.web_dir(&self.disk.config)),
                ];
                for (label, path) in rows {
                    ui.label(label);
                    ui.monospace(path.display().to_string());
                    if ui.small_button("Ouvrir").clicked() {
                        open = Some(path);
                    }
                    ui.end_row();
                }
            });
        if let Some(p) = open {
            if let Err(e) = control::open_path(&p) {
                self.notify(Kind::Error, e);
            }
        }
        ui.add_space(10.0);
        ui.collapsing("Historique des messages", |ui| {
            for n in self.notices.iter().rev() {
                ui.colored_label(
                    kind_color(ui, n.kind),
                    format!("{} {}", n.at.format("%H:%M:%S"), n.text),
                );
            }
        });
    }

    // ------------------------------------------------------------------------
    // Confirmations
    // ------------------------------------------------------------------------

    fn confirm_modal(&mut self, ctx: &egui::Context) {
        let Some(confirm) = &self.confirm else {
            return;
        };
        let (title, text, ok): (&str, String, &str) = match confirm {
            Confirm::Stop => (
                "Arrêter le moteur ?",
                "La supervision, l'acquisition, les alarmes et les automatismes seront interrompus jusqu'au prochain démarrage.".into(),
                "Arrêter",
            ),
            Confirm::Restart => (
                "Redémarrer le moteur ?",
                "La supervision et les automatismes seront interrompus quelques secondes, le temps du redémarrage.".into(),
                "Redémarrer",
            ),
            Confirm::Restore(p) => (
                "Restaurer cette configuration ?",
                format!(
                    "La configuration du {} remplacera l'actuelle (qui sera sauvegardée). Elle s'appliquera au prochain démarrage du moteur.{}",
                    file_label(p),
                    if self.edit != self.disk.config {
                        "

Attention : les modifications non enregistrées de l'onglet Configuration seront perdues."
                    } else {
                        ""
                    }
                ),
                "Restaurer",
            ),
            Confirm::QuitUnsaved => (
                "Quitter sans enregistrer ?",
                "Des modifications de configuration n'ont pas été enregistrées. Le moteur continue de fonctionner après la fermeture de l'utilitaire.".into(),
                "Quitter sans enregistrer",
            ),
            Confirm::QuitBusy(what) => (
                "Une action est en cours",
                format!(
                    "« {what} » n'est pas terminée. Fermer maintenant peut l'interrompre à mi-parcours (par exemple, moteur arrêté mais pas relancé). Attendez la fin de l'action de préférence."
                ),
                "Fermer quand même",
            ),
        };
        let mut decision: Option<bool> = None;
        let modal = egui::Modal::new(egui::Id::unique("confirm_modal")).show(ctx, |ui| {
            ui.set_max_width(420.0);
            ui.heading(title);
            ui.add_space(6.0);
            ui.label(text);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new(ok).strong()).clicked() {
                    decision = Some(true);
                }
                if ui.button("Annuler").clicked() {
                    decision = Some(false);
                }
            });
        });
        if decision.is_none() && modal.should_close() {
            decision = Some(false);
        }
        let Some(yes) = decision else {
            return;
        };
        let confirm = self.confirm.take();
        if !yes {
            return;
        }
        match confirm {
            Some(Confirm::Stop) => self.stop_engine(ctx),
            Some(Confirm::Restart) => self.restart_engine(ctx),
            Some(Confirm::Restore(p)) => self.restore(&p),
            Some(Confirm::QuitUnsaved) | Some(Confirm::QuitBusy(_)) => {
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            None => {}
        }
    }
}

// ============================================================================
// FONCTIONS UTILITAIRES
// ============================================================================

fn section(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::CollapsingHeader::new(RichText::new(title).strong())
        .default_open(true)
        .show(ui, |ui| {
            body(ui);
            ui.add_space(4.0);
        });
}

fn kind_color(ui: &egui::Ui, kind: Kind) -> egui::Color32 {
    match kind {
        Kind::Ok => form::ok_color(ui),
        Kind::Info => ui.visuals().text_color(),
        Kind::Warn => form::warn_color(ui),
        Kind::Error => form::error_color(ui),
    }
}

/// La configuration peut-elle être enregistrée par ce compte ?
///
/// En installation système, le dossier de configuration est créé par l'installateur avec ses
/// permissions : s'il n'existe pas, le logiciel n'est pas installé et l'utilitaire ne doit pas
/// le créer lui-même (il appartiendrait à un utilisateur standard, maître de ses permissions).
fn can_write_config(layout: &Layout) -> bool {
    let installed = layout.kind == LayoutKind::Portable
        || layout.config_file.parent().is_some_and(|d| d.is_dir());
    if !installed {
        return false;
    }
    // Windows, installation système : réservé aux administrateurs, quelles que soient les
    // permissions effectives du dossier (une installation antérieure à la correction des
    // permissions pouvait laisser les utilisateurs y créer des fichiers).
    if cfg!(windows) && layout.kind == LayoutKind::System {
        return control::is_elevated();
    }
    control::config_dir_writable(&layout.config_file)
}

/// Arrêt propre en mode manuel, puis attente de la libération du verrou d'instance (la base
/// est fermée et l'historique vidé avant la fin du processus).
fn stop_manual_and_wait(layout: &Layout, cfg: &AppConfig) -> Result<(), String> {
    probe::request_shutdown(layout, cfg)?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if probe::health(cfg).is_err() && instance_lock_free(layout) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    Err("le moteur ne s'est pas arrêté dans les 20 secondes : consultez les journaux".into())
}

/// Le verrou d'instance est-il libre ? (fichier absent ou verrouillable)
fn instance_lock_free(layout: &Layout) -> bool {
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(layout.lock_file())
    {
        Ok(f) => f.try_lock().is_ok(),
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

fn wait_service_stopped() -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if control::service_query().state == ServiceState::Stopped {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err("le service ne s'est pas arrêté dans les 30 secondes".into())
}

/// Commande du moteur sans élévation (mode portable), sortie récupérée.
fn run_engine(args: &[&str]) -> Result<(i32, String), String> {
    let engine = control::engine_path();
    let mut cmd = std::process::Command::new(&engine);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("« {} » introuvable : {e}", engine.display()))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout).trim(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok((
        out.status.code().unwrap_or(apex_config::exit::FAILURE),
        text,
    ))
}

fn explain_save_error(e: &ConfigError) -> String {
    match e {
        ConfigError::Write { path, source } if source.kind() == std::io::ErrorKind::PermissionDenied => format!(
            "Enregistrement refusé : votre compte ne peut pas écrire « {} ». {}",
            path.display(),
            if control::CAN_RELAUNCH_ELEVATED {
                "Relancez l'utilitaire en administrateur."
            } else {
                "Ajoutez votre compte au groupe apex-scada, ou utilisez sudo."
            }
        ),
        ConfigError::Conflict => {
            "Le fichier a été modifié par un autre programme depuis sa lecture : rechargez-le (vos modifications seront perdues) puis recommencez.".into()
        }
        ConfigError::Locked => "Le fichier est en cours d'enregistrement par un autre programme : réessayez dans un instant.".into(),
        other => format!("Enregistrement impossible : {other}"),
    }
}

/// Date lisible d'une sauvegarde `apex-scada-AAAAMMJJ-HHMMSS.mmm.toml`.
fn file_label(path: &std::path::Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let stamp = name
        .trim_start_matches("apex-scada-")
        .trim_end_matches(".toml");
    match chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%d-%H%M%S%.3f") {
        Ok(dt) => dt.format("%d/%m/%Y à %H:%M:%S").to_string(),
        Err(_) => name,
    }
}

fn human_duration(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{secs} s"),
        (0, 0, m) => format!("{m} min"),
        (0, h, m) => format!("{h} h {m:02} min"),
        (d, h, _) => format!("{d} j {h} h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_names_are_readable() {
        let p = std::path::Path::new("apex-scada-20261016-191408.878.toml");
        assert_eq!(file_label(p), "16/10/2026 à 19:14:08");
        assert_eq!(file_label(std::path::Path::new("autre.toml")), "autre.toml");
    }

    #[test]
    fn durations_are_readable() {
        assert_eq!(human_duration(42), "42 s");
        assert_eq!(human_duration(125), "2 min");
        assert_eq!(human_duration(3 * 3600 + 5 * 60), "3 h 05 min");
        assert_eq!(human_duration(2 * 86_400 + 4 * 3600), "2 j 4 h");
    }

    #[test]
    fn relaunch_arguments_keep_config_and_wizard() {
        let file = std::path::Path::new(r"C:\Sites\A b.toml");
        assert_eq!(
            relaunch_args(file, true),
            vec!["--config", r"C:\Sites\A b.toml", "--wizard"]
        );
        assert_eq!(relaunch_args(file, false).len(), 2);
    }

    #[test]
    fn lock_is_free_when_absent_or_released() {
        let base = std::env::temp_dir().join(format!("apex-admin-lock-{}", std::process::id()));
        let layout = Layout::portable(&base);
        assert!(instance_lock_free(&layout));
        std::fs::create_dir_all(&layout.data_dir).unwrap();
        let held = std::fs::File::create(layout.lock_file()).unwrap();
        held.lock().unwrap();
        assert!(!instance_lock_free(&layout));
        drop(held);
        assert!(instance_lock_free(&layout));
        let _ = std::fs::remove_dir_all(&base);
    }
}
