//! # Module `core` — Noyau Temps Réel (`ScadaCore`), Contrôle des Drivers, Alarmes, Planificateur & Audit
//!
//! Implémente l'ensemble des services centraux de **RustScadaFlow** :
//! - Gestion temps réel des Tags (`DashMap`) + Filtrage COV + Mise à l'échelle linéaire.
//! - Contrôle complet du cycle de vie des Drivers (`Start`, `Stop`, `Restart`, `Delete` via `AbortHandle`).
//! - Pause / Reprise globale de l'acquisition.
//! - Moteur d'Alarmes ISA-18.2 avec journal SQLite (`alarms_journal`).
//! - Planificateur Horaire GTB (`ScheduleRule`) évalué automatiquement.
//! - Journal d'Audit Opérateur (`audit_logs`) et Export/Import complet du projet en JSON.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use chrono::{DateTime, Datelike, Local, Utc};
use dashmap::DashMap;
use sqlx::{Row, Sqlite, SqlitePool};
use tokio::{
    sync::{broadcast, mpsc, Notify},
    task::AbortHandle,
};
use tracing::{error, info, warn};

use crate::{
    config::AppConfig,
    drivers::spawn_driver_task,
    flow_engine::FlowEngineRuntime,
    models::{
        is_valid_hhmm, AlarmInstance, AlarmJournalEntry, AlarmLifecycleState, AlarmSeverity,
        AuditLogEntry, ComparisonOperator, DashboardLayout, DashboardWidget, DriverConfig,
        DriverRuntimeStats, FlowEdge, FlowExecutionTrace, FlowGraph, FlowNode, FlowNodeKind,
        HistoryPoint, HistoryResponse, ImportSymbolLibraryRequest, LoginRequest, LoginResponse,
        ProjectBackupBundle, Quality, RegisteredTag, ScadaError, ScheduleRule, SymbolLibraryItem,
        SystemStatus, TagFullView, TagState, TagValue, UpsertUserRequest, UserAccount,
        UserPublicView, UserRole, WidgetType, WriteCommand,
    },
    security,
};

/// Version unique de l'application (issue de `Cargo.toml`).
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Taille maximale d'un lot d'écriture de l'historique.
const HISTORY_BATCH_MAX: usize = 500;
/// Capacité de la file d'attente de l'historique avant rejet des points.
const HISTORY_QUEUE_CAPACITY: usize = 20_000;
/// Nombre d'échecs de connexion avant verrouillage temporaire d'un identifiant.
const LOGIN_MAX_FAILURES: u32 = 5;
const LOGIN_LOCK_SECONDS: i64 = 60;

/// Comptes d'usine et leur mot de passe initial (pour l'avertissement au démarrage).
const FACTORY_ACCOUNTS: [(&str, &str); 3] = [
    ("admin", "admin123"),
    ("superviseur", "super123"),
    ("visu", "visu123"),
];

/// Session utilisateur authentifiée (en mémoire).
#[derive(Debug, Clone)]
pub struct Session {
    pub username: String,
    pub created_at: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

#[derive(Debug, Clone)]
struct LoginFailure {
    count: u32,
    last_at: DateTime<Utc>,
}

pub struct ScadaCore {
    pub started_at: DateTime<Utc>,
    pub modbus_slave_port: u16,
    pub session_ttl_hours: u64,
    pub history_retention_days: u32,
    acquisition_paused: AtomicBool,
    cov_counter: AtomicU64,
    history_dropped: AtomicU64,
    pub tags_registry: DashMap<String, RegisteredTag>,
    pub tags_state: DashMap<String, TagState>,
    /// Dernière valeur de qualité `Good` de chaque tag (sert à restaurer les consignes du simulateur).
    pub last_good_values: DashMap<String, TagValue>,
    driver_writers: DashMap<String, mpsc::Sender<WriteCommand>>,
    driver_handles: DashMap<String, AbortHandle>,
    pub drivers_config: DashMap<String, DriverConfig>,
    pub dashboards: DashMap<String, DashboardLayout>,
    pub flows: DashMap<String, FlowGraph>,
    pub alarms: DashMap<String, AlarmInstance>,
    pub schedules: DashMap<String, ScheduleRule>,
    pub users: DashMap<String, UserAccount>,
    pub sessions: DashMap<String, Session>,
    login_failures: DashMap<String, LoginFailure>,
    /// Jeton de contrôle local (scripts `.bat`), accepté uniquement depuis la boucle locale.
    pub local_control_token: String,
    pub symbols_library: DashMap<String, SymbolLibraryItem>,
    pub flow_runtime: FlowEngineRuntime,
    cov_tx: broadcast::Sender<TagState>,
    alarm_tx: broadcast::Sender<Vec<AlarmInstance>>,
    driver_tx: broadcast::Sender<Vec<DriverConfig>>,
    flow_trace_tx: broadcast::Sender<FlowExecutionTrace>,
    ingest_tx: mpsc::Sender<TagState>,
    history_tx: mpsc::Sender<TagState>,
    pub shutdown_notify: Arc<Notify>,
    pub restart_requested: AtomicBool,
    db_pool: Option<SqlitePool>,
}

impl ScadaCore {
    pub fn new(db_pool: Option<SqlitePool>, config: &AppConfig) -> Arc<Self> {
        let (cov_tx, _) = broadcast::channel::<TagState>(4096);
        let (alarm_tx, _) = broadcast::channel::<Vec<AlarmInstance>>(512);
        let (driver_tx, _) = broadcast::channel::<Vec<DriverConfig>>(256);
        let (flow_trace_tx, _) = broadcast::channel::<FlowExecutionTrace>(1024);
        let (ingest_tx, ingest_rx) = mpsc::channel::<TagState>(4096);
        let (history_tx, history_rx) = mpsc::channel::<TagState>(HISTORY_QUEUE_CAPACITY);

        let core = Arc::new(Self {
            started_at: Utc::now(),
            modbus_slave_port: config.modbus_slave_port,
            session_ttl_hours: config.session_ttl_hours,
            history_retention_days: config.history_retention_days,
            acquisition_paused: AtomicBool::new(false),
            cov_counter: AtomicU64::new(0),
            history_dropped: AtomicU64::new(0),
            tags_registry: DashMap::new(),
            tags_state: DashMap::new(),
            last_good_values: DashMap::new(),
            driver_writers: DashMap::new(),
            driver_handles: DashMap::new(),
            drivers_config: DashMap::new(),
            dashboards: DashMap::new(),
            flows: DashMap::new(),
            alarms: DashMap::new(),
            schedules: DashMap::new(),
            users: DashMap::new(),
            sessions: DashMap::new(),
            login_failures: DashMap::new(),
            local_control_token: security::generate_token(),
            symbols_library: DashMap::new(),
            flow_runtime: FlowEngineRuntime::new(),
            cov_tx,
            alarm_tx,
            driver_tx,
            flow_trace_tx,
            ingest_tx,
            history_tx,
            shutdown_notify: Arc::new(Notify::new()),
            restart_requested: AtomicBool::new(false),
            db_pool,
        });

        core.seed_industrial_defaults();

        Self::spawn_ingestion_worker(Arc::clone(&core), ingest_rx);
        Self::spawn_history_writer(Arc::clone(&core), history_rx);
        Self::spawn_history_retention_worker(Arc::clone(&core));
        Self::spawn_session_cleanup_worker(Arc::clone(&core));
        Self::spawn_driver_stats_broadcaster(Arc::clone(&core));
        Self::spawn_shelve_expiry_worker(Arc::clone(&core));

        core
    }

    /// Démarre les automatismes (moteur de flux et planificateur horaire). Appelé une fois les
    /// drivers lancés, pour que les premières commandes trouvent les équipements disponibles.
    pub fn start_automation(self: &Arc<Self>) {
        FlowEngineRuntime::spawn_worker(Arc::clone(self));
        let core = Arc::clone(self);
        tokio::spawn(async move {
            // Laisse aux drivers le temps d'établir leur première connexion
            tokio::time::sleep(Duration::from_secs(3)).await;
            Self::spawn_scheduler_worker(core);
        });
    }

    pub fn driver_ingest_sender(&self) -> mpsc::Sender<TagState> {
        self.ingest_tx.clone()
    }

    pub fn subscribe_cov(&self) -> broadcast::Receiver<TagState> {
        self.cov_tx.subscribe()
    }

    pub fn subscribe_alarms(&self) -> broadcast::Receiver<Vec<AlarmInstance>> {
        self.alarm_tx.subscribe()
    }

    pub fn subscribe_drivers(&self) -> broadcast::Receiver<Vec<DriverConfig>> {
        self.driver_tx.subscribe()
    }

    pub fn subscribe_flow_traces(&self) -> broadcast::Receiver<FlowExecutionTrace> {
        self.flow_trace_tx.subscribe()
    }

    pub fn emit_flow_trace(&self, trace: FlowExecutionTrace) {
        let _ = self.flow_trace_tx.send(trace);
    }

    pub fn is_acquisition_paused(&self) -> bool {
        self.acquisition_paused.load(Ordering::Relaxed)
    }

    pub fn set_acquisition_paused(&self, actor: &str, paused: bool) {
        self.acquisition_paused.store(paused, Ordering::SeqCst);
        self.record_audit(
            actor,
            "SYSTÈME",
            if paused {
                "PAUSE_ACQUISITION"
            } else {
                "REPRISE_ACQUISITION"
            },
            "ScadaCore",
            if paused {
                "Acquisition globale mise en pause"
            } else {
                "Acquisition globale reprise"
            },
        );
    }

    pub fn get_system_status(&self) -> SystemStatus {
        let now = Utc::now();
        let uptime = (now - self.started_at).num_seconds().max(0) as u64;
        let running_drivers = self
            .drivers_config
            .iter()
            .filter(|d| d.value().stats.running)
            .count();
        let active_flows = self.flows.iter().filter(|f| f.value().enabled).count();
        let active_alarms = self
            .alarms
            .iter()
            .filter(|a| {
                a.value().state == AlarmLifecycleState::ActiveUnack
                    || a.value().state == AlarmLifecycleState::ActiveAck
            })
            .count();

        SystemStatus {
            app_name: "RustScadaFlow Industrial Platform".to_string(),
            version: APP_VERSION.to_string(),
            pid: std::process::id(),
            started_at: self.started_at,
            uptime_seconds: uptime,
            acquisition_paused: self.is_acquisition_paused(),
            total_tags: self.tags_registry.len(),
            total_drivers: self.drivers_config.len(),
            running_drivers,
            active_flows,
            active_alarms,
            cov_events_total: self.cov_counter.load(Ordering::Relaxed),
            modbus_slave_port: self.modbus_slave_port,
            history_dropped: self.history_dropped.load(Ordering::Relaxed),
            history_retention_days: self.history_retention_days,
        }
    }

    // ========================================================================
    // CONTRÔLE DU CYCLE DE VIE DES DRIVERS (START / STOP / RESTART / DELETE)
    // ========================================================================

    /// Enregistre (ou remplace) un driver industriel, le persiste et le démarre s'il est actif.
    ///
    /// En cas d'échec du démarrage (endpoint invalide...), le driver reste déclaré mais arrêté,
    /// avec l'erreur visible dans ses statistiques, et l'erreur est renvoyée à l'appelant.
    pub async fn register_and_start_driver(
        self: &Arc<Self>,
        actor: &str,
        mut config: DriverConfig,
    ) -> Result<Vec<TagFullView>, ScadaError> {
        security::validate_id("driver", &config.id)?;
        for tag in &config.tags {
            security::validate_id("tag", &tag.id)?;
        }
        let driver_id = config.id.clone();
        self.abort_driver_task(&driver_id);

        let mut views = Vec::with_capacity(config.tags.len());
        for tag in config.tags.iter_mut() {
            tag.module_source = driver_id.clone();
            views.push(self.register_tag(tag.clone(), None));
        }

        config.stats = DriverRuntimeStats::default();
        let start_result = if config.enabled {
            self.spawn_and_track(&mut config).await
        } else {
            Ok(())
        };

        self.drivers_config.insert(driver_id.clone(), config);
        self.persist_driver(&driver_id).await;
        let _ = self.driver_tx.send(self.get_all_drivers());
        self.record_audit(
            actor,
            "DRIVER",
            "REGISTER",
            &driver_id,
            "Déclaration / mise à jour d'un driver industriel",
        );

        start_result.map(|_| views)
    }

    /// Démarre la tâche d'acquisition et met à jour les statistiques de la configuration.
    async fn spawn_and_track(self: &Arc<Self>, config: &mut DriverConfig) -> Result<(), ScadaError> {
        match spawn_driver_task(Arc::clone(self), config.clone()).await {
            Ok((abort_handle, writer_tx)) => {
                self.driver_handles.insert(config.id.clone(), abort_handle);
                self.driver_writers.insert(config.id.clone(), writer_tx);
                config.stats.running = true;
                // Les drivers internes sont immédiatement « connectés » ; les drivers réseau
                // le deviennent à leur première trame réussie (record_driver_poll).
                config.stats.connected = matches!(
                    config.protocol,
                    crate::models::ProtocolType::Simulator
                        | crate::models::ProtocolType::OpcUa
                        | crate::models::ProtocolType::BacnetIp
                        | crate::models::ProtocolType::SiemensS7
                );
                config.stats.last_error = None;
                config.stats.last_poll_at = Some(Utc::now());
                Ok(())
            }
            Err(err) => {
                config.stats.running = false;
                config.stats.connected = false;
                config.stats.last_error = Some(err.clone());
                warn!("Démarrage du driver '{}' impossible : {}", config.id, err);
                Err(ScadaError::DriverError(err))
            }
        }
    }

    fn abort_driver_task(&self, driver_id: &str) {
        if let Some((_, handle)) = self.driver_handles.remove(driver_id) {
            handle.abort();
        }
        self.driver_writers.remove(driver_id);
    }

    /// Démarre au lancement de l'application tous les drivers persistés dont l'état voulu est « actif ».
    pub async fn start_persisted_drivers(self: &Arc<Self>) {
        let ids: Vec<String> = self
            .drivers_config
            .iter()
            .filter(|d| d.value().enabled)
            .map(|d| d.key().clone())
            .collect();
        for id in ids {
            let Some(mut cfg) = self.drivers_config.get(&id).map(|d| d.clone()) else {
                continue;
            };
            cfg.tags = self.get_tags_for_driver(&id);
            let _ = self.spawn_and_track(&mut cfg).await;
            self.drivers_config.insert(id.clone(), cfg);
            info!("Driver '{}' démarré depuis la configuration persistée", id);
        }
        let _ = self.driver_tx.send(self.get_all_drivers());
    }

    /// Arrête la tâche d'acquisition d'un driver sans le supprimer (état voulu : arrêté).
    pub async fn stop_driver(&self, actor: &str, driver_id: &str) -> Result<DriverConfig, ScadaError> {
        self.abort_driver_task(driver_id);

        let mut drv = self
            .drivers_config
            .get_mut(driver_id)
            .ok_or_else(|| ScadaError::DriverNotFound(driver_id.to_string()))?;

        drv.enabled = false;
        drv.stats.running = false;
        drv.stats.connected = false;
        let updated = drv.clone();
        drop(drv);

        self.mark_driver_tags_uncertain(driver_id);
        // Arrêt volontaire : la rupture de communication n'est plus une anomalie
        self.update_comm_alarm(driver_id, true);
        self.persist_driver(driver_id).await;
        let _ = self.driver_tx.send(self.get_all_drivers());
        self.record_audit(
            actor,
            "DRIVER",
            "STOP",
            driver_id,
            "Arrêt manuel du driver industriel",
        );

        Ok(updated)
    }

    /// Passe les tags d'un driver arrêté en qualité `Uncertain` (dernière valeur conservée).
    fn mark_driver_tags_uncertain(&self, driver_id: &str) {
        for tag in self.get_tags_for_driver(driver_id) {
            if let Some(st) = self.tags_state.get(&tag.id).map(|s| s.clone()) {
                self.process_incoming_state(TagState {
                    tag_id: tag.id,
                    value: st.value,
                    quality: Quality::Uncertain,
                    timestamp: Utc::now(),
                });
            }
        }
    }

    /// Démarre ou redémarre à chaud un driver industriel (état voulu : actif).
    pub async fn start_or_restart_driver(
        self: &Arc<Self>,
        actor: &str,
        driver_id: &str,
    ) -> Result<DriverConfig, ScadaError> {
        self.abort_driver_task(driver_id);

        let mut config = self
            .drivers_config
            .get(driver_id)
            .map(|d| d.clone())
            .ok_or_else(|| ScadaError::DriverNotFound(driver_id.to_string()))?;

        config.enabled = true;
        config.tags = self.get_tags_for_driver(driver_id);
        config.stats = DriverRuntimeStats::default();
        let start_result = self.spawn_and_track(&mut config).await;

        self.drivers_config
            .insert(driver_id.to_string(), config.clone());
        self.persist_driver(driver_id).await;
        let _ = self.driver_tx.send(self.get_all_drivers());

        self.record_audit(
            actor,
            "DRIVER",
            "START_RESTART",
            driver_id,
            if start_result.is_ok() {
                "Démarrage / Redémarrage à chaud du driver"
            } else {
                "Échec du démarrage du driver"
            },
        );

        start_result.map(|_| config)
    }

    /// Modifie les paramètres de communication d'un driver (nom, endpoint, esclave, période),
    /// puis le redémarre s'il était actif pour appliquer la nouvelle configuration.
    pub async fn update_driver(
        self: &Arc<Self>,
        actor: &str,
        driver_id: &str,
        req: crate::models::DriverUpdateRequest,
    ) -> Result<DriverConfig, ScadaError> {
        let mut cfg = self
            .drivers_config
            .get(driver_id)
            .map(|d| d.clone())
            .ok_or_else(|| ScadaError::DriverNotFound(driver_id.to_string()))?;

        let mut changes: Vec<String> = Vec::new();
        if let Some(name) = req.name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()) {
            if name != cfg.name {
                changes.push(format!("nom « {} »", name));
                cfg.name = name;
            }
        }
        if let Some(ep) = req.endpoint.map(|e| e.trim().to_string()).filter(|e| !e.is_empty()) {
            if cfg.protocol == crate::models::ProtocolType::ModbusTcp
                && ep.parse::<std::net::SocketAddr>().is_err()
            {
                return Err(ScadaError::BadRequest(format!(
                    "Endpoint Modbus TCP invalide '{}' (attendu IP:port, ex. 192.168.1.10:502)",
                    ep
                )));
            }
            if ep != cfg.endpoint {
                changes.push(format!("endpoint {} → {}", cfg.endpoint, ep));
                cfg.endpoint = ep;
            }
        }
        if let Some(unit) = req.unit_id {
            if unit != cfg.unit_id {
                changes.push(format!("esclave {} → {}", cfg.unit_id, unit));
                cfg.unit_id = unit;
            }
        }
        if let Some(poll) = req.poll_interval_ms {
            let poll = poll.clamp(100, 3_600_000);
            if poll != cfg.poll_interval_ms {
                changes.push(format!("scrutation {} → {} ms", cfg.poll_interval_ms, poll));
                cfg.poll_interval_ms = poll;
            }
        }
        if changes.is_empty() {
            return Ok(cfg);
        }

