//! # Module `models` — Modèles de données du moteur Apex Supervision
//!
//! Couvre l'intégralité des besoins d'une plateforme SCADA / GTB / GTC et Node-RED :
//! - **Tag Database** : Métadonnées, mise à l'échelle linéaire, seuils d'alarmes analogiques (LL/L/H/HH) et TOR.
//! - **Alarmes ISA-18.2** : États temps réel + Journal d'alarmes persisté SQLite.
//! - **Drivers Multi-Protocoles** : Configuration, contrôle d'exécution (Marche/Arrêt/Redémarrage) et télémétrie de trames.
//! - **Studio Synoptique** : Multi-dashboards, 14 types de widgets industriels, tuyauteries animées et faceplates.
//! - **Moteur de Flux Node-RED** : 9 types de blocs fonctionnels (incluant PID, Temporisateur TON/TOF, Formule Math, Rhai).
//! - **Planificateur Horaire GTB (Scheduler)** : Programmation horaire hebdomadaire CVC.
//! - **Traçabilité & Administration Système** : Journal d'audit (`AuditLogEntry`), sauvegarde/restauration JSON, Arrêt/Redémarrage.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ============================================================================
// 1. GESTION CENTRALISÉE DES ERREURS
// ============================================================================

#[derive(Debug, Error)]
pub enum ScadaError {
    #[error("Variable (Tag) introuvable : '{0}'")]
    TagNotFound(String),

    #[error("Écriture refusée : la variable '{0}' est configurée en lecture seule (RO)")]
    TagNotWritable(String),

    #[error("Incompatibilité de type pour le tag : attendu {expected}, reçu {actual}")]
    TypeMismatch { expected: String, actual: String },

    #[error("Driver industriel introuvable : '{0}'")]
    DriverNotFound(String),

    #[error("Alarme introuvable : '{0}'")]
    AlarmNotFound(String),

    #[error("Flux introuvable : '{0}'")]
    FlowNotFound(String),

    #[error("Erreur du driver industriel : {0}")]
    DriverError(String),

    #[error("Erreur du moteur de flux : {0}")]
    FlowError(String),

    #[error("Erreur base de données SQLite : {0}")]
    DatabaseError(String),

    #[error("Requête invalide : {0}")]
    BadRequest(String),

    #[error("{0}")]
    Unauthorized(String),

    #[error("Accès refusé (Privilèges insuffisants) : {0}")]
    Forbidden(String),

    #[error("Serveur occupé : {0}")]
    Busy(String),
}

impl ScadaError {
    /// Code HTTP correspondant (le moteur ne dépend d'aucun framework web).
    pub fn http_status(&self) -> u16 {
        match self {
            ScadaError::TagNotFound(_)
            | ScadaError::DriverNotFound(_)
            | ScadaError::AlarmNotFound(_)
            | ScadaError::FlowNotFound(_) => 404,
            ScadaError::TagNotWritable(_) | ScadaError::Forbidden(_) => 403,
            ScadaError::Unauthorized(_) => 401,
            ScadaError::TypeMismatch { .. } | ScadaError::BadRequest(_) => 400,
            ScadaError::DriverError(_) | ScadaError::FlowError(_) => 502,
            ScadaError::DatabaseError(_) => 500,
            ScadaError::Busy(_) => 503,
        }
    }
}

// ============================================================================
// 2. TAG DATABASE TEMPS RÉEL (ESPRIT PCVUE)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TagDataType {
    Bool,
    Float,
    Int,
    Text,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TagValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

impl TagValue {
    pub fn data_type(&self) -> TagDataType {
        match self {
            TagValue::Bool(_) => TagDataType::Bool,
            TagValue::Int(_) => TagDataType::Int,
            TagValue::Float(_) => TagDataType::Float,
            TagValue::Text(_) => TagDataType::Text,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            TagValue::Float(v) => Some(*v),
            TagValue::Int(v) => Some(*v as f64),
            TagValue::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            TagValue::Text(s) => s.trim().parse::<f64>().ok(),
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            TagValue::Bool(b) => Some(*b),
            TagValue::Int(i) => Some(*i != 0),
            TagValue::Float(f) => Some(f.abs() > f64::EPSILON),
            TagValue::Text(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "on" | "marche" => Some(true),
                "false" | "0" | "off" | "arret" | "arrêt" => Some(false),
                _ => None,
            },
        }
    }