        self.drivers_config.insert(driver_id.to_string(), cfg.clone());
        self.persist_driver(driver_id).await;
        self.record_audit(actor, "DRIVER", "UPDATE", driver_id, &changes.join(", "));

        if cfg.enabled {
            // Redémarrage à chaud avec la nouvelle configuration ; l'alarme de communication
            // éventuelle sera réévaluée à la première trame.
            self.start_or_restart_driver(actor, driver_id).await
        } else {
            let _ = self.driver_tx.send(self.get_all_drivers());
            Ok(cfg)
        }
    }

    /// Supprime un driver, arrête sa tâche et supprime les variables qui lui sont rattachées.
    pub async fn delete_driver(&self, actor: &str, driver_id: &str) -> Result<usize, ScadaError> {
        self.abort_driver_task(driver_id);
        self.drivers_config
            .remove(driver_id)
            .ok_or_else(|| ScadaError::DriverNotFound(driver_id.to_string()))?;

        let tag_ids: Vec<String> = self
            .get_tags_for_driver(driver_id)
            .into_iter()
            .map(|t| t.id)
            .collect();
        for tag_id in &tag_ids {
            self.forget_tag(tag_id);
        }
        self.alarms.remove(&comm_alarm_id(driver_id));

        if let Some(pool) = &self.db_pool {
            log_db_error(
                "suppression driver",
                sqlx::query("DELETE FROM drivers_config WHERE id = ?")
                    .bind(driver_id)
                    .execute(pool)
                    .await,
            );
            for tag_id in &tag_ids {
                log_db_error(
                    "suppression tag du driver",
                    sqlx::query("DELETE FROM tags_catalog WHERE id = ?")
                        .bind(tag_id)
                        .execute(pool)
                        .await,
                );
            }
        }
        let _ = self.driver_tx.send(self.get_all_drivers());
        let _ = self.alarm_tx.send(self.get_all_alarms());
        self.record_audit(
            actor,
            "DRIVER",
            "DELETE",
            driver_id,
            &format!("Suppression du driver et de ses {} variable(s)", tag_ids.len()),
        );
        Ok(tag_ids.len())
    }

    /// Arrête toutes les tâches d'acquisition (extinction, mise hors tension) sans modifier
    /// l'état voulu des drivers : ils redémarreront au prochain rallumage.
    pub fn stop_all_drivers(&self) {
        let ids: Vec<String> = self.driver_handles.iter().map(|e| e.key().clone()).collect();
        for id in ids {
            self.abort_driver_task(&id);
            if let Some(mut drv) = self.drivers_config.get_mut(&id) {
                drv.stats.running = false;
                drv.stats.connected = false;
            }
        }
        self.driver_writers.clear();
        let _ = self.driver_tx.send(self.get_all_drivers());
        info!("Tous les drivers industriels ont été arrêtés proprement.");
    }

    /// Met le moteur SCADA complet hors tension (arrêt de tous les drivers + pause acquisition) tout en gardant l'IHM disponible pour un rallumage à distance.
    pub fn power_off_all_systems(&self, actor: &str) {
        self.set_acquisition_paused(actor, true);
        self.stop_all_drivers();
        self.record_audit(
            actor,
            "SERVEUR",
            "POWER_OFF",
            "RustScadaFlow",
            "Mise hors tension complète du moteur SCADA et arrêt des drivers",
        );
    }

    /// Rallume le moteur SCADA : reprise de l'acquisition et redémarrage des seuls drivers
    /// dont l'état voulu est « actif » (un driver arrêté volontairement reste arrêté).
    pub async fn restart_all_systems(self: &Arc<Self>, actor: &str) {
        self.stop_all_drivers();
        self.acquisition_paused.store(false, Ordering::SeqCst);
        let ids: Vec<String> = self
            .drivers_config
            .iter()
            .filter(|e| e.value().enabled)
            .map(|e| e.key().clone())
            .collect();
        for id in ids {
            if let Err(err) = self.start_or_restart_driver(actor, &id).await {
                warn!("Redémarrage du driver '{}' impossible : {}", id, err);
            }
        }
        self.record_audit(
            actor,
            "SERVEUR",
            "RESTART",
            "RustScadaFlow",
            "Redémarrage du moteur SCADA et des drivers actifs",
        );
    }

    pub fn record_driver_poll(
        &self,
        driver_id: &str,
        connected: bool,
        latency_ms: u64,
        err: Option<String>,
    ) {
        let was_connected = {
            let Some(mut drv) = self.drivers_config.get_mut(driver_id) else {
                return;
            };
            let prev = drv.stats.connected;
            drv.stats.running = true;
            drv.stats.connected = connected;
            drv.stats.last_latency_ms = latency_ms;
            drv.stats.last_poll_at = Some(Utc::now());
            if connected {
                drv.stats.frames_ok += 1;
                drv.stats.last_error = None;
            } else {
                drv.stats.frames_err += 1;
                drv.stats.last_error = err;
            }
            prev
        };
        // Une seule alarme par driver : apparition au premier échec, disparition au retour
        if !connected {
            self.update_comm_alarm(driver_id, false);
        } else if !was_connected {
            self.update_comm_alarm(driver_id, true);
        }
    }

    pub fn get_tags_for_driver(&self, driver_id: &str) -> Vec<RegisteredTag> {
        let mut list: Vec<RegisteredTag> = self
            .tags_registry
            .iter()
            .filter(|e| e.value().module_source == driver_id)
            .map(|e| e.value().clone())
            .collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    /// Persiste la configuration d'un driver avec la liste à jour de ses variables.
    /// L'écriture est attendue : deux modifications successives sont persistées dans l'ordre.
    async fn persist_driver(&self, driver_id: &str) {
        let Some(pool) = &self.db_pool else {
            return;
        };
        let Some(mut cfg) = self.drivers_config.get(driver_id).map(|d| d.clone()) else {
            return;
        };
        cfg.tags = self.get_tags_for_driver(driver_id);
        cfg.stats = DriverRuntimeStats::default();
        match serde_json::to_string(&cfg) {
            Ok(json_str) => log_db_error(
                "persistance driver",
                sqlx::query(
                    "INSERT INTO drivers_config (id, config_json, updated_at) VALUES (?, ?, ?)
                     ON CONFLICT(id) DO UPDATE SET config_json = excluded.config_json, updated_at = excluded.updated_at",
                )
                .bind(&cfg.id)
                .bind(json_str)
                .bind(Utc::now().to_rfc3339())
                .execute(pool)
                .await,
            ),
            Err(e) => warn!("Sérialisation du driver '{}' impossible : {}", cfg.id, e),
        }
    }

    // ========================================================================
    // GESTION DES VARIABLES (TAG DATABASE CRUD)
    // ========================================================================

    pub fn register_tag(
        &self,
        tag: RegisteredTag,
        initial_value: Option<TagValue>,
    ) -> TagFullView {
        let tag_id = tag.id.clone();
        let default_val = initial_value.unwrap_or_else(|| match tag.data_type {
            crate::models::TagDataType::Bool => TagValue::Bool(false),
            crate::models::TagDataType::Float => TagValue::Float(0.0),
            crate::models::TagDataType::Int => TagValue::Int(0),
            crate::models::TagDataType::Text => TagValue::Text(String::new()),
        });

        self.tags_registry.insert(tag_id.clone(), tag.clone());

        let state_entry = self
            .tags_state
            .entry(tag_id.clone())
            .or_insert_with(|| TagState {
                tag_id,
                value: default_val,
                quality: Quality::Uncertain,
                timestamp: Utc::now(),
            });

        TagFullView {
            metadata: tag,
            state: Some(state_entry.value().clone()),
        }
    }

    pub async fn upsert_tag(&self, actor: &str, tag: RegisteredTag) -> Result<TagFullView, ScadaError> {
        security::validate_id("tag", &tag.id)?;
        if tag.module_source.trim().is_empty() {
            return Err(ScadaError::BadRequest(
                "Le driver (module source) du tag est requis".to_string(),
            ));
        }
        if !tag.scale_factor.is_finite() || tag.scale_factor.abs() < 1e-9 {
            return Err(ScadaError::BadRequest(
                "Le facteur d'échelle doit être un nombre non nul".to_string(),
            ));
        }

        // Un changement de driver retire le tag de l'ancienne configuration
        let previous_source = self
            .tags_registry
            .get(&tag.id)
            .map(|t| t.module_source.clone());

        let view = self.register_tag(tag.clone(), None);

        if let Some(pool) = &self.db_pool {
            let json_str = serde_json::to_string(&tag)
                .map_err(|e| ScadaError::DatabaseError(e.to_string()))?;
            sqlx::query(
                "INSERT INTO tags_catalog (id, tag_json, updated_at) VALUES (?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET tag_json = excluded.tag_json, updated_at = excluded.updated_at",
            )
            .bind(&tag.id)
            .bind(json_str)
            .bind(Utc::now().to_rfc3339())
            .execute(pool)
            .await
            .map_err(|e| ScadaError::DatabaseError(e.to_string()))?;
        }

        self.persist_driver(&tag.module_source).await;
        if let Some(prev) = previous_source.filter(|p| p != &tag.module_source) {
            self.persist_driver(&prev).await;
        }

        self.record_audit(
            actor,
            "TAG_DB",
            "UPSERT_TAG",
            &tag.id,
            &format!("Configuration variable {} ({})", tag.label, tag.protocol_address),
        );

        Ok(view)
    }

    /// Retire un tag de toutes les structures temps réel (sans toucher à la base).
    fn forget_tag(&self, tag_id: &str) -> Option<RegisteredTag> {
        let removed = self.tags_registry.remove(tag_id).map(|(_, t)| t);
        self.tags_state.remove(tag_id);
        self.last_good_values.remove(tag_id);
        self.alarms.remove(tag_id);
        removed
    }

    pub async fn delete_tag(&self, actor: &str, tag_id: &str) -> Result<(), ScadaError> {
        let removed = self
            .forget_tag(tag_id)
            .ok_or_else(|| ScadaError::TagNotFound(tag_id.to_string()))?;

        if let Some(pool) = &self.db_pool {
            log_db_error(
                "suppression tag",
                sqlx::query("DELETE FROM tags_catalog WHERE id = ?")
                    .bind(tag_id)
                    .execute(pool)
                    .await,
            );
        }
        self.persist_driver(&removed.module_source).await;
        let _ = self.alarm_tx.send(self.get_all_alarms());
        self.record_audit(actor, "TAG_DB", "DELETE_TAG", tag_id, "Suppression variable");
        Ok(())
    }

    // ========================================================================
    // INGESTION COV, ALARMES ISA-18.2 & HISTORIAN
    // ========================================================================

    /// Point d'entrée des valeurs brutes remontées par les drivers : applique la mise à l'échelle
    /// linéaire (`valeur = brute × facteur + offset`) puis le filtrage COV.
    pub fn process_incoming_state(&self, incoming: TagState) -> bool {
        self.ingest_state(incoming, true)
    }

    fn ingest_state(&self, mut incoming: TagState, apply_scaling: bool) -> bool {
        let tag_meta_opt = self.tags_registry.get(&incoming.tag_id).map(|t| t.clone());

        if let Some(meta) = &tag_meta_opt {
            let has_scaling =
                (meta.scale_factor - 1.0).abs() > 1e-9 || meta.scale_offset.abs() > 1e-9;
            if apply_scaling && has_scaling && meta.data_type == crate::models::TagDataType::Float {
                if let (TagValue::Float(_) | TagValue::Int(_), Some(raw)) =
                    (&incoming.value, incoming.value.as_f64())
                {
                    let scaled = ((raw * meta.scale_factor + meta.scale_offset) * 1000.0).round() / 1000.0;
                    incoming.value = TagValue::Float(scaled);
                }
            }
        }

        // Perte de communication : on garde la dernière valeur connue (qualité Bad) au lieu de
        // la valeur de repli du driver (0, false...), comme un SCADA classique.
        if incoming.quality == Quality::BadDisconnect {
            if let Some(existing) = self.tags_state.get(&incoming.tag_id) {
                incoming.value = existing.value.clone();
            }
        }

        let deadband = tag_meta_opt.as_ref().map(|t| t.deadband).unwrap_or(0.02);
        let historize = tag_meta_opt.as_ref().map(|t| t.historize).unwrap_or(true);

        let should_broadcast = match self.tags_state.get(&incoming.tag_id) {
            Some(existing) => {
                existing.quality != incoming.quality
                    || incoming
                        .value
                        .has_significant_change(&existing.value, deadband)
            }
            None => true,
        };

        if incoming.quality == Quality::Good {
            self.last_good_values
                .insert(incoming.tag_id.clone(), incoming.value.clone());
        }

        if should_broadcast {
            self.cov_counter.fetch_add(1, Ordering::Relaxed);
            self.tags_state
                .insert(incoming.tag_id.clone(), incoming.clone());
            let _ = self.cov_tx.send(incoming.clone());

            if let Some(meta) = &tag_meta_opt {
                self.evaluate_alarm_for_tag(meta, &incoming);
            }

            if historize && self.db_pool.is_some() {
                // File bornée et non bloquante : l'écrivain groupé gère la persistance.
                if self.history_tx.try_send(incoming).is_err() {
                    self.history_dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        should_broadcast
    }

    /// Évalue les seuils d'alarme d'un tag.
    ///
    /// Seule une valeur de qualité `Good` est évaluée : une valeur douteuse ou une perte de
    /// communication ne fait ni apparaître ni disparaître d'alarme de seuil (la rupture de
    /// communication fait l'objet d'une alarme unique par driver, voir `update_comm_alarm`).
    fn evaluate_alarm_for_tag(&self, meta: &RegisteredTag, state: &TagState) {
        if state.quality != Quality::Good {
            return;
        }
        let currently_active = self.alarms.get(&meta.id).is_some_and(|a| {
            matches!(
                a.state,
                AlarmLifecycleState::ActiveUnack | AlarmLifecycleState::ActiveAck
            )
        });
        let reason = compute_alarm_reason(meta, &state.value, currently_active);
        self.apply_alarm_transition(
            &meta.id,
            &meta.id,
            &meta.label,
            reason,
            state.value.clone(),
        );
    }

    /// Applique le cycle de vie ISA-18.2 à une alarme (apparition, aggravation, disparition,
    /// retour à la normale), journalise l'événement et diffuse la liste si elle a changé.
    fn apply_alarm_transition(
        &self,
        alarm_id: &str,
        source_id: &str,
        label: &str,
        reason: Option<(String, AlarmSeverity)>,
        trigger_value: TagValue,
    ) {
        let mut changed = false;
        let mut journal_event: Option<(&'static str, AlarmSeverity, String)> = None;

        match (reason, self.alarms.get_mut(alarm_id)) {
            (Some((msg, sev)), Some(mut existing)) => {
                let was_active = matches!(
                    existing.state,
                    AlarmLifecycleState::ActiveUnack | AlarmLifecycleState::ActiveAck
                );
                if !was_active {
                    existing.trigger_value = trigger_value;
                    existing.state = AlarmLifecycleState::ActiveUnack;
                    existing.raised_at = Utc::now();
                    existing.cleared_at = None;
                    existing.acked_at = None;
                    existing.acked_by = None;
                    existing.ack_comment = None;
                    changed = true;
                    journal_event = Some(("APPARITION", sev, msg.clone()));
                } else if severity_rank(sev) > severity_rank(existing.severity) {
                    // Aggravation (ex. H → HH) : l'alarme doit être réacquittée
                    existing.trigger_value = trigger_value;
                    existing.state = AlarmLifecycleState::ActiveUnack;
                    existing.acked_at = None;
                    existing.acked_by = None;
                    changed = true;
                    journal_event = Some(("AGGRAVATION", sev, msg.clone()));
                } else if existing.message != msg {
                    changed = true;
                }
                existing.message = msg;
                existing.severity = sev;
            }
            (Some((msg, sev)), None) => {
                let instance = AlarmInstance {
                    id: alarm_id.to_string(),
                    tag_id: source_id.to_string(),
                    tag_label: label.to_string(),
                    severity: sev,
                    state: AlarmLifecycleState::ActiveUnack,
                    message: msg.clone(),
                    trigger_value,
                    raised_at: Utc::now(),
                    cleared_at: None,
                    acked_at: None,
                    acked_by: None,
                    ack_comment: None,
                    shelved_until: None,
                    shelved_by: None,
                };
                self.alarms.insert(alarm_id.to_string(), instance);
                changed = true;
                journal_event = Some(("APPARITION", sev, msg));
            }
            (None, Some(mut existing)) => match existing.state {
                AlarmLifecycleState::ActiveUnack => {
                    existing.state = AlarmLifecycleState::ClearedUnack;
                    existing.cleared_at = Some(Utc::now());
                    changed = true;
                    journal_event = Some(("DISPARITION", existing.severity, existing.message.clone()));
                }
                AlarmLifecycleState::ActiveAck => {
                    existing.state = AlarmLifecycleState::Normal;
                    existing.cleared_at = Some(Utc::now());
                    changed = true;
                    journal_event = Some(("RETOUR_NORMAL", existing.severity, existing.message.clone()));
                }
                _ => {}
            },
            (None, None) => {}
        }

        if let Some((ev_type, sev, msg)) = journal_event {
            self.record_alarm_journal(source_id, &format!("{:?}", sev), ev_type, &msg, None);
        }

        if changed {
            let _ = self.alarm_tx.send(self.get_all_alarms());
        }
    }

    /// Alarme unique de rupture de communication par driver (au lieu d'une alarme par tag),
    /// pour éviter les avalanches d'alarmes (ISA-18.2 § gestion des inondations d'alarmes).
    fn update_comm_alarm(&self, driver_id: &str, connected: bool) {
        let alarm_id = comm_alarm_id(driver_id);
        let already_active = self.alarms.get(&alarm_id).is_some_and(|a| {
            matches!(
                a.state,
                AlarmLifecycleState::ActiveUnack | AlarmLifecycleState::ActiveAck
            )
        });
        if !connected && already_active {
            return;
        }
        if connected && !self.alarms.contains_key(&alarm_id) {
            return;
        }
        let name = self
            .drivers_config
            .get(driver_id)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| driver_id.to_string());
        let reason = (!connected).then(|| {
            (
                // Message stable et court : le détail technique reste visible sur la carte du driver
                format!("Rupture de communication avec l'équipement '{}'", driver_id),
                AlarmSeverity::Critical,
            )
        });
        self.apply_alarm_transition(
            &alarm_id,
            driver_id,
            &name,
            reason,
            TagValue::Bool(connected),
        );
    }

    pub fn acknowledge_alarm(
        &self,
        alarm_id: &str,
        operator: &str,
        comment: Option<String>,
    ) -> Result<AlarmInstance, ScadaError> {
        let mut alarm = self
            .alarms
            .get_mut(alarm_id)
            .ok_or_else(|| ScadaError::AlarmNotFound(alarm_id.to_string()))?;

        alarm.acked_at = Some(Utc::now());
        alarm.acked_by = Some(operator.to_string());
        alarm.ack_comment = comment.clone();

        alarm.state = match alarm.state {
            AlarmLifecycleState::ActiveUnack => AlarmLifecycleState::ActiveAck,
            AlarmLifecycleState::ClearedUnack => AlarmLifecycleState::Normal,
            other => other,
        };

        let updated = alarm.clone();
        drop(alarm);

        self.record_alarm_journal(
            &updated.tag_id,
            &format!("{:?}", updated.severity),
            "ACQUITTEMENT",
            &updated.message,
            Some(operator.to_string()),
        );
        self.record_audit(
            operator,
            "ALARME",
            "ACK_ALARM",
            alarm_id,
            comment.as_deref().unwrap_or("Acquittement alarme"),
        );

        let _ = self.alarm_tx.send(self.get_all_alarms());
        Ok(updated)
    }

    /// Met une alarme en sommeil (ISA-18.2 « shelving ») pour une durée limitée (8 h maximum) :
    /// elle reste suivie et journalisée mais n'apparaît plus dans le bandeau ni les compteurs.
    pub fn shelve_alarm(
        &self,
        alarm_id: &str,
        operator: &str,
        minutes: u32,
        comment: Option<String>,
    ) -> Result<AlarmInstance, ScadaError> {
        if !(1..=480).contains(&minutes) {
            return Err(ScadaError::BadRequest(
                "La durée de mise en sommeil doit être comprise entre 1 et 480 minutes".to_string(),
            ));
        }
        let until = Utc::now() + chrono::Duration::minutes(minutes as i64);
        let updated = {
            let mut alarm = self
                .alarms
                .get_mut(alarm_id)
                .ok_or_else(|| ScadaError::AlarmNotFound(alarm_id.to_string()))?;
            alarm.shelved_until = Some(until);
            alarm.shelved_by = Some(operator.to_string());
            alarm.clone()
        };
        let detail = format!(
            "Mise en sommeil {} min{}",
            minutes,
            comment
                .as_deref()
                .filter(|c| !c.trim().is_empty())
                .map(|c| format!(" — {}", c.trim()))
                .unwrap_or_default()
        );
        self.record_alarm_journal(
            &updated.tag_id,
            &format!("{:?}", updated.severity),
            "MISE_EN_SOMMEIL",
            &detail,
            Some(operator.to_string()),
        );
        self.record_audit(operator, "ALARME", "SHELVE_ALARM", alarm_id, &detail);
        let _ = self.alarm_tx.send(self.get_all_alarms());
        Ok(updated)
    }

    pub fn unshelve_alarm(&self, alarm_id: &str, operator: &str) -> Result<AlarmInstance, ScadaError> {
        let updated = {
            let mut alarm = self
                .alarms
                .get_mut(alarm_id)
                .ok_or_else(|| ScadaError::AlarmNotFound(alarm_id.to_string()))?;
            alarm.shelved_until = None;
            alarm.shelved_by = None;
            alarm.clone()
        };
        self.record_alarm_journal(
            &updated.tag_id,
            &format!("{:?}", updated.severity),
            "FIN_SOMMEIL",
            "Réveil manuel de l'alarme",
            Some(operator.to_string()),
        );
        self.record_audit(operator, "ALARME", "UNSHELVE_ALARM", alarm_id, "Réveil manuel de l'alarme");
        let _ = self.alarm_tx.send(self.get_all_alarms());
        Ok(updated)
    }

    /// Réveille automatiquement les alarmes dont la mise en sommeil a expiré (toutes les 20 s).
    fn spawn_shelve_expiry_worker(core: Arc<Self>) {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(20));
            loop {
                ticker.tick().await;
                let now = Utc::now();
                let mut expired: Vec<(String, String)> = Vec::new();
                for mut a in core.alarms.iter_mut() {
                    if a.shelved_until.is_some_and(|u| u <= now) {
                        a.shelved_until = None;
                        a.shelved_by = None;
                        expired.push((a.tag_id.clone(), format!("{:?}", a.severity)));
                    }
                }
                for (tag_id, sev) in &expired {
                    core.record_alarm_journal(tag_id, sev, "FIN_SOMMEIL", "Fin de la mise en sommeil (expiration)", None);
                }
                if !expired.is_empty() {
                    let _ = core.alarm_tx.send(core.get_all_alarms());
                }
            }
        });
    }

    pub fn acknowledge_all_alarms(&self, operator: &str) -> Vec<AlarmInstance> {
        for mut entry in self.alarms.iter_mut() {
            let al = entry.value_mut();
            if al.state == AlarmLifecycleState::ActiveUnack
                || al.state == AlarmLifecycleState::ClearedUnack
            {
                al.acked_at = Some(Utc::now());
                al.acked_by = Some(operator.to_string());
                al.state = if al.state == AlarmLifecycleState::ActiveUnack {
                    AlarmLifecycleState::ActiveAck
                } else {
                    AlarmLifecycleState::Normal
                };
            }
        }
        self.record_audit(
            operator,
            "ALARME",
            "ACK_ALL",
            "Toutes",
            "Acquittement global des alarmes",
        );
        let all = self.get_all_alarms();
        let _ = self.alarm_tx.send(all.clone());
        all
    }

    pub fn clear_normal_alarms(&self, actor: &str) -> Vec<AlarmInstance> {
        let before = self.alarms.len();
        self.alarms
            .retain(|_, al| al.state != AlarmLifecycleState::Normal);
        let removed = before.saturating_sub(self.alarms.len());
        self.record_audit(
            actor,
            "ALARME",
            "CLEAR_NORMAL",
            "Résolues",
            &format!("Effacement de {} alarme(s) revenue(s) à la normale", removed),
        );
        let all = self.get_all_alarms();
        let _ = self.alarm_tx.send(all.clone());
        all
    }

    pub fn get_all_alarms(&self) -> Vec<AlarmInstance> {
        let mut list: Vec<AlarmInstance> =
            self.alarms.iter().map(|e| e.value().clone()).collect();
        list.sort_by_key(|a| std::cmp::Reverse(a.raised_at));
        list
    }

    fn record_alarm_journal(
        &self,
        tag_id: &str,
        severity: &str,
        event_type: &str,
        message: &str,
        operator: Option<String>,
    ) {
        if let Some(pool) = &self.db_pool {
            let pool_c = pool.clone();
            let t = tag_id.to_string();
            let s = severity.to_string();
            let e = event_type.to_string();
            let m = message.to_string();
            tokio::spawn(async move {
                log_db_error(
                    "journal d'alarmes",
                    sqlx::query(
                        "INSERT INTO alarms_journal (tag_id, severity, event_type, message, operator, recorded_at)
                         VALUES (?, ?, ?, ?, ?, ?)",
                    )
                    .bind(t)
                    .bind(s)
                    .bind(e)
                    .bind(m)
                    .bind(operator)
                    .bind(Utc::now().to_rfc3339())
                    .execute(&pool_c)
                    .await,
                );
            });
        }
    }

    pub async fn query_alarm_journal(&self, limit: u32) -> Result<Vec<AlarmJournalEntry>, ScadaError> {
        let Some(pool) = &self.db_pool else {
            return Ok(vec![]);
        };
        let rows = sqlx::query(
            "SELECT id, tag_id, severity, event_type, message, operator, recorded_at
             FROM alarms_journal ORDER BY id DESC LIMIT ?",
        )
        .bind(limit.clamp(10, 500) as i64)
        .fetch_all(pool)
        .await
        .map_err(|e| ScadaError::DatabaseError(e.to_string()))?;

        let mut list = Vec::with_capacity(rows.len());
        for r in rows {
            list.push(AlarmJournalEntry {
                id: r.try_get("id").unwrap_or(0),
                tag_id: r.try_get("tag_id").unwrap_or_default(),
                severity: r.try_get("severity").unwrap_or_default(),
                event_type: r.try_get("event_type").unwrap_or_default(),
                message: r.try_get("message").unwrap_or_default(),
                operator: r
                    .try_get::<Option<String>, _>("operator")
                    .ok()
                    .flatten()
                    .filter(|o| !o.is_empty()),
                recorded_at: r.try_get("recorded_at").unwrap_or_default(),
            });
        }
        Ok(list)
    }

    // ========================================================================
    // ÉCRITURE DE CONSIGNES & TÉLÉCOMMANDES
    // ========================================================================

    /// Envoie une consigne ou une télécommande.
    ///
    /// - **Variable interne** (aucun driver déclaré sous ce nom) : la valeur est appliquée directement.
    /// - **Variable d'équipement** : la commande est convertie en valeur brute (inverse de la mise à
    ///   l'échelle) puis transmise au driver sans attente. La valeur affichée est mise à jour par la
    ///   relecture de l'équipement, jamais de façon « optimiste ». La commande est refusée si le driver
    ///   est arrêté, déconnecté ou saturé.
    pub async fn write_tag(
        &self,
        tag_id: &str,
        raw_value: TagValue,
    ) -> Result<TagState, ScadaError> {
        let tag_meta = self
            .tags_registry
            .get(tag_id)
            .map(|r| r.clone())
            .ok_or_else(|| ScadaError::TagNotFound(tag_id.to_string()))?;

        if !tag_meta.writable {
            return Err(ScadaError::TagNotWritable(tag_id.to_string()));
        }

        let coerced_value = raw_value.coerce_to(tag_meta.data_type)?;
        if let Some(f) = coerced_value.as_f64() {
            if matches!(coerced_value, TagValue::Float(_) | TagValue::Int(_)) && !f.is_finite() {
                return Err(ScadaError::BadRequest("Valeur numérique invalide".to_string()));
            }
        }

        let requested_state = TagState {
            tag_id: tag_meta.id.clone(),
            value: coerced_value.clone(),
            quality: Quality::Good,
            timestamp: Utc::now(),
        };

        let driver_status = self
            .drivers_config
            .get(&tag_meta.module_source)
            .map(|d| (d.stats.running, d.stats.connected));

        let Some((running, connected)) = driver_status else {
            // Variable interne (mémoire) : pas d'équipement à piloter
            self.ingest_state(requested_state.clone(), false);
            return Ok(requested_state);
        };

        if !running {
            return Err(ScadaError::DriverError(format!(
                "Driver '{}' arrêté : commande refusée",
                tag_meta.module_source
            )));
        }
        if !connected {
            return Err(ScadaError::DriverError(format!(
                "Équipement '{}' déconnecté : commande refusée",
                tag_meta.module_source
            )));
        }

        let sender = self
            .driver_writers
            .get(&tag_meta.module_source)
            .map(|w| w.value().clone())
            .ok_or_else(|| {
                ScadaError::DriverError(format!(
                    "Driver '{}' sans canal d'écriture",
                    tag_meta.module_source
                ))
            })?;

        let cmd = WriteCommand {
            tag_id: tag_meta.id.clone(),
            protocol_address: tag_meta.protocol_address.clone(),
            value: to_device_value(&tag_meta, &coerced_value),
        };
        sender.try_send(cmd).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => ScadaError::DriverError(format!(
                "File d'écriture du driver '{}' saturée : réessayez",
                tag_meta.module_source
            )),
            mpsc::error::TrySendError::Closed(_) => ScadaError::DriverError(format!(
                "Canal d'écriture du driver '{}' fermé",
                tag_meta.module_source
            )),
        })?;

        Ok(requested_state)
    }

    // ========================================================================
    // PLANIFICATEUR HORAIRE GTB (SCHEDULER CVC)
    // ========================================================================

    pub fn get_all_schedules(&self) -> Vec<ScheduleRule> {
        let mut list: Vec<ScheduleRule> =
            self.schedules.iter().map(|s| s.value().clone()).collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    pub async fn save_schedule(
        &self,
        actor: &str,
        mut rule: ScheduleRule,
    ) -> Result<ScheduleRule, ScadaError> {
        security::validate_id("planning", &rule.id)?;
        if !is_valid_hhmm(&rule.start_time) || !is_valid_hhmm(&rule.end_time) {
            return Err(ScadaError::BadRequest(
                "Les heures de début et de fin doivent être au format HH:MM".to_string(),
            ));
        }
        rule.days.retain(|d| (1..=7).contains(d));
        rule.days.sort_unstable();
        rule.days.dedup();
        match self.tags_registry.get(&rule.target_tag_id) {
            Some(t) if t.writable => {}
            Some(_) => return Err(ScadaError::TagNotWritable(rule.target_tag_id.clone())),
            None => return Err(ScadaError::TagNotFound(rule.target_tag_id.clone())),
        }
        // Réévaluation immédiate au prochain cycle du planificateur
        rule.last_applied_state = None;

        self.schedules.insert(rule.id.clone(), rule.clone());
        if let Some(pool) = &self.db_pool {
            let js = serde_json::to_string(&rule)
                .map_err(|e| ScadaError::DatabaseError(e.to_string()))?;
            sqlx::query(
                "INSERT INTO schedules (id, rule_json, updated_at) VALUES (?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET rule_json = excluded.rule_json, updated_at = excluded.updated_at",
            )
            .bind(&rule.id)
            .bind(js)
            .bind(Utc::now().to_rfc3339())
            .execute(pool)
            .await
            .map_err(|e| ScadaError::DatabaseError(e.to_string()))?;
        }
        self.record_audit(actor, "PLANNING", "SAVE_SCHEDULE", &rule.id, &rule.name);
        Ok(rule)
    }

    pub async fn delete_schedule(&self, actor: &str, id: &str) -> Result<(), ScadaError> {
        self.schedules.remove(id);
        if let Some(pool) = &self.db_pool {
            log_db_error(
                "suppression planning",
                sqlx::query("DELETE FROM schedules WHERE id = ?")
                    .bind(id)
                    .execute(pool)
                    .await,
            );
        }
        self.record_audit(actor, "PLANNING", "DELETE_SCHEDULE", id, "Suppression programme horaire");
        Ok(())
    }

    fn spawn_scheduler_worker(core: Arc<Self>) {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(15));
            // Règles en échec : on ne journalise qu'au premier échec, puis on réessaie en silence.
            let mut failing: std::collections::HashSet<String> = Default::default();
            loop {
                ticker.tick().await;
                if core.is_acquisition_paused() {
                    continue;
                }
                let now = Local::now();
                let weekday = now.weekday().number_from_monday() as u8;
                let hhmm = now.format("%H:%M").to_string();
                let rules: Vec<ScheduleRule> = core
                    .schedules
                    .iter()
                    .filter(|r| r.value().enabled)
                    .map(|r| r.value().clone())
                    .collect();

                for rule in rules {
                    let is_in_window = rule.is_active_at(weekday, &hhmm);
                    if rule.last_applied_state == Some(is_in_window) {
                        continue;
                    }
                    let target_val = if is_in_window {
                        rule.active_value.clone()
                    } else {
                        rule.inactive_value.clone()
                    };
                    match core.write_tag(&rule.target_tag_id, target_val.clone()).await {
                        Ok(_) => {
                            failing.remove(&rule.id);
                            if let Some(mut r_mut) = core.schedules.get_mut(&rule.id) {
                                r_mut.last_applied_state = Some(is_in_window);
                            }
                            core.record_audit(
                                "Planificateur",
                                "PLANNING",
                                "APPLY_SCHEDULE",
                                &rule.target_tag_id,
                                &format!(
                                    "{} : mode {} ({})",
                                    rule.name,
                                    if is_in_window { "Confort" } else { "Réduit" },
                                    serde_json::to_string(&target_val).unwrap_or_default()
                                ),
                            );
                        }
                        Err(err) => {
                            if failing.insert(rule.id.clone()) {
                                warn!(
                                    "Planning '{}' non appliqué sur '{}' : {} (nouvelle tentative toutes les 15 s)",
                                    rule.id, rule.target_tag_id, err
                                );
                            }
                        }
                    }
                }
            }
        });
    }

    // ========================================================================
    // MULTI-SYNOPTIQUES, MULTI-FLUX, HISTORIAN & AUDIT LOGS
    // ========================================================================

    pub fn get_all_tags(&self) -> Vec<TagFullView> {
        let mut list: Vec<TagFullView> = self
            .tags_registry
            .iter()
            .map(|entry| {
                let meta = entry.value().clone();
                let state = self.tags_state.get(&meta.id).map(|s| s.value().clone());
                TagFullView {
                    metadata: meta,
                    state,
                }
            })
            .collect();
        list.sort_by(|a, b| a.metadata.id.cmp(&b.metadata.id));
        list
    }

    pub fn get_all_drivers(&self) -> Vec<DriverConfig> {
        let mut list: Vec<DriverConfig> = self
            .drivers_config
            .iter()
            .map(|d| d.value().clone())
            .collect();
        for cfg in list.iter_mut() {
            cfg.tags = self.get_tags_for_driver(&cfg.id);
        }
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    pub fn get_all_dashboards(&self) -> Vec<DashboardLayout> {
        let mut list: Vec<DashboardLayout> = self
            .dashboards
            .iter()
            .map(|entry| entry.value().clone())
            .collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    pub async fn save_dashboard(
        &self,
        actor: &str,
        mut layout: DashboardLayout,
    ) -> Result<DashboardLayout, ScadaError> {
        sanitize_dashboard(&mut layout)?;
        layout.updated_at = Utc::now();
        self.dashboards.insert(layout.id.clone(), layout.clone());

        if let Some(pool) = &self.db_pool {
            let json_payload = serde_json::to_string(&layout)
                .map_err(|e| ScadaError::DatabaseError(e.to_string()))?;
            sqlx::query(
                "INSERT INTO dashboards (id, name, layout_json, updated_at) VALUES (?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET name = excluded.name, layout_json = excluded.layout_json, updated_at = excluded.updated_at",
            )
            .bind(&layout.id)
            .bind(&layout.name)
            .bind(json_payload)
            .bind(layout.updated_at.to_rfc3339())
            .execute(pool)
            .await
            .map_err(|e| ScadaError::DatabaseError(e.to_string()))?;
        }
        self.record_audit(actor, "STUDIO", "SAVE_DASHBOARD", &layout.id, &layout.name);
        Ok(layout)
    }

    pub async fn delete_dashboard(&self, actor: &str, id: &str) -> Result<(), ScadaError> {
        self.dashboards.remove(id);
        if let Some(pool) = &self.db_pool {
            log_db_error(
                "suppression synoptique",
                sqlx::query("DELETE FROM dashboards WHERE id = ?")
                    .bind(id)
                    .execute(pool)
                    .await,
            );
        }
        self.record_audit(actor, "STUDIO", "DELETE_DASHBOARD", id, "Suppression synoptique");
        Ok(())
    }

    pub fn get_all_flows(&self) -> Vec<FlowGraph> {
        let mut list: Vec<FlowGraph> = self
            .flows
            .iter()
            .map(|entry| entry.value().clone())
            .collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    pub async fn save_flow(&self, actor: &str, flow: FlowGraph) -> Result<FlowGraph, ScadaError> {
        validate_flow(&flow)?;
        self.flows.insert(flow.id.clone(), flow.clone());

        if let Some(pool) = &self.db_pool {
            let json_payload = serde_json::to_string(&flow)
                .map_err(|e| ScadaError::DatabaseError(e.to_string()))?;
            sqlx::query(
                "INSERT INTO flows (id, name, flow_json, updated_at) VALUES (?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET name = excluded.name, flow_json = excluded.flow_json, updated_at = excluded.updated_at",
            )
            .bind(&flow.id)
            .bind(&flow.name)
            .bind(json_payload)
            .bind(Utc::now().to_rfc3339())
            .execute(pool)
            .await
            .map_err(|e| ScadaError::DatabaseError(e.to_string()))?;
        }
        self.record_audit(actor, "FLOW_ENGINE", "DEPLOY_FLOW", &flow.id, &flow.name);
        Ok(flow)
    }

    pub async fn delete_flow(&self, actor: &str, id: &str) -> Result<(), ScadaError> {
        self.flows
            .remove(id)
            .ok_or_else(|| ScadaError::FlowNotFound(id.to_string()))?;
        if let Some(pool) = &self.db_pool {
            log_db_error(
                "suppression flux",
                sqlx::query("DELETE FROM flows WHERE id = ?")
                    .bind(id)
                    .execute(pool)
                    .await,
            );
        }
        self.record_audit(actor, "FLOW_ENGINE", "DELETE_FLOW", id, "Suppression flux");
        Ok(())
    }

    /// Lit l'historique d'un tag.
    ///
    /// - Sans période : les `limit` points les plus récents.
    /// - Avec une période `[from, to]` : un échantillon **régulier** d'au plus `limit` points
    ///   couvrant toute la période (et non les seuls derniers points), avec des statistiques
    ///   (min, max, moyenne, écart-type) calculées sur la totalité des points de la période.
    pub async fn query_tag_history(
        &self,
        tag_id: &str,
        limit: u32,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
    ) -> Result<HistoryResponse, ScadaError> {
        let unit = self
            .tags_registry
            .get(tag_id)
            .and_then(|t| t.unit.clone());
        let limit = limit.clamp(10, 5000) as i64;

        let Some(pool) = &self.db_pool else {
            return Ok(HistoryResponse {
                tag_id: tag_id.to_string(),
                unit,
                count: 0,
                total_points: 0,
                sampled: false,
                min: None,
                max: None,
                avg: None,
                std_dev: None,
                points: vec![],
            });
        };

        // Les horodatages sont stockés en RFC 3339 UTC (`+00:00`) : l'ordre lexicographique
        // correspond à l'ordre chronologique.
        let has_range = from.is_some() || to.is_some();
        let from_s = from.map(|d| d.to_rfc3339()).unwrap_or_default();
        let to_s = to
            .map(|d| d.to_rfc3339())
            .unwrap_or_else(|| "9999-12-31T23:59:59+00:00".to_string());

        let db_err = |e: sqlx::Error| ScadaError::DatabaseError(e.to_string());

        let rows = if has_range {
            // Échantillonnage régulier : un point sur `pas`, le pas étant calculé pour ne pas
            // dépasser `limit` points sur toute la période.
            sqlx::query(
                "SELECT value_json, quality, recorded_at, total FROM (
                     SELECT value_json, quality, recorded_at, id,
                            ROW_NUMBER() OVER (ORDER BY id) AS rn,
                            COUNT(*) OVER () AS total
                     FROM tag_history
                     WHERE tag_id = ? AND recorded_at >= ? AND recorded_at <= ?
                 )
                 WHERE (rn - 1) % MAX(1, (total + ? - 1) / ?) = 0
                 ORDER BY id ASC
                 LIMIT ?",
            )
            .bind(tag_id)
            .bind(&from_s)
            .bind(&to_s)
            .bind(limit)
            .bind(limit)
            .bind(limit)
            .fetch_all(pool)
            .await
            .map_err(db_err)?
        } else {
            let mut recent = sqlx::query(
                "SELECT value_json, quality, recorded_at FROM tag_history
                 WHERE tag_id = ? ORDER BY id DESC LIMIT ?",
            )
            .bind(tag_id)
            .bind(limit)
            .fetch_all(pool)
            .await
            .map_err(db_err)?;
            recent.reverse();
            recent
        };

        let mut total_points = rows.len();
        let mut points = Vec::with_capacity(rows.len());
        let mut nums = Vec::new();
        for row in &rows {
            if has_range {
                total_points = row.try_get::<i64, _>("total").unwrap_or(0).max(0) as usize;
            }
            let val_str: String = row.try_get("value_json").unwrap_or_default();
            let quality: String = row.try_get("quality").unwrap_or_else(|_| "Good".to_string());
            let timestamp: String = row.try_get("recorded_at").unwrap_or_default();

            if let Ok(value) = serde_json::from_str::<TagValue>(&val_str) {
                if let Some(f) = value.as_f64() {
                    nums.push(f);
                }
                points.push(HistoryPoint {
                    timestamp,
                    value,
                    quality,
                });
            }
        }

        let (min, max, avg, std_dev) = if has_range {
            // Statistiques exactes sur toute la période (valeurs numériques et booléennes)
            let stats = sqlx::query(
                "SELECT COUNT(v) AS n, MIN(v) AS mn, MAX(v) AS mx, AVG(v) AS av, AVG(v * v) AS av2 FROM (
                     SELECT CASE value_json WHEN 'true' THEN 1.0 WHEN 'false' THEN 0.0
                            ELSE CAST(value_json AS REAL) END AS v
                     FROM tag_history
                     WHERE tag_id = ? AND recorded_at >= ? AND recorded_at <= ?
                       AND substr(value_json, 1, 1) <> '\"'
                 )",
            )
            .bind(tag_id)
            .bind(&from_s)
            .bind(&to_s)
            .fetch_one(pool)
            .await
            .map_err(db_err)?;
            let n: i64 = stats.try_get("n").unwrap_or(0);
            if n == 0 {
                (None, None, None, None)
            } else {
                let mean: f64 = stats.try_get("av").unwrap_or(0.0);
                let mean_sq: f64 = stats.try_get("av2").unwrap_or(0.0);
                let sd = (mean_sq - mean * mean).max(0.0).sqrt();
                (
                    stats.try_get::<f64, _>("mn").ok(),
                    stats.try_get::<f64, _>("mx").ok(),
                    Some(round2(mean)),
                    Some(round2(sd)),
                )
            }
        } else {
            series_stats(&nums)
        };

        Ok(HistoryResponse {
            tag_id: tag_id.to_string(),
            unit,
            count: points.len(),
            total_points,
            sampled: has_range && total_points > points.len(),
            min,
            max,
            avg,
            std_dev,
            points,
        })
    }

    pub async fn purge_history(&self, actor: &str) -> Result<u64, ScadaError> {
        let Some(pool) = &self.db_pool else {
            return Ok(0);
        };
        let res = sqlx::query("DELETE FROM tag_history")
            .execute(pool)
            .await
            .map_err(|e| ScadaError::DatabaseError(e.to_string()))?;
        self.record_audit(
            actor,
            "HISTORIAN",
            "PURGE",
            "tag_history",
            &format!("Purge de {} mesures SQLite", res.rows_affected()),
        );
        Ok(res.rows_affected())
    }

    pub fn record_audit(
        &self,
        actor: &str,
        category: &str,
        action: &str,
        target: &str,
        details: &str,
    ) {
        if let Some(pool) = &self.db_pool {
            let pool_c = pool.clone();
            let a = actor.to_string();
            let c = category.to_string();
            let act = action.to_string();
            let tgt = target.to_string();
            let det = details.to_string();
            tokio::spawn(async move {
                log_db_error(
                    "journal d'audit",
                    sqlx::query(
                        "INSERT INTO audit_logs (timestamp, actor, category, action, target, details)
                         VALUES (?, ?, ?, ?, ?, ?)",
                    )
                    .bind(Utc::now().to_rfc3339())
                    .bind(a)
                    .bind(c)
                    .bind(act)
                    .bind(tgt)
                    .bind(det)
                    .execute(&pool_c)
                    .await,
                );
            });
        }
    }

    pub async fn query_audit_logs(&self, limit: u32) -> Result<Vec<AuditLogEntry>, ScadaError> {
        let Some(pool) = &self.db_pool else {
            return Ok(vec![]);
        };
        let rows = sqlx::query(
            "SELECT id, timestamp, actor, category, action, target, details
             FROM audit_logs ORDER BY id DESC LIMIT ?",
        )
        .bind(limit.clamp(10, 500) as i64)
        .fetch_all(pool)
        .await
        .map_err(|e| ScadaError::DatabaseError(e.to_string()))?;

        let mut list = Vec::with_capacity(rows.len());
        for r in rows {
            list.push(AuditLogEntry {
                id: r.try_get("id").unwrap_or(0),
                timestamp: r.try_get("timestamp").unwrap_or_default(),
                actor: r.try_get("actor").unwrap_or_default(),
                category: r.try_get("category").unwrap_or_default(),
                action: r.try_get("action").unwrap_or_default(),
                target: r.try_get("target").unwrap_or_default(),
                details: r.try_get("details").unwrap_or_default(),
            });
        }
        Ok(list)
    }

    pub fn export_project_bundle(&self) -> ProjectBackupBundle {
        ProjectBackupBundle {
            exported_at: Utc::now(),
            version: APP_VERSION.to_string(),
            drivers: self.get_all_drivers(),
            tags: self
                .tags_registry
                .iter()
                .map(|t| t.value().clone())
                .collect(),
            dashboards: self.get_all_dashboards(),
            flows: self.get_all_flows(),
            schedules: self.get_all_schedules(),
        }
    }

    /// Restaure une archive projet complète (drivers, tags, synoptiques, flux, plannings)
    /// et renvoie un rapport détaillé, erreurs comprises.
    pub async fn import_project_bundle(
        self: &Arc<Self>,
        actor: &str,
        bundle: ProjectBackupBundle,
    ) -> Result<serde_json::Value, ScadaError> {
        let mut errors: Vec<String> = Vec::new();
        let mut counts = [0usize; 5];

        for mut drv in bundle.drivers {
            drv.stats = DriverRuntimeStats::default();
            let id = drv.id.clone();
            match self.register_and_start_driver(actor, drv).await {
                Ok(_) => counts[0] += 1,
                // Un driver déclaré mais non démarrable (équipement absent) reste importé
                Err(ScadaError::DriverError(e)) => {
                    counts[0] += 1;
                    errors.push(format!("Driver '{}' importé mais non démarré : {}", id, e));
                }
                Err(e) => errors.push(format!("Driver '{}' : {}", id, e)),
            }
        }
        for tag in bundle.tags {
            let id = tag.id.clone();
            match self.upsert_tag(actor, tag).await {
                Ok(_) => counts[1] += 1,
                Err(e) => errors.push(format!("Tag '{}' : {}", id, e)),
            }
        }
        for dash in bundle.dashboards {
            let id = dash.id.clone();
            match self.save_dashboard(actor, dash).await {
                Ok(_) => counts[2] += 1,
                Err(e) => errors.push(format!("Synoptique '{}' : {}", id, e)),
            }
        }
        for flow in bundle.flows {
            let id = flow.id.clone();
            match self.save_flow(actor, flow).await {
                Ok(_) => counts[3] += 1,
                Err(e) => errors.push(format!("Flux '{}' : {}", id, e)),
            }
        }
        for sched in bundle.schedules {
            let id = sched.id.clone();
            match self.save_schedule(actor, sched).await {
                Ok(_) => counts[4] += 1,
                Err(e) => errors.push(format!("Planning '{}' : {}", id, e)),
            }
        }
        self.record_audit(
            actor,
            "SYSTÈME",
            "IMPORT_PROJECT",
            "Backup JSON",
            &format!(
                "Restauration : {} drivers, {} tags, {} synoptiques, {} flux, {} plannings, {} erreur(s)",
                counts[0], counts[1], counts[2], counts[3], counts[4], errors.len()
            ),
        );
        Ok(serde_json::json!({
            "imported": true,
            "drivers": counts[0],
            "tags": counts[1],
            "dashboards": counts[2],
            "flows": counts[3],
            "schedules": counts[4],
            "errors": errors,
        }))
    }

    // ========================================================================
    // CHARGEMENT DE L'ÉTAT PERSISTÉ (LA BASE FAIT FOI)
    // ========================================================================

    /// Charge la configuration depuis SQLite.
    ///
    /// Au tout premier lancement (indicateur `meta.seeded` absent), les données de démonstration
    /// et les drivers par défaut sont écrits en base avec `INSERT OR IGNORE` : rien d'existant
    /// n'est écrasé. Ensuite, seule la base fait foi : un élément supprimé ne réapparaît plus.
    pub async fn load_persisted_state(
        self: &Arc<Self>,
        default_drivers: Vec<DriverConfig>,
    ) -> Result<(), ScadaError> {
        let Some(pool) = self.db_pool.clone() else {
            // Mode dégradé sans base : démonstration en mémoire uniquement
            for cfg in default_drivers {
                for tag in &cfg.tags {
                    self.register_tag(tag.clone(), None);
                }
                self.drivers_config.insert(cfg.id.clone(), cfg);
            }
            self.migrate_legacy_passwords().await;
            return Ok(());
        };

        let seeded: Option<String> = sqlx::query_scalar("SELECT value FROM meta WHERE key = 'seeded'")
            .fetch_optional(&pool)
            .await
            .map_err(|e| ScadaError::DatabaseError(e.to_string()))?;

        if seeded.is_none() {
            self.write_seed_to_db(&pool, &default_drivers).await?;
            sqlx::query("INSERT OR REPLACE INTO meta (key, value) VALUES ('seeded', ?)")
                .bind(APP_VERSION)
                .execute(&pool)
                .await
                .map_err(|e| ScadaError::DatabaseError(e.to_string()))?;
            info!("Première initialisation : configuration de démonstration enregistrée en base");
        }

        // La base fait foi : la démonstration en mémoire est remplacée par le contenu persisté
        self.dashboards.clear();
        self.flows.clear();
        self.schedules.clear();
        self.users.clear();
        self.symbols_library.clear();

        for js in load_json_column(&pool, "SELECT config_json AS js FROM drivers_config").await {
            match serde_json::from_str::<DriverConfig>(&js) {
                Ok(mut cfg) => {
                    cfg.stats = DriverRuntimeStats::default();
                    for tag in cfg.tags.iter_mut() {
                        tag.module_source = cfg.id.clone();
                        self.register_tag(tag.clone(), None);
                    }
                    self.drivers_config.insert(cfg.id.clone(), cfg);
                }
                Err(e) => warn!("Configuration de driver illisible ignorée : {}", e),
            }
        }

        // Les tags du catalogue priment sur ceux embarqués dans la configuration des drivers
        for js in load_json_column(&pool, "SELECT tag_json AS js FROM tags_catalog").await {
            match serde_json::from_str::<RegisteredTag>(&js) {
                Ok(tag) => {
                    self.register_tag(tag, None);
                }
                Err(e) => warn!("Tag illisible ignoré : {}", e),
            }
        }

        for js in load_json_column(&pool, "SELECT layout_json AS js FROM dashboards").await {
            match serde_json::from_str::<DashboardLayout>(&js) {
                Ok(mut d) => {
                    if let Err(e) = sanitize_dashboard(&mut d) {
                        warn!("Synoptique '{}' invalide ignoré : {}", d.id, e);
                        continue;
                    }
                    self.dashboards.insert(d.id.clone(), d);
                }
                Err(e) => warn!("Synoptique illisible ignoré : {}", e),
            }
        }

        for js in load_json_column(&pool, "SELECT flow_json AS js FROM flows").await {
            match serde_json::from_str::<FlowGraph>(&js) {
                Ok(f) => {
                    self.flows.insert(f.id.clone(), f);
                }
                Err(e) => warn!("Flux illisible ignoré : {}", e),
            }
        }

        for js in load_json_column(&pool, "SELECT rule_json AS js FROM schedules").await {
            match serde_json::from_str::<ScheduleRule>(&js) {
                Ok(s) => {
                    self.schedules.insert(s.id.clone(), s);
                }
                Err(e) => warn!("Planning illisible ignoré : {}", e),
            }
        }

        for js in load_json_column(&pool, "SELECT user_json AS js FROM users_accounts").await {
            match serde_json::from_str::<UserAccount>(&js) {
                Ok(u) => {
                    self.users.insert(u.username.clone(), u);
                }
                Err(e) => warn!("Compte utilisateur illisible ignoré : {}", e),
            }
        }

        for js in load_json_column(&pool, "SELECT symbol_json AS js FROM symbol_libraries").await {
            match serde_json::from_str::<SymbolLibraryItem>(&js) {
                Ok(mut sym) => {
                    sym.svg_content = security::sanitize_svg(&sym.svg_content);
                    self.symbols_library.insert(sym.id.clone(), sym);
                }
                Err(e) => warn!("Symbole illisible ignoré : {}", e),
            }
        }

        // Garde-fou : sans aucun administrateur actif, on recrée le compte admin d'usine
        let has_admin = self
            .users
            .iter()
            .any(|u| u.value().role == UserRole::Admin && u.value().enabled);
        if !has_admin {
            warn!("Aucun administrateur actif en base : recréation du compte 'admin' d'usine");
            self.users.insert("admin".to_string(), factory_admin_account());
        }

        self.migrate_legacy_passwords().await;
        self.warn_factory_passwords().await;
        Ok(())
    }

    async fn write_seed_to_db(
        &self,
        pool: &SqlitePool,
        default_drivers: &[DriverConfig],
    ) -> Result<(), ScadaError> {
        let now = Utc::now().to_rfc3339();
        let mut tx = pool
            .begin()
            .await
            .map_err(|e| ScadaError::DatabaseError(e.to_string()))?;

        for cfg in default_drivers {
            let js = serde_json::to_string(cfg).map_err(|e| ScadaError::DatabaseError(e.to_string()))?;
            sqlx::query("INSERT OR IGNORE INTO drivers_config (id, config_json, updated_at) VALUES (?, ?, ?)")
                .bind(&cfg.id)
                .bind(js)
                .bind(&now)
                .execute(&mut *tx)
                .await
                .map_err(|e| ScadaError::DatabaseError(e.to_string()))?;
        }
        for d in self.get_all_dashboards() {
            let js = serde_json::to_string(&d).map_err(|e| ScadaError::DatabaseError(e.to_string()))?;
            sqlx::query("INSERT OR IGNORE INTO dashboards (id, name, layout_json, updated_at) VALUES (?, ?, ?, ?)")
                .bind(&d.id)
                .bind(&d.name)
                .bind(js)
                .bind(&now)
                .execute(&mut *tx)
                .await
                .map_err(|e| ScadaError::DatabaseError(e.to_string()))?;
        }
        for f in self.get_all_flows() {
            let js = serde_json::to_string(&f).map_err(|e| ScadaError::DatabaseError(e.to_string()))?;
            sqlx::query("INSERT OR IGNORE INTO flows (id, name, flow_json, updated_at) VALUES (?, ?, ?, ?)")
                .bind(&f.id)
                .bind(&f.name)
                .bind(js)
                .bind(&now)
                .execute(&mut *tx)
                .await
                .map_err(|e| ScadaError::DatabaseError(e.to_string()))?;
        }
        for s in self.get_all_schedules() {
            let js = serde_json::to_string(&s).map_err(|e| ScadaError::DatabaseError(e.to_string()))?;
            sqlx::query("INSERT OR IGNORE INTO schedules (id, rule_json, updated_at) VALUES (?, ?, ?)")
                .bind(&s.id)
                .bind(js)
                .bind(&now)
                .execute(&mut *tx)
                .await
                .map_err(|e| ScadaError::DatabaseError(e.to_string()))?;
        }
        let users: Vec<UserAccount> = self.users.iter().map(|u| u.value().clone()).collect();
        for u in users {
            let js = serde_json::to_string(&u).map_err(|e| ScadaError::DatabaseError(e.to_string()))?;
            sqlx::query("INSERT OR IGNORE INTO users_accounts (username, user_json, updated_at) VALUES (?, ?, ?)")
                .bind(&u.username)
                .bind(js)
                .bind(&now)
                .execute(&mut *tx)
                .await
                .map_err(|e| ScadaError::DatabaseError(e.to_string()))?;
        }
        for sym in self.get_all_symbols() {
            let js = serde_json::to_string(&sym).map_err(|e| ScadaError::DatabaseError(e.to_string()))?;
            sqlx::query("INSERT OR IGNORE INTO symbol_libraries (id, symbol_json, updated_at) VALUES (?, ?, ?)")
                .bind(&sym.id)
                .bind(js)
                .bind(&now)
                .execute(&mut *tx)
                .await
                .map_err(|e| ScadaError::DatabaseError(e.to_string()))?;
        }

        tx.commit()
            .await
            .map_err(|e| ScadaError::DatabaseError(e.to_string()))
    }

    /// Convertit en Argon2id les mots de passe encore stockés en clair (versions ≤ 1.0).
    async fn migrate_legacy_passwords(&self) {
        let legacy: Vec<UserAccount> = self
            .users
            .iter()
            .filter(|u| !security::is_password_hash(&u.value().password_hash))
            .map(|u| u.value().clone())
            .collect();

        for mut account in legacy {
            let plain = account.password_hash.clone();
            match tokio::task::spawn_blocking(move || security::hash_password(&plain)).await {
                Ok(Ok(hash)) => {
                    account.password_hash = hash;
                    self.users.insert(account.username.clone(), account.clone());
                    self.persist_user(&account).await;
                    info!("Mot de passe du compte '{}' migré vers Argon2id", account.username);
                }
                Ok(Err(e)) => error!("Migration du mot de passe de '{}' impossible : {}", account.username, e),
                Err(e) => error!("Migration du mot de passe de '{}' interrompue : {}", account.username, e),
            }
        }
    }

    /// Avertit dans les journaux tant qu'un compte d'usine garde son mot de passe initial.
    async fn warn_factory_passwords(&self) {
        for (username, factory_pwd) in FACTORY_ACCOUNTS {
            let Some(hash) = self.users.get(username).map(|u| u.password_hash.clone()) else {
                continue;
            };
            let still_default =
                tokio::task::spawn_blocking(move || security::verify_password(factory_pwd, &hash))
                    .await
                    .unwrap_or(false);
            if still_default {
                warn!(
                    "SÉCURITÉ : le compte '{}' utilise encore son mot de passe d'usine — changez-le depuis l'IHM",
                    username
                );
            }
        }
    }

    async fn persist_user(&self, account: &UserAccount) {
        let Some(pool) = &self.db_pool else {
            return;
        };
        match serde_json::to_string(account) {
            Ok(json) => log_db_error(
                "persistance utilisateur",
                sqlx::query(
                    "INSERT INTO users_accounts (username, user_json, updated_at)
                     VALUES (?, ?, ?)
                     ON CONFLICT(username) DO UPDATE SET user_json = excluded.user_json, updated_at = excluded.updated_at",
                )
                .bind(&account.username)
                .bind(json)
                .bind(Utc::now().to_rfc3339())
                .execute(pool)
                .await,
            ),
            Err(e) => warn!("Sérialisation du compte '{}' impossible : {}", account.username, e),
        }
    }

    // ========================================================================
    // TÂCHES DE FOND (INGESTION, HISTORIQUE, RÉTENTION, SESSIONS, TÉLÉMÉTRIE)
    // ========================================================================

    fn spawn_ingestion_worker(core: Arc<Self>, mut ingest_rx: mpsc::Receiver<TagState>) {
        tokio::spawn(async move {
            while let Some(state) = ingest_rx.recv().await {
                core.process_incoming_state(state);
            }
        });
    }

    /// Écrivain unique de l'historique : regroupe les points (500 max ou 1 s) en une seule
    /// requête `INSERT` multi-lignes, au lieu d'une écriture disque par changement de valeur.
    fn spawn_history_writer(core: Arc<Self>, mut rx: mpsc::Receiver<TagState>) {
        let Some(pool) = core.db_pool.clone() else {
            return;
        };
        tokio::spawn(async move {
            let mut batch: Vec<TagState> = Vec::with_capacity(HISTORY_BATCH_MAX);
            loop {
                let Some(first) = rx.recv().await else {
                    break;
                };
                batch.push(first);
                let deadline = tokio::time::Instant::now() + Duration::from_millis(1000);
                while batch.len() < HISTORY_BATCH_MAX {
                    match tokio::time::timeout_at(deadline, rx.recv()).await {
                        Ok(Some(st)) => batch.push(st),
                        Ok(None) | Err(_) => break,
                    }
                }
                write_history_batch(&pool, &batch).await;
                batch.clear();
            }
        });
    }

    /// Purge périodique (toutes les 6 h) des points d'historique plus anciens que la rétention.
    fn spawn_history_retention_worker(core: Arc<Self>) {
        let days = core.history_retention_days;
        let Some(pool) = core.db_pool.clone() else {
            return;
        };
        if days == 0 {
            info!("Rétention de l'historique désactivée (SCADA_HISTORY_RETENTION_DAYS=0)");
            return;
        }
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let mut ticker = tokio::time::interval(Duration::from_secs(6 * 3600));
            loop {
                ticker.tick().await;
                let cutoff = (Utc::now() - chrono::Duration::days(days as i64)).to_rfc3339();
                let mut total: u64 = 0;
                // Suppression par tranches pour ne pas bloquer la base longtemps
                loop {
                    match sqlx::query(
                        "DELETE FROM tag_history WHERE id IN
                         (SELECT id FROM tag_history WHERE recorded_at < ? LIMIT 5000)",
                    )
                    .bind(&cutoff)
                    .execute(&pool)
                    .await
                    {
                        Ok(res) if res.rows_affected() > 0 => {
                            total += res.rows_affected();
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                        Ok(_) => break,
                        Err(e) => {
                            warn!("Erreur SQLite (rétention historique) : {}", e);
                            break;
                        }
                    }
                }
                if total > 0 {
                    info!("Rétention historique : {} points de plus de {} jours supprimés", total, days);
                    core.record_audit(
                        "Système",
                        "HISTORIAN",
                        "RETENTION",
                        "tag_history",
                        &format!("Purge automatique de {} points (> {} jours)", total, days),
                    );
                }
            }
        });
    }

    fn spawn_session_cleanup_worker(core: Arc<Self>) {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(600));
            loop {
                ticker.tick().await;
                let ttl = chrono::Duration::hours(core.session_ttl_hours as i64);
                let now = Utc::now();
                core.sessions.retain(|_, s| now - s.last_seen < ttl);
                core.login_failures
                    .retain(|_, f| (now - f.last_at).num_seconds() < 3600);
            }
        });
    }

    /// Diffuse la télémétrie des drivers (trames, latence, erreurs) toutes les 2 s,
    /// uniquement si au moins un client WebSocket est connecté.
    fn spawn_driver_stats_broadcaster(core: Arc<Self>) {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(2));
            loop {
                ticker.tick().await;
                if core.driver_tx.receiver_count() > 0 {
                    let _ = core.driver_tx.send(core.get_all_drivers());
                }
            }
        });
    }

    fn seed_industrial_defaults(&self) {
        // SYNOPTIQUE 1 : CVC CTA 01 & 02
        let syn_cvc = DashboardLayout {
            id: "main-gtb-synoptic".to_string(),
            name: "01. CVC — Centrales de Traitement d'Air (CTA 01 & 02)".to_string(),
            category: "CVC / Climatisation".to_string(),
            description: "Supervision complète de la CTA 01".to_string(),
            grid_size: 20,
            updated_at: Utc::now(),
            widgets: vec![
                DashboardWidget {
                    id: "w-cta-fan".to_string(),
                    x: 20.0,
                    y: 20.0,
                    width: 220.0,
                    height: 195.0,
                    widget_type: WidgetType::Fan,
                    bound_tag_id: Some("GTB.CTA01.Ventilateur_Marche".to_string()),
                    title: "Ventilateur Soufflage CTA 01".to_string(),
                    properties: serde_json::json!({}),
                },
                DashboardWidget {
                    id: "w-cta-damper".to_string(),
                    x: 260.0,
                    y: 20.0,
                    width: 230.0,
                    height: 195.0,
                    widget_type: WidgetType::Damper,
                    bound_tag_id: Some("GTB.CTA01.Registre_AirNeuf".to_string()),
                    title: "Registre Air Neuf Motorisé".to_string(),
                    properties: serde_json::json!({ "min": 0, "max": 100 }),
                },
                DashboardWidget {
                    id: "w-cta-valve".to_string(),
                    x: 510.0,
                    y: 20.0,
                    width: 230.0,
                    height: 195.0,
                    widget_type: WidgetType::Valve,
                    bound_tag_id: Some("GTB.CTA01.Vanne_Chaud_Cmd".to_string()),
                    title: "Vanne 3 Voies Batterie Chaude".to_string(),
                    properties: serde_json::json!({ "min": 0, "max": 100 }),
                },
                DashboardWidget {
                    id: "w-cta-setpoint".to_string(),
                    x: 760.0,
                    y: 20.0,
                    width: 240.0,
                    height: 195.0,
                    widget_type: WidgetType::SetpointButton,
                    bound_tag_id: Some("GTB.CTA01.Consigne_Temp".to_string()),
                    title: "Consigne Température CTA 01".to_string(),
                    properties: serde_json::json!({ "step": 0.5, "min": 14, "max": 30 }),
                },
                DashboardWidget {
                    id: "w-cta-gauge-soufflage".to_string(),
                    x: 20.0,
                    y: 235.0,
                    width: 220.0,
                    height: 210.0,
                    widget_type: WidgetType::Gauge,
                    bound_tag_id: Some("GTB.CTA01.Temp_Soufflage".to_string()),
                    title: "Température Soufflage".to_string(),
                    properties: serde_json::json!({ "min": 10, "max": 35 }),
                },
                DashboardWidget {
                    id: "w-cta-trend".to_string(),
                    x: 260.0,
                    y: 235.0,
                    width: 480.0,
                    height: 210.0,
                    widget_type: WidgetType::TrendChart,
                    bound_tag_id: Some("GTB.CTA01.Temp_Soufflage".to_string()),
                    title: "Tendance Temps Réel — Température Soufflage (°C)".to_string(),
                    properties: serde_json::json!({}),
                },
                DashboardWidget {
                    id: "w-cta-hygro".to_string(),
                    x: 760.0,
                    y: 235.0,
                    width: 240.0,
                    height: 100.0,
                    widget_type: WidgetType::NumericDisplay,
                    bound_tag_id: Some("GTB.CTA01.Hygrometrie_Reprise".to_string()),
                    title: "Hygrométrie Reprise Air".to_string(),
                    properties: serde_json::json!({}),
                },
                DashboardWidget {
                    id: "w-cta-filtre".to_string(),
                    x: 760.0,
                    y: 345.0,
                    width: 240.0,
                    height: 100.0,
                    widget_type: WidgetType::IndicatorLed,
                    bound_tag_id: Some("GTB.CTA01.Defaut_Filtre".to_string()),
                    title: "Pressostat Filtre Opacimétrique F7".to_string(),
                    properties: serde_json::json!({}),
                },
            ],
        };

        // SYNOPTIQUE 2 : Chaufferie Hydraulique & Groupe Froid Modbus TCP
        let syn_chauf = DashboardLayout {
            id: "hydraulique-chaufferie".to_string(),
            name: "02. Hydraulique — Chaufferie & Groupe Froid (Modbus TCP)".to_string(),
            category: "Production Thermique".to_string(),
            description: "Supervision de la chaudière, des pompes P1/P2 et de l'automate Modbus TCP Groupe Froid".to_string(),
            grid_size: 20,
            updated_at: Utc::now(),
            widgets: vec![
                DashboardWidget {
                    id: "w-ch-boiler".to_string(),
                    x: 20.0,
                    y: 20.0,
                    width: 240.0,
                    height: 200.0,
                    widget_type: WidgetType::Boiler,
                    bound_tag_id: Some("IND.CHAUF.Temp_Depart_Chaudiere".to_string()),
                    title: "Chaudière Gaz 850 kW".to_string(),
                    properties: serde_json::json!({}),
                },
                DashboardWidget {
                    id: "w-ch-p1".to_string(),
                    x: 280.0,
                    y: 20.0,
                    width: 230.0,
                    height: 200.0,
                    widget_type: WidgetType::Pump,
                    bound_tag_id: Some("IND.CHAUF.Pompe_P1_Etat".to_string()),
                    title: "Pompe Primaire P1 (Prioritaire)".to_string(),
                    properties: serde_json::json!({}),
                },
                DashboardWidget {
                    id: "w-ch-p2".to_string(),
                    x: 530.0,
                    y: 20.0,
                    width: 230.0,
                    height: 200.0,
                    widget_type: WidgetType::Pump,
                    bound_tag_id: Some("IND.CHAUF.Pompe_P2_Secours".to_string()),
                    title: "Pompe Secours P2 (Auto)".to_string(),
                    properties: serde_json::json!({}),
                },
                DashboardWidget {
                    id: "w-ch-bache".to_string(),
                    x: 780.0,
                    y: 20.0,
                    width: 220.0,
                    height: 200.0,
                    widget_type: WidgetType::TankLevel,
                    bound_tag_id: Some("IND.CHAUF.Niveau_Bache".to_string()),
                    title: "Bâche Tampon Hydraulique".to_string(),
                    properties: serde_json::json!({}),
                },
                DashboardWidget {
                    id: "w-mb-gf-temp".to_string(),
                    x: 20.0,
                    y: 240.0,
                    width: 240.0,
                    height: 195.0,
                    widget_type: WidgetType::Chiller,
                    bound_tag_id: Some("MODBUS.GF01.Temp_Eau_Glacee".to_string()),
                    title: "Groupe Froid Eau Glacée (Modbus)".to_string(),
                    properties: serde_json::json!({}),
                },
                DashboardWidget {
                    id: "w-mb-gf-sp".to_string(),
                    x: 280.0,
                    y: 240.0,
                    width: 230.0,
                    height: 195.0,
                    widget_type: WidgetType::SetpointButton,
                    bound_tag_id: Some("MODBUS.GF01.Consigne_Eau_Glacee".to_string()),
                    title: "Consigne Eau Glacée Modbus".to_string(),
                    properties: serde_json::json!({ "step": 0.5 }),
                },
                DashboardWidget {
                    id: "w-ch-trend-p".to_string(),
                    x: 530.0,
                    y: 240.0,
                    width: 470.0,
                    height: 195.0,
                    widget_type: WidgetType::TrendChart,
                    bound_tag_id: Some("IND.CHAUF.Pression_Reseau".to_string()),
                    title: "Courbe de Pression Réseau (bar)".to_string(),
                    properties: serde_json::json!({}),
                },
            ],
        };

        // SYNOPTIQUE 3 : Supervision Énergétique TGBT
        let syn_nrj = DashboardLayout {
            id: "energie-tgbt".to_string(),
            name: "03. Énergie — TGBT & Comptage Électrique".to_string(),
            category: "Énergie / TGBT".to_string(),
            description: "Mesure temps réel de la puissance active, de la tension triphasée et du facteur de puissance".to_string(),
            grid_size: 20,
            updated_at: Utc::now(),
            widgets: vec![
                DashboardWidget {
                    id: "w-nrj-kw".to_string(),
                    x: 20.0,
                    y: 20.0,
                    width: 250.0,
                    height: 200.0,
                    widget_type: WidgetType::Gauge,
                    bound_tag_id: Some("NRJ.TGBT.Puissance_Active".to_string()),
                    title: "Puissance Active TGBT".to_string(),
                    properties: serde_json::json!({ "min": 0, "max": 120 }),
                },
                DashboardWidget {
                    id: "w-nrj-u".to_string(),
                    x: 290.0,
                    y: 20.0,
                    width: 240.0,
                    height: 200.0,
                    widget_type: WidgetType::NumericDisplay,
                    bound_tag_id: Some("NRJ.TGBT.Tension_Triphasee".to_string()),
                    title: "Tension Composée U12".to_string(),
                    properties: serde_json::json!({}),
                },
                DashboardWidget {
                    id: "w-nrj-cos".to_string(),
                    x: 550.0,
                    y: 20.0,
                    width: 230.0,
                    height: 200.0,
                    widget_type: WidgetType::NumericDisplay,
                    bound_tag_id: Some("NRJ.TGBT.Cos_Phi".to_string()),
                    title: "Facteur de Puissance (Cos φ)".to_string(),
                    properties: serde_json::json!({}),
                },
                DashboardWidget {
                    id: "w-nrj-gf-amp".to_string(),
                    x: 800.0,
                    y: 20.0,
                    width: 220.0,
                    height: 200.0,
                    widget_type: WidgetType::NumericDisplay,
                    bound_tag_id: Some("MODBUS.GF01.Courant_Compresseur".to_string()),
                    title: "Courant Compresseur Modbus".to_string(),
                    properties: serde_json::json!({}),
                },
                DashboardWidget {
                    id: "w-nrj-trend".to_string(),
                    x: 20.0,
                    y: 240.0,
                    width: 1000.0,
                    height: 210.0,
                    widget_type: WidgetType::TrendChart,
                    bound_tag_id: Some("NRJ.TGBT.Puissance_Active".to_string()),
                    title: "Profil d'Appel de Puissance TGBT (kW)".to_string(),
                    properties: serde_json::json!({}),
                },
            ],
        };

        self.dashboards.insert(syn_cvc.id.clone(), syn_cvc);
        self.dashboards.insert(syn_chauf.id.clone(), syn_chauf);
        self.dashboards.insert(syn_nrj.id.clone(), syn_nrj);

        // FLUX 1 : Régulation PID CTA 01
        let flow_pid = FlowGraph {
            id: "flow-cta01-regulation".to_string(),
            name: "01. Régulation PID & Loi d'Eau CTA 01".to_string(),
            description: "Boucle PID calculant l'ouverture de la vanne 3 voies chauffage".to_string(),
            enabled: true,
            nodes: vec![
                FlowNode {
                    id: "n-in-temp".to_string(),
                    label: "TagInput : Temp_Soufflage".to_string(),
                    x: 40.0,
                    y: 90.0,
                    kind: FlowNodeKind::TagInput {
                        tag_id: "GTB.CTA01.Temp_Soufflage".to_string(),
                    },
                },
                FlowNode {
                    id: "n-pid-cta".to_string(),
                    label: "Régulateur PID Chauffage".to_string(),
                    x: 310.0,
                    y: 90.0,
                    kind: FlowNodeKind::PidController {
                        setpoint_tag_id: "GTB.CTA01.Consigne_Temp".to_string(),
                        kp: 14.0,
                        ki: 1.8,
                        kd: 0.5,
                        min_out: 0.0,
                        max_out: 100.0,
                        reverse_acting: false,
                    },
                },
                FlowNode {
                    id: "n-rhai-comp".to_string(),
                    label: "Rhai : Compensation Extérieure".to_string(),
                    x: 590.0,
                    y: 90.0,
                    kind: FlowNodeKind::RhaiScript {
                        script: "// Compensation loi d'eau selon la température extérieure\nlet t_ext = tag(\"GTB.CTA01.Temp_Ext\");\nlet bonus = if t_ext < 15.0 { (15.0 - t_ext) * 2.5 } else { 0.0 };\nclamp(val + bonus, 0.0, 100.0)".to_string(),
                    },
                },
                FlowNode {
                    id: "n-out-valve".to_string(),
                    label: "TagOutput : Vanne_Chaud_Cmd".to_string(),
                    x: 870.0,
                    y: 90.0,
                    kind: FlowNodeKind::TagOutput {
                        target_tag_id: "GTB.CTA01.Vanne_Chaud_Cmd".to_string(),
                    },
                },
            ],
            edges: vec![
                FlowEdge {
                    id: "e-pid-1".to_string(),
                    from_node: "n-in-temp".to_string(),
                    from_port: "out".to_string(),
                    to_node: "n-pid-cta".to_string(),
                    to_port: "in".to_string(),
                },
                FlowEdge {
                    id: "e-pid-2".to_string(),
                    from_node: "n-pid-cta".to_string(),
                    from_port: "out".to_string(),
                    to_node: "n-rhai-comp".to_string(),
                    to_port: "in".to_string(),
                },
                FlowEdge {
                    id: "e-pid-3".to_string(),
                    from_node: "n-rhai-comp".to_string(),
                    from_port: "out".to_string(),
                    to_node: "n-out-valve".to_string(),
                    to_port: "in".to_string(),
                },
            ],
        };

        // FLUX 2 : Secours Hydraulique P1 -> P2
        let flow_secours = FlowGraph {
            id: "flow-secours-pompe".to_string(),
            name: "02. Secours Hydraulique Automatique (P1 -> P2)".to_string(),
            description: "Démarre automatiquement la pompe de secours P2 si la pression collecteur chute sous 1.8 bar".to_string(),
            enabled: true,
            nodes: vec![
                FlowNode {
                    id: "n-in-press".to_string(),
                    label: "TagInput : Pression_Reseau".to_string(),
                    x: 50.0,
                    y: 120.0,
                    kind: FlowNodeKind::TagInput {
                        tag_id: "IND.CHAUF.Pression_Reseau".to_string(),
                    },
                },
                FlowNode {
                    id: "n-thresh-press".to_string(),
                    label: "Seuil Pression Basse (< 1.8 bar)".to_string(),
                    x: 330.0,
                    y: 120.0,
                    kind: FlowNodeKind::Threshold {
                        operator: ComparisonOperator::Lt,
                        threshold: 1.8,
                        hysteresis: 0.3,
                    },
                },
                FlowNode {
                    id: "n-rhai-p2-on".to_string(),
                    label: "Rhai : Activer Secours P2".to_string(),
                    x: 610.0,
                    y: 75.0,
                    kind: FlowNodeKind::RhaiScript {
                        script: "// Chute de pression détectée : démarrage pompe P2\ntrue".to_string(),
                    },
                },
                FlowNode {
                    id: "n-out-p2".to_string(),
                    label: "TagOutput : Pompe_P2_Secours".to_string(),
                    x: 880.0,
                    y: 75.0,
                    kind: FlowNodeKind::TagOutput {
                        target_tag_id: "IND.CHAUF.Pompe_P2_Secours".to_string(),
                    },
                },
            ],
            edges: vec![
                FlowEdge {
                    id: "e-sec-1".to_string(),
                    from_node: "n-in-press".to_string(),
                    from_port: "out".to_string(),
                    to_node: "n-thresh-press".to_string(),
                    to_port: "in".to_string(),
                },
                FlowEdge {
                    id: "e-sec-2".to_string(),
                    from_node: "n-thresh-press".to_string(),
                    from_port: "true".to_string(),
                    to_node: "n-rhai-p2-on".to_string(),
                    to_port: "in".to_string(),
                },
                FlowEdge {
                    id: "e-sec-3".to_string(),
                    from_node: "n-rhai-p2-on".to_string(),
                    from_port: "out".to_string(),
                    to_node: "n-out-p2".to_string(),
                    to_port: "in".to_string(),
                },
            ],
        };

        self.flows.insert(flow_pid.id.clone(), flow_pid);
        self.flows.insert(flow_secours.id.clone(), flow_secours);

        // PROGRAMMES HORAIRES GTB PAR DÉFAUT
        let sched_1 = ScheduleRule {
            id: "sched-cta01-confort".to_string(),
            name: "Programme Occupation Bureaux — CTA 01 (Confort 21.5°C / Réduit 17.5°C)"
                .to_string(),
            target_tag_id: "GTB.CTA01.Consigne_Temp".to_string(),
            start_time: "07:00".to_string(),
            end_time: "19:30".to_string(),
            active_value: TagValue::Float(21.5),
            inactive_value: TagValue::Float(17.5),
            enabled: true,
            days: crate::models::all_week_days(),
            last_applied_state: None,
        };
        let sched_2 = ScheduleRule {
            id: "sched-gf01-glacee".to_string(),
            name: "Programme Groupe Froid Modbus — Eau Glacée (Jour 7.0°C / Nuit 10.5°C)"
                .to_string(),
            target_tag_id: "MODBUS.GF01.Consigne_Eau_Glacee".to_string(),
            start_time: "06:30".to_string(),
            end_time: "20:00".to_string(),
            active_value: TagValue::Float(7.0),
            inactive_value: TagValue::Float(10.5),
            enabled: true,
            days: crate::models::all_week_days(),
            last_applied_state: None,
        };
        self.schedules.insert(sched_1.id.clone(), sched_1);
        self.schedules.insert(sched_2.id.clone(), sched_2);

        // COMPTES UTILISATEURS PAR DÉFAUT (3 GROUPES : ADMIN, SUPERVISEUR, VISUALISATION)
        let default_users = vec![
            UserAccount {
                username: "admin".to_string(),
                display_name: "Administrateur Système SCADA".to_string(),
                role: UserRole::Admin,
                password_hash: "admin123".to_string(),
                department: "Direction Technique / Automatisme".to_string(),
                enabled: true,
                created_at: Utc::now(),
                last_login_at: Some(Utc::now()),
            },
            UserAccount {
                username: "superviseur".to_string(),
                display_name: "Ingénieur Superviseur GTB".to_string(),
                role: UserRole::Supervisor,
                password_hash: "super123".to_string(),
                department: "Exploitation & Maintenance CVC".to_string(),
                enabled: true,
                created_at: Utc::now(),
                last_login_at: None,
            },
            UserAccount {
                username: "visu".to_string(),
                display_name: "Poste Opérateur Visualisation".to_string(),
                role: UserRole::Viewer,
                password_hash: "visu123".to_string(),
                department: "Salle de Contrôle / Garde".to_string(),
                enabled: true,
                created_at: Utc::now(),
                last_login_at: None,
            },
        ];

        // Les mots de passe d'usine sont hachés (Argon2id) lors du chargement initial
        // (voir `migrate_legacy_passwords`) : ils ne sont jamais conservés en clair.
        for u in default_users {
            self.users.insert(u.username.clone(), u);
        }

        // PRÉ-CHARGEMENT DU PACK INITIAL ISO 10628 / P&ID DANS LA BIBLIOTHÈQUE GÉNÉRIQUE
        for sym in Self::builtin_pack_symbols("iso_10628_pid") {
            self.symbols_library.insert(sym.id.clone(), sym);
        }
    }

    // ========================================================================
    // 9. AUTHENTIFICATION & GESTION DES UTILISATEURS (RBAC : ADMIN / SUPERVISEUR / VISUALISATION)
    // ========================================================================

    /// Authentifie un utilisateur et ouvre une session (jeton aléatoire de 256 bits).
    ///
    /// Les messages d'erreur sont volontairement identiques pour un identifiant inconnu et un
    /// mot de passe faux (pas d'énumération des comptes). Après 5 échecs consécutifs,
    /// l'identifiant est verrouillé 60 secondes.
    pub async fn login(&self, req: LoginRequest) -> Result<LoginResponse, ScadaError> {
        let key = req.username.trim().to_lowercase();
        let now = Utc::now();

        if let Some(f) = self.login_failures.get(&key) {
            let elapsed = (now - f.last_at).num_seconds();
            if f.count >= LOGIN_MAX_FAILURES && elapsed < LOGIN_LOCK_SECONDS {
                return Err(ScadaError::Forbidden(format!(
                    "Trop de tentatives échouées : réessayez dans {} s",
                    LOGIN_LOCK_SECONDS - elapsed
                )));
            }
        }

        let account = self.users.get(&key).map(|u| u.clone());
        let stored_hash = account
            .as_ref()
            .map(|u| u.password_hash.clone())
            .unwrap_or_default();
        let password = req.password.clone();
        let password_ok = !stored_hash.is_empty()
            && tokio::task::spawn_blocking(move || security::verify_password(&password, &stored_hash))
                .await
                .unwrap_or(false);

        let Some(account) = account.filter(|_| password_ok) else {
            let mut entry = self.login_failures.entry(key.clone()).or_insert(LoginFailure {
                count: 0,
                last_at: now,
            });
            // Le compteur repart de zéro une fois le verrouillage expiré
            if (now - entry.last_at).num_seconds() >= LOGIN_LOCK_SECONDS {
                entry.count = 0;
            }
            entry.count += 1;
            entry.last_at = now;
            drop(entry);
            self.record_audit(&key, "AUTH", "LOGIN_FAILED", &key, "Échec d'authentification");
            return Err(ScadaError::Unauthorized(
                "Identifiant ou mot de passe incorrect".to_string(),
            ));
        };

        if !account.enabled {
            return Err(ScadaError::Forbidden(
                "Ce compte utilisateur est désactivé par l'administrateur".to_string(),
            ));
        }

        self.login_failures.remove(&key);
        let mut account = account;
        account.last_login_at = Some(now);
        self.users.insert(account.username.clone(), account.clone());
        self.persist_user(&account).await;

        let token = security::generate_token();
        self.sessions.insert(
            token.clone(),
            Session {
                username: account.username.clone(),
                created_at: now,
                last_seen: now,
            },
        );

        self.record_audit(
            &account.username,
            "AUTH",
            "LOGIN",
            &account.username,
            &format!(
                "Connexion réussie avec le profil '{}'",
                account.role.label_fr()
            ),
        );

        Ok(LoginResponse {
            token,
            user: account.to_public_view(),
        })
    }

    pub fn logout(&self, token: &str) {
        let clean = token.strip_prefix("Bearer ").unwrap_or(token).trim();
        if let Some((_, session)) = self.sessions.remove(clean) {
            self.record_audit(
                &session.username,
                "AUTH",
                "LOGOUT",
                &session.username,
                "Déconnexion de la session utilisateur",
            );
        }
    }

    /// Résout l'utilisateur d'un jeton (`X-Scada-Token`, `Authorization: Bearer` ou `?token=`).
    ///
    /// Renvoie `None` si le jeton est absent, inconnu, expiré ou si le compte est désactivé.
    /// Il n'existe **aucun** repli vers un compte par défaut. Le jeton de contrôle local
    /// (scripts `.bat`) n'est accepté que depuis la boucle locale.
    pub fn resolve_user(&self, token: Option<&str>, peer_is_loopback: bool) -> Option<UserAccount> {
        let raw = token?;
        let clean = raw.strip_prefix("Bearer ").unwrap_or(raw).trim();
        if clean.is_empty() {
            return None;
        }

        if peer_is_loopback && constant_time_eq(clean, &self.local_control_token) {
            return self.users.get("admin").map(|u| u.clone()).filter(|u| u.enabled);
        }

        let ttl = chrono::Duration::hours(self.session_ttl_hours as i64);
        let now = Utc::now();
        let username = {
            let mut session = self.sessions.get_mut(clean)?;
            if now - session.last_seen >= ttl {
                drop(session);
                self.sessions.remove(clean);
                return None;
            }
            session.last_seen = now;
            session.username.clone()
        };
        self.users
            .get(&username)
            .map(|u| u.clone())
            .filter(|u| u.enabled)
    }

    /// Vérifie que l'utilisateur appartient au groupe `Admin` (Paramètres Système & Utilisateurs).
    pub fn require_admin(user: &UserAccount) -> Result<(), ScadaError> {
        if user.role != UserRole::Admin {
            return Err(ScadaError::Forbidden(format!(
                "Action réservée au groupe 'Administrateur' (votre groupe actuel : '{}')",
                user.role.label_fr()
            )));
        }
        Ok(())
    }

    /// Vérifie que l'utilisateur appartient au groupe `Admin` ou `Supervisor` (interdit au groupe `Viewer`).
    pub fn require_supervisor_or_admin(user: &UserAccount) -> Result<(), ScadaError> {
        if user.role == UserRole::Viewer {
            return Err(ScadaError::Forbidden(
                "Mode Visualisation uniquement : votre groupe n'autorise pas la modification ou l'envoi de consignes"
                    .to_string(),
            ));
        }
        Ok(())
    }

    pub fn get_all_users(&self) -> Vec<UserPublicView> {
        let mut list: Vec<UserPublicView> = self
            .users
            .iter()
            .map(|u| u.value().to_public_view())
            .collect();
        list.sort_by(|a, b| a.username.cmp(&b.username));
        list
    }

    /// Ferme toutes les sessions ouvertes d'un utilisateur (désactivation, suppression, changement de rôle).
    fn revoke_user_sessions(&self, username: &str) {
        self.sessions.retain(|_, s| s.username != username);
    }

    fn active_admin_count_excluding(&self, username: &str) -> usize {
        self.users
            .iter()
            .filter(|u| {
                u.key() != username && u.value().role == UserRole::Admin && u.value().enabled
            })
            .count()
    }

    pub async fn upsert_user(
        &self,
        actor: &str,
        req: UpsertUserRequest,
    ) -> Result<UserPublicView, ScadaError> {
        let uname = req.username.trim().to_lowercase();
        security::validate_id("utilisateur", &uname)?;

        let existing = self.users.get(&uname).map(|u| u.clone());

        // On ne peut pas retirer le dernier administrateur actif
        let loses_admin = existing.as_ref().is_some_and(|u| u.role == UserRole::Admin && u.enabled)
            && (req.role != UserRole::Admin || !req.enabled);
        if loses_admin && self.active_admin_count_excluding(&uname) == 0 {
            return Err(ScadaError::BadRequest(
                "Impossible : il doit rester au moins un administrateur actif".to_string(),
            ));
        }

        let new_password = req
            .password
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string);
        let password_hash = match (new_password, &existing) {
            (Some(p), _) => {
                security::validate_new_password(&p)?;
                tokio::task::spawn_blocking(move || security::hash_password(&p))
                    .await
                    .map_err(|e| ScadaError::BadRequest(e.to_string()))??
            }
            (None, Some(u)) => u.password_hash.clone(),
            (None, None) => {
                return Err(ScadaError::BadRequest(
                    "Un mot de passe est obligatoire pour créer un compte".to_string(),
                ))
            }
        };

        let role_or_state_changed = existing
            .as_ref()
            .is_some_and(|u| u.role != req.role || (u.enabled && !req.enabled));

        let account = UserAccount {
            username: uname.clone(),
            display_name: if req.display_name.trim().is_empty() {
                uname.clone()
            } else {
                req.display_name.trim().to_string()
            },
            role: req.role,
            password_hash,
            department: req.department.trim().to_string(),
            enabled: req.enabled,
            created_at: existing.as_ref().map(|u| u.created_at).unwrap_or_else(Utc::now),
            last_login_at: existing.and_then(|u| u.last_login_at),
        };

        self.users.insert(uname.clone(), account.clone());
        self.persist_user(&account).await;
        if role_or_state_changed {
            self.revoke_user_sessions(&uname);
        }

        self.record_audit(
            actor,
            "UTILISATEURS",
            "UPSERT_USER",
            &uname,
            &format!("Profil '{}' configuré dans le groupe '{}'", account.display_name, account.role.label_fr()),
        );

        Ok(account.to_public_view())
    }

    pub async fn delete_user(&self, actor: &str, username: &str) -> Result<(), ScadaError> {
        let uname = username.trim().to_lowercase();
        if uname == "admin" {
            return Err(ScadaError::BadRequest(
                "Le compte 'admin' principal ne peut pas être supprimé".to_string(),
            ));
        }
        if uname == actor {
            return Err(ScadaError::BadRequest(
                "Vous ne pouvez pas supprimer votre propre compte".to_string(),
            ));
        }
        self.users
            .remove(&uname)
            .ok_or_else(|| ScadaError::BadRequest(format!("Utilisateur '{}' introuvable", uname)))?;
        self.revoke_user_sessions(&uname);

        if let Some(pool) = &self.db_pool {
            log_db_error(
                "suppression utilisateur",
                sqlx::query("DELETE FROM users_accounts WHERE username = ?")
                    .bind(&uname)
                    .execute(pool)
                    .await,
            );
        }

        self.record_audit(
            actor,
            "UTILISATEURS",
            "DELETE_USER",
            &uname,
            "Suppression du compte utilisateur",
        );
        Ok(())
    }

    /// Changement de son propre mot de passe (contrôle de l'ancien mot de passe).
    /// Les autres sessions de l'utilisateur sont fermées ; la session courante est conservée.
    pub async fn change_own_password(
        &self,
        user: &UserAccount,
        current_token: &str,
        current_password: &str,
        new_password: &str,
    ) -> Result<(), ScadaError> {
        security::validate_new_password(new_password)?;
        let stored = user.password_hash.clone();
        let current = current_password.to_string();
        let ok = tokio::task::spawn_blocking(move || security::verify_password(&current, &stored))
            .await
            .unwrap_or(false);
        if !ok {
            return Err(ScadaError::Unauthorized(
                "Mot de passe actuel incorrect".to_string(),
            ));
        }
        let new_p = new_password.to_string();
        let hash = tokio::task::spawn_blocking(move || security::hash_password(&new_p))
            .await
            .map_err(|e| ScadaError::BadRequest(e.to_string()))??;

        let mut account = self
            .users
            .get(&user.username)
            .map(|u| u.clone())
            .ok_or_else(|| ScadaError::Unauthorized("Compte introuvable".to_string()))?;
        account.password_hash = hash;
        self.users.insert(account.username.clone(), account.clone());
        self.persist_user(&account).await;

        let keep = current_token
            .strip_prefix("Bearer ")
            .unwrap_or(current_token)
            .trim()
            .to_string();
        self.sessions
            .retain(|tok, s| s.username != account.username || *tok == keep);

        self.record_audit(
            &account.username,
            "AUTH",
            "CHANGE_PASSWORD",
            &account.username,
            "Changement du mot de passe par l'utilisateur",
        );
        Ok(())
    }

    // ========================================================================
    // 10. BIBLIOTHÈQUES GÉNÉRIQUES DE SYMBOLES INDUSTRIELS (IMPORT SVG / DRAW.IO / JSON)
    // ========================================================================

    pub fn get_all_symbols(&self) -> Vec<SymbolLibraryItem> {
        let mut list: Vec<SymbolLibraryItem> = self
            .symbols_library
            .iter()
            .map(|s| s.value().clone())
            .collect();
        list.sort_by(|a, b| a.category.cmp(&b.category).then(a.name.cmp(&b.name)));
        list
    }

    pub async fn persist_symbol(&self, sym: &SymbolLibraryItem) {
        if let Some(pool) = &self.db_pool {
            if let Ok(js) = serde_json::to_string(sym) {
                log_db_error(
                    "persistance symbole",
                    sqlx::query(
                        "INSERT INTO symbol_libraries (id, symbol_json, updated_at)
                         VALUES (?, ?, ?)
                         ON CONFLICT(id) DO UPDATE SET symbol_json = excluded.symbol_json, updated_at = excluded.updated_at",
                    )
                    .bind(&sym.id)
                    .bind(js)
                    .bind(Utc::now().to_rfc3339())
                    .execute(pool)
                    .await,
                );
            }
        }
    }

    /// Assainit (SVG, identifiant), enregistre et persiste un symbole importé.
    async fn store_symbol(&self, mut item: SymbolLibraryItem) -> SymbolLibraryItem {
        item.id = security::slugify_id(&item.id);
        item.svg_content = security::sanitize_svg(&item.svg_content);
        self.symbols_library.insert(item.id.clone(), item.clone());
        self.persist_symbol(&item).await;
        item
    }

    pub async fn delete_symbol(&self, actor: &str, id: &str) -> Result<(), ScadaError> {
        self.symbols_library
            .remove(id)
            .ok_or_else(|| ScadaError::BadRequest(format!("Symbole '{}' introuvable", id)))?;
        if let Some(pool) = &self.db_pool {
            log_db_error(
                "suppression symbole",
                sqlx::query("DELETE FROM symbol_libraries WHERE id = ?")
                    .bind(id)
                    .execute(pool)
                    .await,
            );
        }
        self.record_audit(actor, "BIBLIOTHÈQUE", "DELETE_SYMBOL", id, "Suppression d'un symbole industriel");
        Ok(())
    }

    /// Installe l'un des 3 packs industriels gratuits pré-intégrés (ISO 10628, ANSI/ISA-5.1, IEC 60617).
    pub async fn install_builtin_symbol_pack(
        &self,
        actor: &str,
        pack_id: &str,
    ) -> Result<Vec<SymbolLibraryItem>, ScadaError> {
        let items = Self::builtin_pack_symbols(pack_id);
        if items.is_empty() {
            return Err(ScadaError::BadRequest(format!(
                "Pack de symboles '{}' inconnu",
                pack_id
            )));
        }
        for sym in &items {
            self.symbols_library.insert(sym.id.clone(), sym.clone());
            self.persist_symbol(sym).await;
        }
        self.record_audit(
            actor,
            "BIBLIOTHÈQUE",
            "INSTALL_PACK",
            pack_id,
            &format!("Installation de {} symboles industriels normalisés", items.len()),
        );
        Ok(items)
    }

    /// Importe une bibliothèque générique aux formats :
    /// 1. **SVG unitaire** (`.svg` Wikimedia ISO 10628, Equinor Engineering Symbols, FUXA).
    /// 2. **SVG Sprite multi-symboles** (`<symbol id="...">...</symbol>` W3C / Inkscape).
    /// 3. **Draw.io / Diagrams.net** (`<mxlibrary>[...]</mxlibrary>` XML/JSON).
    /// 4. **Pack JSON SCADA** (`[{"name": "...", "svg_content": "..."}]`).
    pub async fn import_symbol_library(
        &self,
        actor: &str,
        req: ImportSymbolLibraryRequest,
    ) -> Result<Vec<SymbolLibraryItem>, ScadaError> {
        let raw = req.raw_content.trim();
        if raw.is_empty() {
            return Err(ScadaError::BadRequest(
                "Le contenu du fichier ou de la bibliothèque à importer est vide".to_string(),
            ));
        }

        let default_cat = if req.category.trim().is_empty() {
            "Bibliothèque Importée".to_string()
        } else {
            req.category.trim().to_string()
        };
        let default_name = if req.library_name.trim().is_empty() {
            "Symbole Industriel".to_string()
        } else {
            req.library_name.trim().to_string()
        };

        let mut imported = Vec::new();

        // CAS 1 : Format Draw.io / Diagrams.net (<mxlibrary>[...]</mxlibrary>)
        if raw.contains("<mxlibrary>") || req.format == "drawio_xml" {
            let inner = raw
                .split("<mxlibrary>")
                .nth(1)
                .and_then(|s| s.split("</mxlibrary>").next())
                .unwrap_or(raw)
                .trim();

            if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(inner) {
                for (idx, entry) in arr.iter().enumerate() {
                    let title = entry
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&default_name);
                    let svg_str = if let Some(svg_raw) = entry.get("svg").and_then(|v| v.as_str()) {
                        svg_raw.to_string()
                    } else if let Some(xml_raw) = entry.get("xml").and_then(|v| v.as_str()) {
                        if xml_raw.contains("<svg") {
                            xml_raw.to_string()
                        } else {
                            format!(
                                "<svg viewBox=\"0 0 100 100\"><rect x=\"12\" y=\"12\" width=\"76\" height=\"76\" rx=\"8\" fill=\"#0f172a\" stroke=\"currentColor\" stroke-width=\"3\"/><text x=\"50\" y=\"55\" text-anchor=\"middle\" fill=\"currentColor\" font-size=\"12\" font-weight=\"bold\">{}</text></svg>",
                                title
                            )
                        }
                    } else {
                        continue;
                    };

                    let item = SymbolLibraryItem {
                        id: format!("drawio-{}-{}", Utc::now().timestamp_millis(), idx),
                        name: title.to_string(),
                        category: default_cat.clone(),
                        standard: "Draw.io / Diagrams.net".to_string(),
                        svg_content: svg_str,
                        default_width: 210.0,
                        default_height: 185.0,
                        animate_rotation: req.animate_rotation,
                        colorize_on_state: true,
                        created_at: Utc::now(),
                    };
                    imported.push(self.store_symbol(item).await);
                }
            }
        }
        // CAS 2 : Pack JSON SCADA (Tableau d'objets ou objet contenant "symbols")
        else if raw.starts_with('{') || raw.starts_with('[') || req.format == "scada_json" {
            let val: serde_json::Value = serde_json::from_str(raw).map_err(|e| {
                ScadaError::BadRequest(format!("JSON de bibliothèque invalide : {}", e))
            })?;
            let arr_opt = if let Some(a) = val.as_array() {
                Some(a.clone())
            } else {
                val.get("symbols")
                    .and_then(|s| s.as_array())
                    .cloned()
            };

            let Some(arr) = arr_opt else {
                return Err(ScadaError::BadRequest(
                    "Le JSON doit être un tableau de symboles ou contenir une clé 'symbols'"
                        .to_string(),
                ));
            };

            for (idx, entry) in arr.iter().enumerate() {
                let name = entry
                    .get("name")
                    .or_else(|| entry.get("title"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(&default_name);
                let cat = entry
                    .get("category")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&default_cat);
                let std_name = entry
                    .get("standard")
                    .and_then(|v| v.as_str())
                    .unwrap_or("SCADA JSON Pack");
                let svg = entry
                    .get("svg_content")
                    .or_else(|| entry.get("svg"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if svg.is_empty() {
                    continue;
                }
                let anim = entry
                    .get("animate_rotation")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(req.animate_rotation);

                let item = SymbolLibraryItem {
                    id: entry
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| {
                            format!("json-sym-{}-{}", Utc::now().timestamp_millis(), idx)
                        }),
                    name: name.to_string(),
                    category: cat.to_string(),
                    standard: std_name.to_string(),
                    svg_content: svg.to_string(),
                    default_width: 210.0,
                    default_height: 185.0,
                    animate_rotation: anim,
                    colorize_on_state: true,
                    created_at: Utc::now(),
                };
                imported.push(self.store_symbol(item).await);
            }
        }
        // CAS 3 : Planche Sprite SVG multi-symboles (<symbol id="...">...</symbol>)
        else if raw.contains("<symbol") {
            for (idx, part) in raw.split("<symbol").skip(1).enumerate() {
                if let Some(end_idx) = part.find("</symbol>") {
                    let sym_block = &part[..end_idx];
                    let id_attr = sym_block
                        .split("id=\"")
                        .nth(1)
                        .and_then(|s| s.split('"').next())
                        .unwrap_or("sym");
                    let view_box = sym_block
                        .split("viewBox=\"")
                        .nth(1)
                        .and_then(|s| s.split('"').next())
                        .unwrap_or("0 0 100 100");
                    let body_start = sym_block.find('>').map(|i| i + 1).unwrap_or(0);
                    let inner_svg = &sym_block[body_start..];
                    let wrapped = format!(
                        "<svg viewBox=\"{}\" width=\"88\" height=\"88\">{}</svg>",
                        view_box, inner_svg
                    );
                    let item = SymbolLibraryItem {
                        id: format!("sprite-{}-{}-{}", id_attr, Utc::now().timestamp_millis(), idx),
                        name: format!("{} ({})", default_name, id_attr),
                        category: default_cat.clone(),
                        standard: "W3C SVG Sprite / Inkscape".to_string(),
                        svg_content: wrapped,
                        default_width: 210.0,
                        default_height: 185.0,
                        animate_rotation: req.animate_rotation,
                        colorize_on_state: true,
                        created_at: Utc::now(),
                    };
                    imported.push(self.store_symbol(item).await);
                }
            }
        }
        // CAS 4 : Fichier SVG standard unitaire (.svg ISO 10628 / Wikimedia / Equinor / FUXA)
        else if raw.contains("<svg") {
            let start = raw.find("<svg").unwrap_or(0);
            let clean_svg = raw[start..].to_string();
            let item = SymbolLibraryItem {
                id: format!("svg-custom-{}", Utc::now().timestamp_millis()),
                name: default_name,
                category: default_cat,
                standard: "SVG Vectoriel (ISO 10628 / FUXA)".to_string(),
                svg_content: clean_svg,
                default_width: 210.0,
                default_height: 185.0,
                animate_rotation: req.animate_rotation,
                colorize_on_state: true,
                created_at: Utc::now(),
            };
            imported.push(self.store_symbol(item).await);
        } else {
            return Err(ScadaError::BadRequest(
                "Format non reconnu : fournissez un fichier .svg, une planche <symbol>, un fichier Draw.io <mxlibrary> ou un pack .json".to_string(),
            ));
        }

        self.record_audit(
            actor,
            "BIBLIOTHÈQUE",
            "IMPORT_SYMBOLS",
            &req.library_name,
            &format!("Import réussi de {} symbole(s) industriel(s)", imported.len()),
        );

        Ok(imported)
    }

    pub fn builtin_pack_symbols(pack_id: &str) -> Vec<SymbolLibraryItem> {
        let now = Utc::now();
        match pack_id {
            "iso_10628_pid" => vec![
                SymbolLibraryItem {
                    id: "iso10628-agitator-tank".to_string(),
                    name: "Réacteur / Cuve Agitée".to_string(),
                    category: "ISO 10628 P&ID Process".to_string(),
                    standard: "ISO 10628 / Equinor P&ID".to_string(),
                    svg_content: r##"<svg viewBox="0 0 100 100" width="84" height="84"><rect x="20" y="14" width="60" height="72" rx="14" fill="#0b1120" stroke="currentColor" stroke-width="3.5"/><line x1="50" y1="6" x2="50" y2="68" stroke="currentColor" stroke-width="3.5"/><g class="sym-rotatable" style="transform-origin:50px 65px;"><ellipse cx="36" cy="65" rx="12" ry="5" fill="currentColor"/><ellipse cx="64" cy="65" rx="12" ry="5" fill="currentColor"/></g></svg>"##.to_string(),
                    default_width: 210.0,
                    default_height: 190.0,
                    animate_rotation: true,
                    colorize_on_state: true,
                    created_at: now,
                },
                SymbolLibraryItem {
                    id: "iso10628-cyclone-sep".to_string(),
                    name: "Séparateur Cyclone".to_string(),
                    category: "ISO 10628 P&ID Process".to_string(),
                    standard: "ISO 10628 / Wikimedia".to_string(),
                    svg_content: r##"<svg viewBox="0 0 100 100" width="84" height="84"><rect x="28" y="14" width="44" height="34" fill="#0b1120" stroke="currentColor" stroke-width="3.5"/><polygon points="28,48 72,48 50,90" fill="#0b1120" stroke="currentColor" stroke-width="3.5"/><line x1="12" y1="24" x2="28" y2="24" stroke="currentColor" stroke-width="4"/><line x1="50" y1="4" x2="50" y2="14" stroke="currentColor" stroke-width="4"/></svg>"##.to_string(),
                    default_width: 210.0,
                    default_height: 190.0,
                    animate_rotation: false,
                    colorize_on_state: true,
                    created_at: now,
                },
                SymbolLibraryItem {
                    id: "iso10628-safety-valve-psv".to_string(),
                    name: "Soupape de Sécurité PSV".to_string(),
                    category: "ISO 10628 P&ID Process".to_string(),
                    standard: "ISO 10628 / DIN".to_string(),
                    svg_content: r##"<svg viewBox="0 0 100 100" width="84" height="84"><polygon points="22,60 50,44 22,28" fill="#0b1120" stroke="currentColor" stroke-width="3"/><polygon points="50,44 34,76 66,76" fill="#0b1120" stroke="currentColor" stroke-width="3"/><line x1="50" y1="44" x2="50" y2="12" stroke="#f59e0b" stroke-width="3.5"/><polyline points="42,20 58,24 42,28 58,32" fill="none" stroke="#f59e0b" stroke-width="2.5"/></svg>"##.to_string(),
                    default_width: 210.0,
                    default_height: 185.0,
                    animate_rotation: false,
                    colorize_on_state: true,
                    created_at: now,
                },
                SymbolLibraryItem {
                    id: "iso10628-dosing-pump".to_string(),
                    name: "Pompe Doseuse Membrane".to_string(),
                    category: "ISO 10628 P&ID Process".to_string(),
                    standard: "ISO 10628 / Equinor P&ID".to_string(),
                    svg_content: r##"<svg viewBox="0 0 100 100" width="84" height="84"><circle cx="50" cy="52" r="32" fill="#0b1120" stroke="currentColor" stroke-width="3.5"/><polygon points="50,20 82,52 50,84" fill="none" stroke="currentColor" stroke-width="3"/><path d="M 35 24 Q 50 52 35 80" fill="none" stroke="#38bdf8" stroke-width="3" stroke-dasharray="4 3"/></svg>"##.to_string(),
                    default_width: 210.0,
                    default_height: 190.0,
                    animate_rotation: false,
                    colorize_on_state: true,
                    created_at: now,
                },
            ],
            "isa_5_1_instr" => vec![
                SymbolLibraryItem {
                    id: "isa51-transmitter-pt".to_string(),
                    name: "Transmetteur Pression (PT/PIT)".to_string(),
                    category: "ANSI/ISA-5.1 Instrumentation".to_string(),
                    standard: "ANSI/ISA-5.1".to_string(),
                    svg_content: r##"<svg viewBox="0 0 100 100" width="84" height="84"><circle cx="50" cy="45" r="30" fill="#0b1120" stroke="currentColor" stroke-width="3.5"/><line x1="20" y1="45" x2="80" y2="45" stroke="currentColor" stroke-width="2.5"/><text x="50" y="38" text-anchor="middle" fill="currentColor" font-size="14" font-weight="bold">PIT</text><text x="50" y="62" text-anchor="middle" fill="#94a3b8" font-size="11">101</text><line x1="50" y1="75" x2="50" y2="95" stroke="currentColor" stroke-width="3"/></svg>"##.to_string(),
                    default_width: 200.0,
                    default_height: 185.0,
                    animate_rotation: false,
                    colorize_on_state: true,
                    created_at: now,
                },
                SymbolLibraryItem {
                    id: "isa51-pneumatic-fcv".to_string(),
                    name: "Vanne Régulation Pneumatique (FCV)".to_string(),
                    category: "ANSI/ISA-5.1 Instrumentation".to_string(),
                    standard: "ANSI/ISA-5.1".to_string(),
                    svg_content: r##"<svg viewBox="0 0 100 100" width="84" height="84"><path d="M 28 32 A 22 18 0 0 1 72 32 Z" fill="#0b1120" stroke="currentColor" stroke-width="3"/><line x1="50" y1="32" x2="50" y2="62" stroke="currentColor" stroke-width="3.5"/><polygon points="20,46 50,62 20,78" fill="#0b1120" stroke="currentColor" stroke-width="3"/><polygon points="80,46 50,62 80,78" fill="#0b1120" stroke="currentColor" stroke-width="3"/></svg>"##.to_string(),
                    default_width: 210.0,
                    default_height: 185.0,
                    animate_rotation: false,
                    colorize_on_state: true,
                    created_at: now,
                },
                SymbolLibraryItem {
                    id: "isa51-radar-level-lit".to_string(),
                    name: "Sonde Niveau Radar (LIT)".to_string(),
                    category: "ANSI/ISA-5.1 Instrumentation".to_string(),
                    standard: "ANSI/ISA-5.1".to_string(),
                    svg_content: r##"<svg viewBox="0 0 100 100" width="84" height="84"><rect x="34" y="10" width="32" height="22" rx="4" fill="#0b1120" stroke="currentColor" stroke-width="3"/><polygon points="42,32 58,32 66,48 34,48" fill="#0b1120" stroke="currentColor" stroke-width="2.5"/><path d="M 38 60 Q 50 70 62 60" fill="none" stroke="#38bdf8" stroke-width="2.5"/><path d="M 30 72 Q 50 84 70 72" fill="none" stroke="#38bdf8" stroke-width="2.5"/><path d="M 22 84 Q 50 98 78 84" fill="none" stroke="#38bdf8" stroke-width="2.5"/></svg>"##.to_string(),
                    default_width: 200.0,
                    default_height: 185.0,
                    animate_rotation: false,
                    colorize_on_state: true,
                    created_at: now,
                },
            ],
            "iec_60617_cvc" => vec![
                SymbolLibraryItem {
                    id: "iec60617-genset".to_string(),
                    name: "Groupe Électrogène Secours (GE)".to_string(),
                    category: "IEC 60617 Énergie & CVC".to_string(),
                    standard: "IEC 60617".to_string(),
                    svg_content: r##"<svg viewBox="0 0 100 100" width="84" height="84"><rect x="12" y="18" width="76" height="64" rx="8" fill="#0b1120" stroke="currentColor" stroke-width="3.5"/><circle cx="50" cy="50" r="22" fill="none" stroke="currentColor" stroke-width="3"/><text x="50" y="48" text-anchor="middle" fill="currentColor" font-size="14" font-weight="bold">G</text><text x="50" y="63" text-anchor="middle" fill="#38bdf8" font-size="12">3~</text></svg>"##.to_string(),
                    default_width: 220.0,
                    default_height: 190.0,
                    animate_rotation: false,
                    colorize_on_state: true,
                    created_at: now,
                },
                SymbolLibraryItem {
                    id: "iec60617-ups-inverter".to_string(),
                    name: "Onduleur Statique ASI / UPS".to_string(),
                    category: "IEC 60617 Énergie & CVC".to_string(),
                    standard: "IEC 60617".to_string(),
                    svg_content: r##"<svg viewBox="0 0 100 100" width="84" height="84"><rect x="16" y="16" width="68" height="68" rx="6" fill="#0b1120" stroke="currentColor" stroke-width="3.5"/><line x1="16" y1="84" x2="84" y2="16" stroke="currentColor" stroke-width="2.5"/><text x="34" y="42" text-anchor="middle" fill="#f59e0b" font-size="16" font-weight="bold">=</text><text x="66" y="72" text-anchor="middle" fill="#38bdf8" font-size="18" font-weight="bold">~</text></svg>"##.to_string(),
                    default_width: 210.0,
                    default_height: 185.0,
                    animate_rotation: false,
                    colorize_on_state: true,
                    created_at: now,
                },
                SymbolLibraryItem {
                    id: "iec60617-solar-pv".to_string(),
                    name: "Champ Photovoltaïque (PV)".to_string(),
                    category: "IEC 60617 Énergie & CVC".to_string(),
                    standard: "IEC 60617".to_string(),
                    svg_content: r##"<svg viewBox="0 0 100 100" width="84" height="84"><polygon points="22,22 78,22 88,76 12,76" fill="#0b1120" stroke="currentColor" stroke-width="3"/><line x1="38" y1="22" x2="32" y2="76" stroke="currentColor" stroke-width="2"/><line x1="62" y1="22" x2="68" y2="76" stroke="currentColor" stroke-width="2"/><line x1="17" y1="48" x2="83" y2="48" stroke="currentColor" stroke-width="2"/></svg>"##.to_string(),
                    default_width: 210.0,
                    default_height: 185.0,
                    animate_rotation: false,
                    colorize_on_state: true,
                    created_at: now,
                },
            ],
            _ => vec![],
        }
    }
}