    pub fn coerce_to(&self, target: TagDataType) -> Result<TagValue, ScadaError> {
        match (self, target) {
            (TagValue::Bool(b), TagDataType::Bool) => Ok(TagValue::Bool(*b)),
            (TagValue::Int(i), TagDataType::Int) => Ok(TagValue::Int(*i)),
            (TagValue::Float(f), TagDataType::Float) => Ok(TagValue::Float(*f)),
            (TagValue::Text(s), TagDataType::Text) => Ok(TagValue::Text(s.clone())),
            (TagValue::Int(i), TagDataType::Float) => Ok(TagValue::Float(*i as f64)),
            (TagValue::Float(f), TagDataType::Int) => Ok(TagValue::Int(f.round() as i64)),
            (other, TagDataType::Bool) => {
                other
                    .as_bool()
                    .map(TagValue::Bool)
                    .ok_or_else(|| ScadaError::TypeMismatch {
                        expected: "Bool".to_string(),
                        actual: format!("{:?}", other),
                    })
            }
            (other, TagDataType::Float) => {
                other
                    .as_f64()
                    .map(TagValue::Float)
                    .ok_or_else(|| ScadaError::TypeMismatch {
                        expected: "Float".to_string(),
                        actual: format!("{:?}", other),
                    })
            }
            (other, TagDataType::Int) => other
                .as_f64()
                .map(|v| TagValue::Int(v.round() as i64))
                .ok_or_else(|| ScadaError::TypeMismatch {
                    expected: "Int".to_string(),
                    actual: format!("{:?}", other),
                }),
            (other, TagDataType::Text) => Ok(TagValue::Text(match other {
                TagValue::Bool(b) => b.to_string(),
                TagValue::Int(i) => i.to_string(),
                TagValue::Float(f) => format!("{:.2}", f),
                TagValue::Text(s) => s.clone(),
            })),
        }
    }