// ============================================================================
// FONCTIONS UTILITAIRES DU NOYAU
// ============================================================================

/// Journalise une erreur SQLite au lieu de l'ignorer silencieusement.
pub(crate) fn log_db_error<T>(context: &str, res: Result<T, sqlx::Error>) {
    if let Err(e) = res {
        warn!("Erreur SQLite ({}) : {}", context, e);
    }
}

/// Convertit une valeur d'ingénierie en valeur brute équipement (inverse de la mise à l'échelle).
fn to_device_value(meta: &RegisteredTag, value: &TagValue) -> TagValue {
    let has_scaling = (meta.scale_factor - 1.0).abs() > 1e-9 || meta.scale_offset.abs() > 1e-9;
    if has_scaling
        && meta.data_type == crate::models::TagDataType::Float
        && meta.scale_factor.abs() > 1e-9
    {
        if let Some(f) = value.as_f64() {
            let raw = (f - meta.scale_offset) / meta.scale_factor;
            return TagValue::Float((raw * 1_000_000.0).round() / 1_000_000.0);
        }
    }
    value.clone()
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Min, max, moyenne et écart-type (population) d'une série.
fn series_stats(nums: &[f64]) -> (Option<f64>, Option<f64>, Option<f64>, Option<f64>) {
    if nums.is_empty() {
        return (None, None, None, None);
    }
    let min_v = nums.iter().copied().fold(f64::INFINITY, f64::min);
    let max_v = nums.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let mean = nums.iter().sum::<f64>() / nums.len() as f64;
    let variance = nums.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / nums.len() as f64;
    (Some(min_v), Some(max_v), Some(round2(mean)), Some(round2(variance.sqrt())))
}

/// Identifiant de l'alarme de communication d'un driver.
pub fn comm_alarm_id(driver_id: &str) -> String {
    format!("COMM.{}", driver_id)
}

/// Rang de gravité (plus grand = plus grave).
fn severity_rank(sev: AlarmSeverity) -> u8 {
    match sev {
        AlarmSeverity::Advisory => 0,
        AlarmSeverity::Medium => 1,
        AlarmSeverity::High => 2,
        AlarmSeverity::Critical => 3,
    }
}

/// Calcule la raison d'alarme d'une valeur (ou `None` si la valeur est normale).
///
/// Ordre de priorité : défaut TOR, HH, H, LL, L. Quand l'alarme est déjà active, les seuils sont
/// décalés de la bande morte d'alarme pour éviter le bagotement autour d'un seuil.
/// Les messages sont stables (sans la valeur courante) : la valeur de déclenchement est
/// conservée à part dans `trigger_value`.
fn compute_alarm_reason(
    meta: &RegisteredTag,
    value: &TagValue,
    currently_active: bool,
) -> Option<(String, AlarmSeverity)> {
    let cfg = &meta.alarm_config;
    if !cfg.enabled {
        return None;
    }

    if let (Some(expected), Some(current)) = (cfg.trigger_on_bool, value.as_bool()) {
        if current == expected {
            let msg = cfg
                .message
                .clone()
                .unwrap_or_else(|| format!("Défaut actif sur {}", meta.label));
            return Some((msg, cfg.severity));
        }
        if matches!(value, TagValue::Bool(_)) {
            return None;
        }
    }

    let num = value.as_f64()?;
    if !num.is_finite() {
        return None;
    }
    let unit = meta.unit.as_deref().unwrap_or("");
    let band = |threshold: f64| -> f64 {
        if !currently_active {
            return 0.0;
        }
        cfg.deadband
            .filter(|d| d.is_finite() && *d >= 0.0)
            .unwrap_or_else(|| (meta.deadband.abs() * 2.0).max(threshold.abs() * 0.01))
    };
    let with_context = |base: String| match &cfg.message {
        Some(m) if !m.is_empty() => format!("{} — {}", m, base),
        _ => base,
    };

    if let Some(hh) = cfg.high_high {
        if num >= hh - band(hh) {
            return Some((
                with_context(format!("Seuil Très Haut dépassé (HH ≥ {}{})", hh, unit)),
                AlarmSeverity::Critical,
            ));
        }
    }
    if let Some(h) = cfg.high {
        if num >= h - band(h) {
            return Some((
                with_context(format!("Seuil Haut dépassé (H ≥ {}{})", h, unit)),
                cfg.severity,
            ));
        }
    }
    if let Some(ll) = cfg.low_low {
        if num <= ll + band(ll) {
            return Some((
                with_context(format!("Seuil Très Bas franchi (LL ≤ {}{})", ll, unit)),
                AlarmSeverity::Critical,
            ));
        }
    }
    if let Some(l) = cfg.low {
        if num <= l + band(l) {
            return Some((
                with_context(format!("Seuil Bas franchi (L ≤ {}{})", l, unit)),
                cfg.severity,
            ));
        }
    }
    None
}

/// Comparaison en temps constant (évite de révéler un jeton par mesure du temps de réponse).
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Compte administrateur d'usine (mot de passe haché juste après par la migration).
fn factory_admin_account() -> UserAccount {
    UserAccount {
        username: "admin".to_string(),
        display_name: "Administrateur Système SCADA".to_string(),
        role: UserRole::Admin,
        password_hash: FACTORY_ACCOUNTS[0].1.to_string(),
        department: "Direction Technique / Automatisme".to_string(),
        enabled: true,
        created_at: Utc::now(),
        last_login_at: None,
    }
}

/// Lit une colonne JSON (`js`) de toutes les lignes d'une requête, en journalisant les erreurs.
async fn load_json_column(pool: &SqlitePool, sql: &str) -> Vec<String> {
    match sqlx::query(sql).fetch_all(pool).await {
        Ok(rows) => rows
            .iter()
            .filter_map(|r| r.try_get::<String, _>("js").ok())
            .collect(),
        Err(e) => {
            warn!("Erreur SQLite (chargement '{}') : {}", sql, e);
            Vec::new()
        }
    }
}

/// Écrit un lot de points d'historique en une seule requête multi-lignes.
async fn write_history_batch(pool: &SqlitePool, batch: &[TagState]) {
    if batch.is_empty() {
        return;
    }
    let mut qb: sqlx::QueryBuilder<Sqlite> = sqlx::QueryBuilder::new(
        "INSERT INTO tag_history (tag_id, value_json, quality, recorded_at) ",
    );
    qb.push_values(batch, |mut b, st| {
        b.push_bind(st.tag_id.clone())
            .push_bind(serde_json::to_string(&st.value).unwrap_or_else(|_| "0".to_string()))
            .push_bind(format!("{:?}", st.quality))
            .push_bind(st.timestamp.to_rfc3339());
    });
    if let Err(e) = qb.build().execute(pool).await {
        warn!(
            "Erreur SQLite (écriture de {} points d'historique) : {}",
            batch.len(),
            e
        );
    }
}

/// Valide les identifiants d'un synoptique et assainit les SVG embarqués dans ses widgets.
fn sanitize_dashboard(layout: &mut DashboardLayout) -> Result<(), ScadaError> {
    security::validate_id("synoptique", &layout.id)?;
    for w in layout.widgets.iter_mut() {
        security::validate_id("widget", &w.id)?;
        if w.bound_tag_id.as_deref().is_some_and(|t| t.trim().is_empty()) {
            w.bound_tag_id = None;
        }
        if let Some(tag_id) = &w.bound_tag_id {
            security::validate_id("tag lié", tag_id)?;
        }
        let svg = w
            .properties
            .get("svg_content")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        if let Some(svg) = svg {
            w.properties["svg_content"] = serde_json::Value::String(security::sanitize_svg(&svg));
        }
        if let Some(sym_id) = w.properties.get("symbol_id").and_then(|v| v.as_str()) {
            security::validate_id("symbole", sym_id)?;
        }
    }
    Ok(())
}

/// Valide les identifiants d'un flux (nœuds, câbles, ports) et la cohérence des liaisons.
fn validate_flow(flow: &FlowGraph) -> Result<(), ScadaError> {
    security::validate_id("flux", &flow.id)?;
    let mut node_ids = std::collections::HashSet::new();
    for n in &flow.nodes {
        security::validate_id("nœud", &n.id)?;
        if !node_ids.insert(n.id.as_str()) {
            return Err(ScadaError::BadRequest(format!("Nœud '{}' dupliqué", n.id)));
        }
    }
    for e in &flow.edges {
        security::validate_id("câble", &e.id)?;
        security::validate_id("port", &e.from_port)?;
        security::validate_id("port", &e.to_port)?;
        if !node_ids.contains(e.from_node.as_str()) || !node_ids.contains(e.to_node.as_str()) {
            return Err(ScadaError::BadRequest(format!(
                "Le câble '{}' relie un nœud inexistant",
                e.id
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::TagDataType;

    fn tag(scale: f64, offset: f64) -> RegisteredTag {
        RegisteredTag {
            id: "T".into(),
            label: "T".into(),
            module_source: "D".into(),
            protocol_address: "HR:0".into(),
            data_type: TagDataType::Float,
            unit: None,
            writable: true,
            deadband: 0.0,
            scale_factor: scale,
            scale_offset: offset,
            historize: false,
            alarm_config: Default::default(),
        }
    }

    #[test]
    fn device_value_is_inverse_of_scaling() {
        let t = tag(0.1, 2.0);
        // valeur d'ingénierie 23.5 = brute × 0.1 + 2  ⇒ brute = 215
        assert_eq!(to_device_value(&t, &TagValue::Float(23.5)), TagValue::Float(215.0));
        assert_eq!(
            to_device_value(&tag(1.0, 0.0), &TagValue::Float(7.25)),
            TagValue::Float(7.25)
        );
    }

    #[test]
    fn constant_time_comparison() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
    }

    #[test]
    fn dashboard_svg_is_sanitized() {
        let mut d = DashboardLayout {
            id: "syn-1".into(),
            name: "x".into(),
            category: "c".into(),
            description: "d".into(),
            grid_size: 20,
            widgets: vec![DashboardWidget {
                id: "w-1".into(),
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
                widget_type: WidgetType::CustomSvgSymbol,
                bound_tag_id: Some(String::new()),
                title: "t".into(),
                properties: serde_json::json!({ "svg_content": "<svg onload=\"alert(1)\"><script>x</script></svg>" }),
            }],
            updated_at: Utc::now(),
        };
        sanitize_dashboard(&mut d).unwrap();
        let svg = d.widgets[0].properties["svg_content"].as_str().unwrap();
        assert!(!svg.contains("onload") && !svg.contains("script"));
        assert!(d.widgets[0].bound_tag_id.is_none());

        d.widgets[0].bound_tag_id = Some("x');alert(1)//".into());
        assert!(sanitize_dashboard(&mut d).is_err());
    }

    #[test]
    fn flow_with_dangling_edge_is_rejected() {
        let flow = FlowGraph {
            id: "f".into(),
            name: "f".into(),
            description: String::new(),
            enabled: true,
            nodes: vec![FlowNode {
                id: "n1".into(),
                label: "n1".into(),
                x: 0.0,
                y: 0.0,
                kind: FlowNodeKind::TagInput { tag_id: "T".into() },
            }],
            edges: vec![FlowEdge {
                id: "e1".into(),
                from_node: "n1".into(),
                from_port: "out".into(),
                to_node: "absent".into(),
                to_port: "in".into(),
            }],
        };
        assert!(validate_flow(&flow).is_err());
    }
    fn alarmed_tag() -> RegisteredTag {
        let mut t = tag(1.0, 0.0);
        t.unit = Some("°C".into());
        t.deadband = 0.05;
        t.alarm_config = crate::models::TagAlarmConfig {
            enabled: true,
            low: Some(15.0),
            high: Some(27.0),
            high_high: Some(31.0),
            severity: AlarmSeverity::High,
            ..Default::default()
        };
        t
    }

    #[test]
    fn alarm_levels_and_priority() {
        let t = alarmed_tag();
        assert!(compute_alarm_reason(&t, &TagValue::Float(20.0), false).is_none());
        let (msg, sev) = compute_alarm_reason(&t, &TagValue::Float(28.0), false).unwrap();
        assert_eq!(sev, AlarmSeverity::High);
        assert!(msg.contains("H ≥ 27"));
        assert_eq!(compute_alarm_reason(&t, &TagValue::Float(31.5), false).unwrap().1, AlarmSeverity::Critical);
        assert_eq!(compute_alarm_reason(&t, &TagValue::Float(14.0), false).unwrap().1, AlarmSeverity::High);
    }

    #[test]
    fn alarm_deadband_prevents_chattering() {
        let t = alarmed_tag();
        // Bande morte par défaut sur H=27 : max(2 × 0.05, 1 % de 27) = 0.27
        assert!(compute_alarm_reason(&t, &TagValue::Float(26.9), false).is_none());
        assert!(compute_alarm_reason(&t, &TagValue::Float(26.9), true).is_some(), "reste active dans la bande morte");
        assert!(compute_alarm_reason(&t, &TagValue::Float(26.7), true).is_none(), "disparaît sous seuil - bande morte");
    }

    #[test]
    fn alarm_messages_are_stable() {
        let t = alarmed_tag();
        let a = compute_alarm_reason(&t, &TagValue::Float(28.1), false).unwrap().0;
        let b = compute_alarm_reason(&t, &TagValue::Float(29.4), false).unwrap().0;
        assert_eq!(a, b, "le message ne doit pas dépendre de la valeur courante");
    }

    #[test]
    fn boolean_fault_alarm() {
        let mut t = tag(1.0, 0.0);
        t.data_type = TagDataType::Bool;
        t.alarm_config = crate::models::TagAlarmConfig {
            enabled: true,
            trigger_on_bool: Some(true),
            message: Some("Encrassement filtre".into()),
            ..Default::default()
        };
        assert_eq!(compute_alarm_reason(&t, &TagValue::Bool(true), false).unwrap().0, "Encrassement filtre");
        assert!(compute_alarm_reason(&t, &TagValue::Bool(false), true).is_none());
        t.alarm_config.enabled = false;
        assert!(compute_alarm_reason(&t, &TagValue::Bool(true), false).is_none());
    }
}