    pub fn has_significant_change(&self, previous: &Self, deadband: f64) -> bool {
        match (self, previous) {
            (TagValue::Float(new_v), TagValue::Float(old_v)) => {
                (new_v - old_v).abs() >= deadband.max(1e-6)
            }
            _ => self != previous,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Quality {
    Good,
    Uncertain,
    BadDisconnect,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TagAlarmConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub trigger_on_bool: Option<bool>,
    #[serde(default)]
    pub low_low: Option<f64>,
    #[serde(default)]
    pub low: Option<f64>,
    #[serde(default)]
    pub high: Option<f64>,
    #[serde(default)]
    pub high_high: Option<f64>,
    #[serde(default)]
    pub severity: AlarmSeverity,
    #[serde(default)]
    pub message: Option<String>,
    /// Bande morte d'alarme (unités du tag) : une alarme active ne disparaît qu'une fois la
    /// valeur revenue au-delà du seuil de cette marge. Par défaut : max(2 × deadband COV, 1 % du seuil).
    #[serde(default)]
    pub deadband: Option<f64>,
    /// Interdit la mise en sommeil de cette alarme (ex. alarmes de sécurité, ISA-18.2).
    #[serde(default)]
    pub shelving_disabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredTag {
    pub id: String,
    pub label: String,
    pub module_source: String,
    pub protocol_address: String,
    pub data_type: TagDataType,
    pub unit: Option<String>,
    pub writable: bool,
    #[serde(default = "default_deadband")]
    pub deadband: f64,
    #[serde(default = "default_scale_factor")]
    pub scale_factor: f64,
    #[serde(default)]
    pub scale_offset: f64,
    #[serde(default = "default_true")]
    pub historize: bool,
    #[serde(default)]
    pub alarm_config: TagAlarmConfig,
}

fn default_deadband() -> f64 {
    0.05
}
fn default_scale_factor() -> f64 {
    1.0
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagState {
    pub tag_id: String,
    pub value: TagValue,
    pub quality: Quality,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagFullView {
    #[serde(flatten)]
    pub metadata: RegisteredTag,
    pub state: Option<TagState>,
}

// ============================================================================
// 3. ALARMES ISA-18.2 & JOURNAL D'ALARMES
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AlarmSeverity {
    Critical,
    #[default]
    High,
    Medium,
    Advisory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AlarmLifecycleState {
    ActiveUnack,
    ActiveAck,
    ClearedUnack,
    Normal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlarmInstance {
    pub id: String,
    pub tag_id: String,
    pub tag_label: String,
    pub severity: AlarmSeverity,
    pub state: AlarmLifecycleState,
    pub message: String,
    pub trigger_value: TagValue,
    pub raised_at: DateTime<Utc>,
    pub cleared_at: Option<DateTime<Utc>>,
    pub acked_at: Option<DateTime<Utc>>,
    pub acked_by: Option<String>,
    pub ack_comment: Option<String>,
    /// Mise en sommeil (ISA-18.2 « shelving ») : alarme masquée du bandeau jusqu'à cette date.
    #[serde(default)]
    pub shelved_until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub shelved_by: Option<String>,
    /// Niveau de seuil à l'origine de l'alarme active (la bande morte d'alarme ne s'applique
    /// qu'à ce niveau). Absent pour une alarme de communication ou une alarme d'avant la v1.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<AlarmLevel>,
}

/// Niveau d'une alarme de variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AlarmLevel {
    /// Défaut tout-ou-rien (valeur booléenne attendue).
    Tor,
    HighHigh,
    High,
    LowLow,
    Low,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlarmJournalEntry {
    pub id: i64,
    pub tag_id: String,
    pub severity: String,
    pub event_type: String,
    pub message: String,
    pub operator: Option<String>,
    pub recorded_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShelveAlarmRequest {
    /// Durée de mise en sommeil en minutes (1 à 480).
    pub minutes: u32,
    #[serde(default)]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AckAlarmRequest {
    /// Conservé pour compatibilité : l'opérateur enregistré est toujours l'utilisateur authentifié.
    #[serde(default)]
    pub operator: String,
    #[serde(default)]
    pub comment: Option<String>,
}

// ============================================================================
// 4. DRIVERS INDUSTRIELS & CONTRÔLE D'EXÉCUTION
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProtocolType {
    Simulator,
    ModbusTcp,
    Mqtt,
    OpcUa,
    SiemensS7,
    BacnetIp,
}

impl ProtocolType {
    /// Protocoles sans implémentation réelle : simulateur GTB et drivers OPC-UA, BACnet/IP et
    /// Siemens S7 (qui génèrent des valeurs fictives). Ils ne démarrent qu'avec la simulation active.
    pub fn is_simulated(&self) -> bool {
        matches!(
            self,
            ProtocolType::Simulator
                | ProtocolType::OpcUa
                | ProtocolType::BacnetIp
                | ProtocolType::SiemensS7
        )
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DriverRuntimeStats {
    pub running: bool,
    pub connected: bool,
    pub frames_ok: u64,
    pub frames_err: u64,
    pub last_latency_ms: u64,
    pub last_poll_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    /// Dernière commande refusée par l'équipement (exception Modbus, valeur invalide). Elle
    /// reste affichée jusqu'à la prochaine écriture acceptée, contrairement à `last_error` que
    /// la scrutation suivante efface.
    #[serde(default)]
    pub last_write_rejected: Option<String>,
    #[serde(default)]
    pub last_write_rejected_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriverConfig {
    pub id: String,
    pub name: String,
    pub protocol: ProtocolType,
    pub endpoint: String,
    #[serde(default = "default_unit_id")]
    pub unit_id: u8,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_ms: u64,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub tags: Vec<RegisteredTag>,
    #[serde(default)]
    pub stats: DriverRuntimeStats,
}

fn default_unit_id() -> u8 {
    1
}
fn default_poll_interval() -> u64 {
    1000
}

/// Modification des paramètres de communication d'un driver existant (champs facultatifs).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DriverUpdateRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub unit_id: Option<u8>,
    #[serde(default)]
    pub poll_interval_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct WriteCommand {
    pub tag_id: String,
    pub protocol_address: String,
    pub value: TagValue,
}

// ============================================================================
// 5. STUDIO SYNOPTIQUE DRAG & DROP
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WidgetType {
    Pump,
    Valve,
    MotorValve2Way,
    Fan,
    Boiler,
    Chiller,
    Compressor,
    HeatExchanger,
    CoolingTower,
    HeaterBattery,
    FilterUnit,
    Damper,
    CircuitBreaker,
    Transformer,
    ElectricMotor,
    FlowMeter,
    PressureGauge,
    Thermometer,
    Gauge,
    TrendChart,
    IndicatorLed,
    SetpointButton,
    SwitchButton,
    PushButton,
    TankLevel,
    NumericDisplay,
    PipeSegment,
    CustomSvgSymbol,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardWidget {
    pub id: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub widget_type: WidgetType,
    pub bound_tag_id: Option<String>,
    pub title: String,
    #[serde(default)]
    pub properties: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardLayout {
    pub id: String,
    pub name: String,
    pub category: String,
    pub description: String,
    pub grid_size: u32,
    pub widgets: Vec<DashboardWidget>,
    /// Fixée par le serveur à chaque enregistrement : facultative dans les requêtes.
    #[serde(default = "Utc::now")]
    pub updated_at: DateTime<Utc>,
}

// ============================================================================
// 6. MOTEUR DE FLUX VISUEL (ESPRIT NODE-RED)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ComparisonOperator {
    Lt,
    Lte,
    Gt,
    Gte,
    Eq,
    Neq,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogicGateType {
    And,
    Or,
    Xor,
    Not,
    Nand,
    Nor,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "node_type")]
pub enum FlowNodeKind {
    TagInput {
        tag_id: String,
    },
    Threshold {
        operator: ComparisonOperator,
        threshold: f64,
        #[serde(default)]
        hysteresis: f64,
    },
    Hysteresis {
        low_threshold: f64,
        high_threshold: f64,
        #[serde(default)]
        invert: bool,
    },
    PidController {
        setpoint_tag_id: String,
        kp: f64,
        ki: f64,
        kd: f64,
        min_out: f64,
        max_out: f64,
        #[serde(default)]
        reverse_acting: bool,
    },
    LogicGate {
        gate: LogicGateType,
    },
    /// Temporisateur automate TON (retard à la montée) en millisecondes.
    DelayTimer {
        delay_ms: u64,
    },
    /// Bloc de mise à l'échelle / formule linéaire (`output = input * factor + offset`).
    MathFormula {
        factor: f64,
        offset: f64,
    },
    RhaiScript {
        script: String,
    },
    TagOutput {
        target_tag_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowNode {
    pub id: String,
    pub label: String,
    pub x: f64,
    pub y: f64,
    pub kind: FlowNodeKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowEdge {
    pub id: String,
    pub from_node: String,
    pub from_port: String,
    pub to_node: String,
    #[serde(default = "default_in_port")]
    pub to_port: String,
}

fn default_in_port() -> String {
    "in".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowGraph {
    pub id: String,
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub nodes: Vec<FlowNode>,
    pub edges: Vec<FlowEdge>,
}

// ============================================================================
// 7. PLANIFICATEUR HORAIRE GTB (SCHEDULER CVC)
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleRule {
    pub id: String,
    pub name: String,
    pub target_tag_id: String,
    /// Heure de début au format `"HH:MM"` (ex: `"07:00"`).
    pub start_time: String,
    /// Heure de fin au format `"HH:MM"` (ex: `"19:00"`).
    pub end_time: String,
    /// Valeur appliquée pendant la plage active (Mode Confort).
    pub active_value: TagValue,
    /// Valeur appliquée hors plage (Mode Réduit / Nuit).
    pub inactive_value: TagValue,
    pub enabled: bool,
    /// Jours actifs (1 = lundi … 7 = dimanche). Par défaut : tous les jours.
    #[serde(default = "all_week_days")]
    pub days: Vec<u8>,
    #[serde(default)]
    pub last_applied_state: Option<bool>,
}

/// Planning tel que renvoyé à l'IHM : la règle, plus la raison pour laquelle elle ne
/// s'applique pas en ce moment (mode sûr), calculée par le serveur.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleView {
    #[serde(flatten)]
    pub rule: ScheduleRule,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspended_reason: Option<String>,
}

/// Flux tel que renvoyé à l'IHM : le graphe, plus ses sorties bloquées par le mode sûr.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowView {
    #[serde(flatten)]
    pub flow: FlowGraph,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspended_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_outputs: Vec<String>,
}

pub fn all_week_days() -> Vec<u8> {
    vec![1, 2, 3, 4, 5, 6, 7]
}

impl ScheduleRule {
    /// Indique si la plage « Confort » est active pour un jour (1 = lundi … 7 = dimanche)
    /// et une heure `"HH:MM"` donnés. Une plage qui passe minuit (ex. 22:00 → 06:00) est
    /// rattachée à son jour de début.
    pub fn is_active_at(&self, weekday: u8, hhmm: &str) -> bool {
        let yesterday = if weekday <= 1 { 7 } else { weekday - 1 };
        let today_on = self.days.contains(&weekday);
        if self.start_time <= self.end_time {
            today_on && hhmm >= self.start_time.as_str() && hhmm < self.end_time.as_str()
        } else {
            (today_on && hhmm >= self.start_time.as_str())
                || (self.days.contains(&yesterday) && hhmm < self.end_time.as_str())
        }
    }
}

/// Vérifie le format `"HH:MM"` (00:00 à 23:59).
pub fn is_valid_hhmm(s: &str) -> bool {
    let b = s.as_bytes();
    if !s.is_ascii() || b.len() != 5 || b[2] != b':' {
        return false;
    }
    match (s[0..2].parse::<u8>(), s[3..5].parse::<u8>()) {
        (Ok(h), Ok(m)) => h < 24 && m < 60 && b[0].is_ascii_digit() && b[3].is_ascii_digit(),
        _ => false,
    }
}

// ============================================================================
// 8. HISTORIAN, AUDIT LOGS & CONTRÔLE SYSTÈME (SHUTDOWN / RESTART / EXPORT)
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryPoint {
    pub timestamp: String,
    pub value: TagValue,
    pub quality: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryResponse {
    pub tag_id: String,
    pub unit: Option<String>,
    pub count: usize,
    /// Nombre total de points sur la période (avant sous-échantillonnage).
    #[serde(default)]
    pub total_points: usize,
    /// Vrai si les points renvoyés sont un échantillon régulier de la période.
    #[serde(default)]
    pub sampled: bool,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub avg: Option<f64>,
    pub std_dev: Option<f64>,
    pub points: Vec<HistoryPoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditLogEntry {
    pub id: i64,
    pub timestamp: String,
    pub actor: String,
    pub category: String,
    pub action: String,
    pub target: String,
    pub details: String,
}

/// Réglages système persistés (table `meta`, clé `settings`), modifiables par l'administrateur.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemSettings {
    /// Autorise la mise en sommeil (« shelving ») des alarmes. Désactivée, toutes les alarmes en
    /// sommeil sont réveillées et plus aucune ne peut être mise en sommeil.
    #[serde(default = "default_true")]
    pub alarm_shelving_enabled: bool,
    /// Durée maximale d'une mise en sommeil, en minutes (1 à 480).
    #[serde(default = "default_shelve_max")]
    pub alarm_shelve_max_minutes: u32,
}

fn default_shelve_max() -> u32 {
    480
}

impl Default for SystemSettings {
    fn default() -> Self {
        Self {
            alarm_shelving_enabled: true,
            alarm_shelve_max_minutes: default_shelve_max(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemStatus {
    pub app_name: String,
    pub version: String,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub uptime_seconds: u64,
    pub acquisition_paused: bool,
    pub total_tags: usize,
    pub total_drivers: usize,
    pub running_drivers: usize,
    pub active_flows: usize,
    pub active_alarms: usize,
    pub cov_events_total: u64,
    pub modbus_slave_port: u16,
    /// Mode sûr : aucune commande vers les équipements réels.
    #[serde(default)]
    pub safe_mode: bool,
    /// Simulation active (automate embarqué, simulateur, protocoles simulés).
    #[serde(default)]
    pub simulation_enabled: bool,
    /// Points d'historique écartés car la file d'écriture était saturée.
    #[serde(default)]
    pub history_dropped: u64,
    #[serde(default)]
    pub history_retention_days: u32,
    /// Réglages système modifiables par l'administrateur.
    #[serde(default)]
    pub settings: SystemSettings,
}

/// Archive complète exportable/importable d'un projet Apex Supervision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectBackupBundle {
    pub exported_at: DateTime<Utc>,
    pub version: String,
    pub drivers: Vec<DriverConfig>,
    pub tags: Vec<RegisteredTag>,
    pub dashboards: Vec<DashboardLayout>,
    pub flows: Vec<FlowGraph>,
    pub schedules: Vec<ScheduleRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteTagRequest {
    pub value: TagValue,
    #[serde(default)]
    pub operator_note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowExecutionTrace {
    pub flow_id: String,
    pub from_node_id: String,
    pub to_node_id: String,
    pub port: String,
    pub value: TagValue,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data")]
pub enum WsServerMessage {
    InitialSnapshot {
        tags: Vec<TagFullView>,
        flows: Vec<FlowView>,
        alarms: Vec<AlarmInstance>,
        drivers: Vec<DriverConfig>,
        schedules: Vec<ScheduleView>,
        status: SystemStatus,
    },
    TagUpdate(TagState),
    /// Lot de mises à jour COV (dernière valeur par tag sur une fenêtre de 100 ms).
    TagUpdateBatch(Vec<TagState>),
    AlarmUpdate(Vec<AlarmInstance>),
    DriverListUpdate(Vec<DriverConfig>),
    FlowTrace(FlowExecutionTrace),
    /// Réglages système modifiés par un administrateur.
    SettingsUpdate(SystemSettings),
    SystemStateEvent {
        state: String,
        message: String,
    },
}

// ============================================================================
// 9. GESTION DES UTILISATEURS & CONTRÔLE D'ACCÈS PAR GROUPES (RBAC)
// ============================================================================

/// Les 3 groupes d'utilisateurs industriels :
/// - `Admin` : Tous les droits (Exploitation, Studio, Flux, Drivers, Alarmes, Plannings, Paramètres Système & Utilisateurs).
/// - `Supervisor` : Tous les droits métiers (Exploitation + Studio + Flux + Drivers + Alarmes + Plannings) SAUF la gestion des paramètres système et des utilisateurs.
/// - `Viewer` : Visualisation uniquement des synoptiques/dashboards en lecture seule (aucune écriture, aucun accès Studio ni ateliers techniques).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UserRole {
    Admin,
    Supervisor,
    Viewer,
}

impl UserRole {
    pub fn label_fr(&self) -> &'static str {
        match self {
            UserRole::Admin => "Administrateur",
            UserRole::Supervisor => "Superviseur",
            UserRole::Viewer => "Visualisation",
        }
    }

    pub fn permissions(&self) -> UserPermissions {
        match self {
            UserRole::Admin => UserPermissions {
                can_view_dashboards: true,
                can_write_tags: true,
                can_edit_studio: true,
                can_manage_flows: true,
                can_manage_drivers_tags: true,
                can_view_alarms: true,
                can_ack_alarms: true,
                can_manage_alarms: true,
                can_manage_schedules: true,
                can_view_historian: true,
                can_manage_system: true,
                can_manage_users: true,
            },
            UserRole::Supervisor => UserPermissions {
                can_view_dashboards: true,
                can_write_tags: true,
                can_edit_studio: true,
                can_manage_flows: true,
                can_manage_drivers_tags: true,
                can_view_alarms: true,
                can_ack_alarms: true,
                can_manage_alarms: true,
                can_manage_schedules: true,
                can_view_historian: true,
                can_manage_system: false,
                can_manage_users: false,
            },
            UserRole::Viewer => UserPermissions {
                can_view_dashboards: true,
                can_write_tags: false,
                can_edit_studio: false,
                can_manage_flows: false,
                can_manage_drivers_tags: false,
                can_view_alarms: true,
                // Acquittement d'une alarme à la fois, confirmé dans l'IHM et tracé à son nom
                can_ack_alarms: true,
                can_manage_alarms: false,
                can_manage_schedules: false,
                can_view_historian: false,
                can_manage_system: false,
                can_manage_users: false,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPermissions {
    pub can_view_dashboards: bool,
    pub can_write_tags: bool,
    pub can_edit_studio: bool,
    pub can_manage_flows: bool,
    pub can_manage_drivers_tags: bool,
    /// Consultation de la liste et du journal des alarmes.
    pub can_view_alarms: bool,
    /// Acquittement d'une alarme à la fois.
    pub can_ack_alarms: bool,
    /// Acquittement global, mise en sommeil et purge des alarmes revenues à la normale.
    pub can_manage_alarms: bool,
    pub can_manage_schedules: bool,
    pub can_view_historian: bool,
    pub can_manage_system: bool,
    pub can_manage_users: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserAccount {
    pub username: String,
    pub display_name: String,
    pub role: UserRole,
    pub password_hash: String,
    pub department: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
}

impl UserAccount {
    pub fn to_public_view(&self) -> UserPublicView {
        UserPublicView {
            username: self.username.clone(),
            display_name: self.display_name.clone(),
            role: self.role,
            role_label: self.role.label_fr().to_string(),
            department: self.department.clone(),
            enabled: self.enabled,
            created_at: self.created_at,
            last_login_at: self.last_login_at,
            permissions: self.role.permissions(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPublicView {
    pub username: String,
    pub display_name: String,
    pub role: UserRole,
    pub role_label: String,
    pub department: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
    pub permissions: UserPermissions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginResponse {
    pub token: String,
    pub user: UserPublicView,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpsertUserRequest {
    pub username: String,
    pub display_name: String,
    pub role: UserRole,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub department: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

// ============================================================================
// 10. BIBLIOTHÈQUES GÉNÉRIQUES DE SYMBOLES INDUSTRIELS (SVG / P&ID / DRAW.IO / FUXA)
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolLibraryItem {
    pub id: String,
    pub name: String,
    pub category: String,
    pub standard: String,
    pub svg_content: String,
    #[serde(default = "default_sym_w")]
    pub default_width: f64,
    #[serde(default = "default_sym_h")]
    pub default_height: f64,
    #[serde(default)]
    pub animate_rotation: bool,
    #[serde(default = "default_true")]
    pub colorize_on_state: bool,
    pub created_at: DateTime<Utc>,
}

fn default_sym_w() -> f64 {
    210.0
}
fn default_sym_h() -> f64 {
    185.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportSymbolLibraryRequest {
    /// Format d'entrée : `"svg"`, `"drawio_xml"`, `"scada_json"` ou `"auto"`
    #[serde(default)]
    pub format: String,
    /// Nom par défaut ou nom de la bibliothèque
    #[serde(default)]
    pub library_name: String,
    /// Catégorie cible (ex: `"ISO 10628 P&ID"`, `"ISA-5.1 Instrumentation"`, `"CVC / GTB"`)
    #[serde(default)]
    pub category: String,
    /// Contenu brut du fichier (.svg, .xml Draw.io `<mxlibrary>`, ou .json pack)
    pub raw_content: String,
    #[serde(default)]
    pub animate_rotation: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_value_coercion() {
        assert_eq!(
            TagValue::Int(3).coerce_to(TagDataType::Float).unwrap(),
            TagValue::Float(3.0)
        );
        assert_eq!(
            TagValue::Float(2.6).coerce_to(TagDataType::Int).unwrap(),
            TagValue::Int(3)
        );
        assert_eq!(
            TagValue::Text("marche".into())
                .coerce_to(TagDataType::Bool)
                .unwrap(),
            TagValue::Bool(true)
        );
        assert_eq!(
            TagValue::Text(" 21.5 ".into())
                .coerce_to(TagDataType::Float)
                .unwrap(),
            TagValue::Float(21.5)
        );
        assert_eq!(
            TagValue::Bool(true).coerce_to(TagDataType::Text).unwrap(),
            TagValue::Text("true".into())
        );
        assert!(TagValue::Text("abc".into())
            .coerce_to(TagDataType::Float)
            .is_err());
    }

    #[test]
    fn deadband_filtering() {
        let old = TagValue::Float(20.0);
        assert!(!TagValue::Float(20.04).has_significant_change(&old, 0.05));
        assert!(TagValue::Float(20.06).has_significant_change(&old, 0.05));
        assert!(TagValue::Bool(true).has_significant_change(&TagValue::Bool(false), 0.5));
        assert!(!TagValue::Int(4).has_significant_change(&TagValue::Int(4), 0.0));
    }

    fn rule(start: &str, end: &str, days: Vec<u8>) -> ScheduleRule {
        ScheduleRule {
            id: "r".into(),
            name: "r".into(),
            target_tag_id: "t".into(),
            start_time: start.into(),
            end_time: end.into(),
            active_value: TagValue::Float(1.0),
            inactive_value: TagValue::Float(0.0),
            enabled: true,
            days,
            last_applied_state: None,
        }
    }

    #[test]
    fn schedule_day_window() {
        let r = rule("07:00", "19:00", vec![1, 2, 3, 4, 5]);
        assert!(r.is_active_at(1, "07:00"));
        assert!(r.is_active_at(5, "18:59"));
        assert!(!r.is_active_at(5, "19:00"));
        assert!(!r.is_active_at(6, "10:00")); // samedi exclu
    }

    #[test]
    fn schedule_overnight_window() {
        let r = rule("22:00", "06:00", vec![5]); // nuit du vendredi au samedi
        assert!(r.is_active_at(5, "23:30"));
        assert!(r.is_active_at(6, "05:59")); // samedi matin, rattaché au vendredi
        assert!(!r.is_active_at(6, "23:30"));
        assert!(!r.is_active_at(5, "05:00")); // jeudi soir non programmé
    }

    #[test]
    fn schedule_default_days_and_json_compat() {
        let js = r#"{"id":"s","name":"n","target_tag_id":"t","start_time":"07:00","end_time":"19:00","active_value":21.5,"inactive_value":17.5,"enabled":true}"#;
        let r: ScheduleRule = serde_json::from_str(js).unwrap();
        assert_eq!(r.days, all_week_days());
    }

    #[test]
    fn hhmm_validation() {
        assert!(is_valid_hhmm("00:00"));
        assert!(is_valid_hhmm("23:59"));
        assert!(!is_valid_hhmm("24:00"));
        assert!(!is_valid_hhmm("7:00"));
        assert!(!is_valid_hhmm("+7:00"));
        assert!(!is_valid_hhmm("07h00"));
        assert!(!is_valid_hhmm("é7:00"));
    }
    #[test]
    fn settings_defaults_and_compat() {
        let d = SystemSettings::default();
        assert!(d.alarm_shelving_enabled);
        assert_eq!(d.alarm_shelve_max_minutes, 480);
        let partial: SystemSettings =
            serde_json::from_str(r#"{"alarm_shelving_enabled":false}"#).unwrap();
        assert!(!partial.alarm_shelving_enabled);
        assert_eq!(partial.alarm_shelve_max_minutes, 480);
    }

    #[test]
    fn legacy_alarm_and_tag_json_still_load() {
        // Alarme persistée avant la v1.2 (sans champs de mise en sommeil)
        let js = r#"{"id":"T","tag_id":"T","tag_label":"t","severity":"High","state":"ActiveAck","message":"m","trigger_value":1.5,"raised_at":"2026-10-16T10:00:00Z","cleared_at":null,"acked_at":null,"acked_by":"admin","ack_comment":null}"#;
        let a: AlarmInstance = serde_json::from_str(js).unwrap();
        assert!(a.shelved_until.is_none());
        assert_eq!(a.state, AlarmLifecycleState::ActiveAck);
        // Configuration d'alarme sans les champs deadband / shelving_disabled
        let cfg: TagAlarmConfig = serde_json::from_str(r#"{"enabled":true,"high":27.0}"#).unwrap();
        assert!(!cfg.shelving_disabled);
        assert!(cfg.deadband.is_none());
    }
}
