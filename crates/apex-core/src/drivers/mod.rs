//! # Module `drivers` — Gestionnaire de Drivers Industriels Multi-Protocoles avec Cycle de Vie Complet
//!
//! Chaque driver industriel (`Simulator`, `ModbusTcp`, `Mqtt`, `OpcUa`, `BacnetIp`, `SiemensS7`)
//! est supervisé par une tâche Tokio contrôlable (`Start`, `Stop`, `Restart`) et remonte en temps réel
//! ses compteurs de trames (`frames_ok`, `frames_err`) et sa latence réseau (`last_latency_ms`).

pub mod bacnet;
pub mod modbus_slave;
pub mod opc_ua;
pub mod s7;

use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use byteorder::{BigEndian, ByteOrder};
use chrono::Utc;
use dashmap::DashMap;
use rand::Rng;
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use tokio::{sync::mpsc, task::AbortHandle};
use tokio_modbus::prelude::*;
use tracing::{info, warn};

use crate::{
    core::ScadaCore,
    models::{
        AlarmSeverity, DriverConfig, DriverRuntimeStats, ProtocolType, Quality, RegisteredTag,
        TagAlarmConfig, TagDataType, TagState, TagValue, WriteCommand,
    },
};

#[async_trait]
pub trait IndustrialDriver: Send + Sync {
    /// Identifiant du driver (clé de configuration).
    fn id(&self) -> &str;
    async fn start(&mut self, core_tx: mpsc::Sender<TagState>) -> Result<AbortHandle, String>;
    async fn write_tag(&self, address: &str, value: TagValue) -> Result<(), String>;
    async fn browse_tags(&self) -> Result<Vec<RegisteredTag>, String>;
}

/// Construit la configuration du vrai automate Modbus TCP local connecté à `127.0.0.1:5502`.
pub fn build_local_modbus_plc_config(port: u16) -> DriverConfig {
    let src = "MODBUS_GF01".to_string();
    let tags = vec![
        RegisteredTag {
            id: "MODBUS.GF01.Compresseur_Marche".to_string(),
            label: "Compresseur Groupe Froid GF01 (Coil 0)".to_string(),
            module_source: src.clone(),
            protocol_address: "COIL:0".to_string(),
            data_type: TagDataType::Bool,
            unit: None,
            writable: true,
            deadband: 0.0,
            scale_factor: 1.0,
            scale_offset: 0.0,
            historize: true,
            alarm_config: TagAlarmConfig::default(),
        },
        RegisteredTag {
            id: "MODBUS.GF01.Pompe_Glycol_Marche".to_string(),
            label: "Pompe Eau Glacée Glycolée (Coil 1)".to_string(),
            module_source: src.clone(),
            protocol_address: "COIL:1".to_string(),
            data_type: TagDataType::Bool,
            unit: None,
            writable: true,
            deadband: 0.0,
            scale_factor: 1.0,
            scale_offset: 0.0,
            historize: true,
            alarm_config: TagAlarmConfig::default(),
        },
        RegisteredTag {
            id: "MODBUS.GF01.Temp_Eau_Glacee".to_string(),
            label: "Température Départ Eau Glacée (HR:0 Float32)".to_string(),
            module_source: src.clone(),
            protocol_address: "HR:0:FLOAT32".to_string(),
            data_type: TagDataType::Float,
            unit: Some("°C".to_string()),
            writable: false,
            deadband: 0.05,
            scale_factor: 1.0,
            scale_offset: 0.0,
            historize: true,
            alarm_config: TagAlarmConfig {
                enabled: true,
                low: Some(4.0),
                high: Some(12.5),
                high_high: Some(15.0),
                severity: AlarmSeverity::High,
                message: Some("Température Eau Glacée GF01 hors consigne".to_string()),
                ..Default::default()
            },
        },
        RegisteredTag {
            id: "MODBUS.GF01.Consigne_Eau_Glacee".to_string(),
            label: "Consigne Eau Glacée Groupe Froid (HR:2 Float32)".to_string(),
            module_source: src.clone(),
            protocol_address: "HR:2:FLOAT32".to_string(),
            data_type: TagDataType::Float,
            unit: Some("°C".to_string()),
            writable: true,
            deadband: 0.02,
            scale_factor: 1.0,
            scale_offset: 0.0,
            historize: true,
            alarm_config: TagAlarmConfig::default(),
        },
        RegisteredTag {
            id: "MODBUS.GF01.Pression_Condenseur".to_string(),
            label: "Pression Condenseur Fréon R410A (HR:4 Float32)".to_string(),
            module_source: src.clone(),
            protocol_address: "HR:4:FLOAT32".to_string(),
            data_type: TagDataType::Float,
            unit: Some("bar".to_string()),
            writable: false,
            deadband: 0.05,
            scale_factor: 1.0,
            scale_offset: 0.0,
            historize: true,
            alarm_config: TagAlarmConfig {
                enabled: true,
                high: Some(18.0),
                severity: AlarmSeverity::Critical,
                ..Default::default()
            },
        },
        RegisteredTag {
            id: "MODBUS.GF01.Courant_Compresseur".to_string(),
            label: "Intensité Compresseur GF01 (HR:10 Int16)".to_string(),
            module_source: src.clone(),
            protocol_address: "HR:10:INT16".to_string(),
            data_type: TagDataType::Int,
            unit: Some("A".to_string()),
            writable: false,
            deadband: 1.0,
            scale_factor: 1.0,
            scale_offset: 0.0,
            historize: true,
            alarm_config: TagAlarmConfig::default(),
        },
    ];

    DriverConfig {
        id: src,
        name: "Automate Modbus TCP — Groupe Froid Eau Glacée (PLC Local)".to_string(),
        protocol: ProtocolType::ModbusTcp,
        endpoint: format!("127.0.0.1:{}", port),
        unit_id: 1,
        poll_interval_ms: 900,
        enabled: true,
        tags,
        stats: DriverRuntimeStats::default(),
    }
}

/// Lance ou relance dynamiquement la tâche asynchrone d'un driver enregistré dans `ScadaCore`.
pub async fn spawn_driver_task(
    core: Arc<ScadaCore>,
    config: DriverConfig,
) -> Result<(AbortHandle, mpsc::Sender<WriteCommand>), String> {
    let core_tx = core.driver_ingest_sender();

    match config.protocol {
        ProtocolType::Simulator => {
            let mut sim = GtbSimulatorDriver::from_config(config, Arc::clone(&core));
            let writer = sim.writer_sender();
            let handle = sim.start(core_tx).await?;
            Ok((handle, writer))
        }
        ProtocolType::ModbusTcp => {
            let mut mb = ModbusTcpDriver::new(config, Arc::clone(&core));
            let writer = mb.writer_sender();
            let handle = mb.start(core_tx).await?;
            Ok((handle, writer))
        }
        ProtocolType::Mqtt => {
            let mut mq = MqttDriver::new(config, Arc::clone(&core));
            let writer = mq.writer_sender();
            let handle = mq.start(core_tx).await?;
            Ok((handle, writer))
        }
        ProtocolType::SiemensS7 => {
            let mut s7 = s7::S7Driver::new(config, Arc::clone(&core));
            let writer = s7.writer_sender();
            let handle = s7.start(core_tx).await?;
            Ok((handle, writer))
        }
        ProtocolType::OpcUa => {
            let mut ua = opc_ua::OpcUaDriver::new(config, Arc::clone(&core));
            let writer = ua.writer_sender();
            let handle = ua.start(core_tx).await?;
            Ok((handle, writer))
        }
        ProtocolType::BacnetIp => {
            let mut bac = bacnet::BacnetDriver::new(config, Arc::clone(&core));
            let writer = bac.writer_sender();
            let handle = bac.start(core_tx).await?;
            Ok((handle, writer))
        }
    }
}

// ============================================================================
// 1. SIMULATEUR MULTI-SYSTÈMES COMPLET (CVC, CHAUFFERIE, TGBT)
// ============================================================================

pub struct GtbSimulatorDriver {
    config: DriverConfig,
    core: Arc<ScadaCore>,
    internal_values: Arc<DashMap<String, TagValue>>,
    write_rx: Option<mpsc::Receiver<WriteCommand>>,
    write_tx: mpsc::Sender<WriteCommand>,
}

impl GtbSimulatorDriver {
    pub fn new_default(driver_name: &str, core: Arc<ScadaCore>) -> Self {
        let src = driver_name.to_string();
        let tags = vec![
            RegisteredTag {
                id: "GTB.CTA01.Temp_Ext".to_string(),
                label: "Température Extérieure Sonde Nord".to_string(),
                module_source: src.clone(),
                protocol_address: "SIM:CTA01:TEMP_EXT".to_string(),
                data_type: TagDataType::Float,
                unit: Some("°C".to_string()),
                writable: false,
                deadband: 0.05,
                scale_factor: 1.0,
                scale_offset: 0.0,
                historize: true,
                alarm_config: TagAlarmConfig {
                    enabled: true,
                    low: Some(2.0),
                    high: Some(35.0),
                    severity: AlarmSeverity::Advisory,
                    ..Default::default()
                },
            },
            RegisteredTag {
                id: "GTB.CTA01.Temp_Soufflage".to_string(),
                label: "Température Soufflage CTA 01".to_string(),
                module_source: src.clone(),
                protocol_address: "SIM:CTA01:TEMP_SOUFFLAGE".to_string(),
                data_type: TagDataType::Float,
                unit: Some("°C".to_string()),
                writable: false,
                deadband: 0.05,
                scale_factor: 1.0,
                scale_offset: 0.0,
                historize: true,
                alarm_config: TagAlarmConfig {
                    enabled: true,
                    low_low: Some(12.0),
                    low: Some(15.0),
                    high: Some(27.0),
                    high_high: Some(31.0),
                    severity: AlarmSeverity::High,
                    message: Some("Écart thermique soufflage CTA 01".to_string()),
                    ..Default::default()
                },
            },
            RegisteredTag {
                id: "GTB.CTA01.Consigne_Temp".to_string(),
                label: "Consigne Température CTA 01".to_string(),
                module_source: src.clone(),
                protocol_address: "SIM:CTA01:CONSIGNE_TEMP".to_string(),
                data_type: TagDataType::Float,
                unit: Some("°C".to_string()),
                writable: true,
                deadband: 0.01,
                scale_factor: 1.0,
                scale_offset: 0.0,
                historize: true,
                alarm_config: TagAlarmConfig::default(),
            },
            RegisteredTag {
                id: "GTB.CTA01.Vanne_Chaud_Cmd".to_string(),
                label: "Ouverture Vanne 3 Voies Batterie Chaude".to_string(),
                module_source: src.clone(),
                protocol_address: "SIM:CTA01:VANNE_CHAUD".to_string(),
                data_type: TagDataType::Float,
                unit: Some("%".to_string()),
                writable: true,
                deadband: 0.1,
                scale_factor: 1.0,
                scale_offset: 0.0,
                historize: true,
                alarm_config: TagAlarmConfig::default(),
            },
            RegisteredTag {
                id: "GTB.CTA01.Registre_AirNeuf".to_string(),
                label: "Registre Motorisé Air Neuf CTA 01".to_string(),
                module_source: src.clone(),
                protocol_address: "SIM:CTA01:REGISTRE_AN".to_string(),
                data_type: TagDataType::Float,
                unit: Some("%".to_string()),
                writable: true,
                deadband: 0.1,
                scale_factor: 1.0,
                scale_offset: 0.0,
                historize: true,
                alarm_config: TagAlarmConfig::default(),
            },
            RegisteredTag {
                id: "GTB.CTA01.Hygrometrie_Reprise".to_string(),
                label: "Hygrométrie Relative Air Reprise".to_string(),
                module_source: src.clone(),
                protocol_address: "SIM:CTA01:HYGRO_REP".to_string(),
                data_type: TagDataType::Float,
                unit: Some("%HR".to_string()),
                writable: false,
                deadband: 0.2,
                scale_factor: 1.0,
                scale_offset: 0.0,
                historize: true,
                alarm_config: TagAlarmConfig {
                    enabled: true,
                    low: Some(30.0),
                    high: Some(70.0),
                    severity: AlarmSeverity::Medium,
                    ..Default::default()
                },
            },
            RegisteredTag {
                id: "GTB.CTA01.Ventilateur_Marche".to_string(),
                label: "Ventilateur Soufflage CTA 01".to_string(),
                module_source: src.clone(),
                protocol_address: "SIM:CTA01:VENTIL_MARCHE".to_string(),
                data_type: TagDataType::Bool,
                unit: None,
                writable: true,
                deadband: 0.0,
                scale_factor: 1.0,
                scale_offset: 0.0,
                historize: true,
                alarm_config: TagAlarmConfig::default(),
            },
            RegisteredTag {
                id: "GTB.CTA01.Defaut_Filtre".to_string(),
                label: "Pressostat Différentiel Filtre F7".to_string(),
                module_source: src.clone(),
                protocol_address: "SIM:CTA01:DEFAUT_FILTRE".to_string(),
                data_type: TagDataType::Bool,
                unit: None,
                writable: true,
                deadband: 0.0,
                scale_factor: 1.0,
                scale_offset: 0.0,
                historize: true,
                alarm_config: TagAlarmConfig {
                    enabled: true,
                    trigger_on_bool: Some(true),
                    severity: AlarmSeverity::High,
                    message: Some(
                        "Encrassement critique Filtre F7 CTA 01 (ΔP > 250 Pa)".to_string(),
                    ),
                    ..Default::default()
                },
            },
            RegisteredTag {
                id: "IND.CHAUF.Temp_Depart_Chaudiere".to_string(),
                label: "Température Départ Chaudière Gaz".to_string(),
                module_source: src.clone(),
                protocol_address: "SIM:CHAUF:TEMP_DEP".to_string(),
                data_type: TagDataType::Float,
                unit: Some("°C".to_string()),
                writable: true,
                deadband: 0.1,
                scale_factor: 1.0,
                scale_offset: 0.0,
                historize: true,
                alarm_config: TagAlarmConfig {
                    enabled: true,
                    low: Some(50.0),
                    high: Some(85.0),
                    high_high: Some(92.0),
                    severity: AlarmSeverity::Critical,
                    ..Default::default()
                },
            },
            RegisteredTag {
                id: "IND.CHAUF.Pompe_P1_Etat".to_string(),
                label: "Pompe Hydraulique Primaire P1".to_string(),
                module_source: src.clone(),
                protocol_address: "SIM:CHAUF:POMPE_P1".to_string(),
                data_type: TagDataType::Bool,
                unit: None,
                writable: true,
                deadband: 0.0,
                scale_factor: 1.0,
                scale_offset: 0.0,
                historize: true,
                alarm_config: TagAlarmConfig::default(),
            },
            RegisteredTag {
                id: "IND.CHAUF.Pompe_P2_Secours".to_string(),
                label: "Pompe Hydraulique Secours P2".to_string(),
                module_source: src.clone(),
                protocol_address: "SIM:CHAUF:POMPE_P2".to_string(),
                data_type: TagDataType::Bool,
                unit: None,
                writable: true,
                deadband: 0.0,
                scale_factor: 1.0,
                scale_offset: 0.0,
                historize: true,
                alarm_config: TagAlarmConfig::default(),
            },
            RegisteredTag {
                id: "IND.CHAUF.Pression_Reseau".to_string(),
                label: "Pression Collecteur Hydraulique".to_string(),
                module_source: src.clone(),
                protocol_address: "SIM:CHAUF:PRESSION".to_string(),
                data_type: TagDataType::Float,
                unit: Some("bar".to_string()),
                writable: false,
                deadband: 0.02,
                scale_factor: 1.0,
                scale_offset: 0.0,
                historize: true,
                alarm_config: TagAlarmConfig {
                    enabled: true,
                    low_low: Some(1.0),
                    low: Some(1.6),
                    high: Some(4.5),
                    high_high: Some(5.2),
                    severity: AlarmSeverity::Critical,
                    message: Some("Pression collecteur hors plage nominale".to_string()),
                    ..Default::default()
                },
            },
            RegisteredTag {
                id: "IND.CHAUF.Debit_Primaire".to_string(),
                label: "Débit Boucle Primaire Chaufferie".to_string(),
                module_source: src.clone(),
                protocol_address: "SIM:CHAUF:DEBIT".to_string(),
                data_type: TagDataType::Float,
                unit: Some("m³/h".to_string()),
                writable: false,
                deadband: 0.1,
                scale_factor: 1.0,
                scale_offset: 0.0,
                historize: true,
                alarm_config: TagAlarmConfig::default(),
            },
            RegisteredTag {
                id: "IND.CHAUF.Niveau_Bache".to_string(),
                label: "Niveau Bâche Tampon Hydraulique".to_string(),
                module_source: src.clone(),
                protocol_address: "SIM:CHAUF:NIVEAU_BACHE".to_string(),
                data_type: TagDataType::Float,
                unit: Some("%".to_string()),
                writable: false,
                deadband: 0.2,
                scale_factor: 1.0,
                scale_offset: 0.0,
                historize: true,
                alarm_config: TagAlarmConfig {
                    enabled: true,
                    low: Some(25.0),
                    high: Some(92.0),
                    severity: AlarmSeverity::Medium,
                    ..Default::default()
                },
            },
            RegisteredTag {
                id: "NRJ.TGBT.Puissance_Active".to_string(),
                label: "Puissance Électrique Globale TGBT".to_string(),
                module_source: src.clone(),
                protocol_address: "SIM:TGBT:PUISSANCE_KW".to_string(),
                data_type: TagDataType::Float,
                unit: Some("kW".to_string()),
                writable: false,
                deadband: 0.3,
                scale_factor: 1.0,
                scale_offset: 0.0,
                historize: true,
                alarm_config: TagAlarmConfig {
                    enabled: true,
                    high: Some(85.0),
                    high_high: Some(100.0),
                    severity: AlarmSeverity::High,
                    ..Default::default()
                },
            },
            RegisteredTag {
                id: "NRJ.TGBT.Tension_Triphasee".to_string(),
                label: "Tension Composée Réseau U12".to_string(),
                module_source: src.clone(),
                protocol_address: "SIM:TGBT:TENSION_U12".to_string(),
                data_type: TagDataType::Float,
                unit: Some("V".to_string()),
                writable: false,
                deadband: 0.4,
                scale_factor: 1.0,
                scale_offset: 0.0,
                historize: true,
                alarm_config: TagAlarmConfig::default(),
            },
            RegisteredTag {
                id: "NRJ.TGBT.Cos_Phi".to_string(),
                label: "Facteur de Puissance Global Cos φ".to_string(),
                module_source: src.clone(),
                protocol_address: "SIM:TGBT:COS_PHI".to_string(),
                data_type: TagDataType::Float,
                unit: Some("".to_string()),
                writable: false,
                deadband: 0.01,
                scale_factor: 1.0,
                scale_offset: 0.0,
                historize: true,
                alarm_config: TagAlarmConfig::default(),
            },
        ];

        let cfg = DriverConfig {
            id: src,
            name: "Simulateur Physique Multi-Zones (CVC / Chaufferie / TGBT)".to_string(),
            protocol: ProtocolType::Simulator,
            endpoint: "internal://physics-engine".to_string(),
            unit_id: 1,
            poll_interval_ms: 700,
            enabled: true,
            tags,
            stats: DriverRuntimeStats::default(),
        };

        Self::from_config(cfg, core)
    }

    pub fn from_config(config: DriverConfig, core: Arc<ScadaCore>) -> Self {
        let (write_tx, write_rx) = mpsc::channel::<WriteCommand>(256);
        let internal_values = Arc::new(DashMap::new());
        internal_values.insert("GTB.CTA01.Temp_Ext".to_string(), TagValue::Float(13.5));
        internal_values.insert(
            "GTB.CTA01.Temp_Soufflage".to_string(),
            TagValue::Float(20.4),
        );
        internal_values.insert("GTB.CTA01.Consigne_Temp".to_string(), TagValue::Float(21.5));
        internal_values.insert(
            "GTB.CTA01.Vanne_Chaud_Cmd".to_string(),
            TagValue::Float(48.0),
        );
        internal_values.insert(
            "GTB.CTA01.Registre_AirNeuf".to_string(),
            TagValue::Float(65.0),
        );
        internal_values.insert(
            "GTB.CTA01.Hygrometrie_Reprise".to_string(),
            TagValue::Float(51.2),
        );
        internal_values.insert(
            "GTB.CTA01.Ventilateur_Marche".to_string(),
            TagValue::Bool(true),
        );
        internal_values.insert("GTB.CTA01.Defaut_Filtre".to_string(), TagValue::Bool(false));
        internal_values.insert(
            "IND.CHAUF.Temp_Depart_Chaudiere".to_string(),
            TagValue::Float(72.5),
        );
        internal_values.insert("IND.CHAUF.Pompe_P1_Etat".to_string(), TagValue::Bool(true));
        internal_values.insert(
            "IND.CHAUF.Pompe_P2_Secours".to_string(),
            TagValue::Bool(false),
        );
        internal_values.insert(
            "IND.CHAUF.Pression_Reseau".to_string(),
            TagValue::Float(3.2),
        );
        internal_values.insert(
            "IND.CHAUF.Debit_Primaire".to_string(),
            TagValue::Float(18.5),
        );
        internal_values.insert("IND.CHAUF.Niveau_Bache".to_string(), TagValue::Float(75.0));
        internal_values.insert(
            "NRJ.TGBT.Puissance_Active".to_string(),
            TagValue::Float(44.2),
        );
        internal_values.insert(
            "NRJ.TGBT.Tension_Triphasee".to_string(),
            TagValue::Float(401.2),
        );
        internal_values.insert("NRJ.TGBT.Cos_Phi".to_string(), TagValue::Float(0.94));

        // Restaure les dernières valeurs connues (consignes opérateur, états de commande)
        // pour qu'un redémarrage du driver ne les réinitialise pas.
        for tag in &config.tags {
            if let Some(v) = core.last_good_values.get(&tag.id) {
                internal_values.insert(tag.id.clone(), v.value().clone());
            }
        }

        Self {
            config,
            core,
            internal_values,
            write_rx: Some(write_rx),
            write_tx,
        }
    }

    pub fn writer_sender(&self) -> mpsc::Sender<WriteCommand> {
        self.write_tx.clone()
    }

    pub fn config(&self) -> DriverConfig {
        self.config.clone()
    }
}

#[async_trait]
impl IndustrialDriver for GtbSimulatorDriver {
    fn id(&self) -> &str {
        &self.config.id
    }

    async fn start(&mut self, core_tx: mpsc::Sender<TagState>) -> Result<AbortHandle, String> {
        let mut write_rx = self
            .write_rx
            .take()
            .ok_or_else(|| "Simulateur déjà démarré".to_string())?;

        let values = Arc::clone(&self.internal_values);
        let tags_meta = self.config.tags.clone();
        let driver_id = self.config.id.clone();
        let poll_ms = self.config.poll_interval_ms.max(250);
        let core_ref = Arc::clone(&self.core);

        let join_handle = tokio::spawn(async move {
            for tag in &tags_meta {
                if let Some(val) = values.get(&tag.id) {
                    let _ = core_tx
                        .send(TagState {
                            tag_id: tag.id.clone(),
                            value: val.value().clone(),
                            quality: Quality::Good,
                            timestamp: Utc::now(),
                        })
                        .await;
                }
            }

            let mut step: f64 = 0.0;
            let mut ticker = tokio::time::interval(Duration::from_millis(poll_ms));

            loop {
                tokio::select! {
                    Some(cmd) = write_rx.recv() => {
                        values.insert(cmd.tag_id.clone(), cmd.value.clone());
                        let _ = core_tx.send(TagState {
                            tag_id: cmd.tag_id,
                            value: cmd.value,
                            quality: Quality::Good,
                            timestamp: Utc::now(),
                        }).await;
                        core_ref.record_driver_poll(&driver_id, true, 1, None);
                    }
                    _ = ticker.tick() => {
                        if core_ref.is_acquisition_paused() {
                            continue;
                        }
                        step += 0.12;
                        let noise: f64 = {
                            let mut rng = rand::thread_rng();
                            rng.gen_range(-0.08..0.08)
                        };

                        let ventil_on = values.get("GTB.CTA01.Ventilateur_Marche").and_then(|v| v.as_bool()).unwrap_or(true);
                        let p1_on = values.get("IND.CHAUF.Pompe_P1_Etat").and_then(|v| v.as_bool()).unwrap_or(true);
                        let p2_on = values.get("IND.CHAUF.Pompe_P2_Secours").and_then(|v| v.as_bool()).unwrap_or(false);
                        let vanne_pct = values.get("GTB.CTA01.Vanne_Chaud_Cmd").and_then(|v| v.as_f64()).unwrap_or(48.0).clamp(0.0, 100.0);
                        let registre_pct = values.get("GTB.CTA01.Registre_AirNeuf").and_then(|v| v.as_f64()).unwrap_or(65.0).clamp(0.0, 100.0);
                        let temp_chaudiere = values.get("IND.CHAUF.Temp_Depart_Chaudiere").and_then(|v| v.as_f64()).unwrap_or(72.5);
                        let curr_soufflage = values.get("GTB.CTA01.Temp_Soufflage").and_then(|v| v.as_f64()).unwrap_or(20.4);
                        let curr_pression = values.get("IND.CHAUF.Pression_Reseau").and_then(|v| v.as_f64()).unwrap_or(3.2);

                        let temp_ext = round2(13.5 + (step * 0.22).sin() * 2.0 + noise * 0.4);
                        let hygro = round2(51.0 + (step * 0.18).cos() * 4.2 + noise);

                        let active_pumps = (if p1_on { 1.0 } else { 0.0 }) + (if p2_on { 1.0 } else { 0.0 });
                        let target_pression = if active_pumps > 0.0 {
                            2.75 + (active_pumps * 0.48) + noise * 0.12
                        } else {
                            0.40
                        };
                        let new_pression = round2(curr_pression + (target_pression - curr_pression) * 0.4);

                        let debit = if active_pumps > 0.0 {
                            round2((7.5 + (vanne_pct * 0.19)) * active_pumps + noise * 0.3)
                        } else {
                            0.0
                        };

                        let apport_chaud = if active_pumps > 0.0 && ventil_on {
                            (vanne_pct / 100.0) * ((temp_chaudiere - temp_ext) * 0.28)
                        } else {
                            0.0
                        };
                        let air_neuf_influence = (registre_pct / 100.0) * (temp_ext - 19.0) * 0.15;
                        let target_soufflage = temp_ext + apport_chaud + air_neuf_influence;
                        let new_soufflage = round2(curr_soufflage + (target_soufflage - curr_soufflage) * 0.22 + noise * 0.08);

                        let niveau_bache = round2(75.0 + (step * 0.35).cos() * 5.5 + noise);
                        let puissance_kw = round2(
                            19.0
                            + (if ventil_on { 11.5 } else { 0.0 })
                            + (active_pumps * 14.8)
                            + (vanne_pct * 0.12)
                            + noise * 1.1
                        );
                        let tension_u = round2(400.5 + (step * 0.5).sin() * 2.4 + noise * 2.0);
                        let cos_phi = round2(0.93 + (active_pumps * 0.015) + noise * 0.01);

                        let updates = [
                            ("GTB.CTA01.Temp_Ext", TagValue::Float(temp_ext)),
                            ("GTB.CTA01.Temp_Soufflage", TagValue::Float(new_soufflage)),
                            ("GTB.CTA01.Hygrometrie_Reprise", TagValue::Float(hygro)),
                            ("IND.CHAUF.Pression_Reseau", TagValue::Float(new_pression)),
                            ("IND.CHAUF.Debit_Primaire", TagValue::Float(debit)),
                            ("IND.CHAUF.Niveau_Bache", TagValue::Float(niveau_bache)),
                            ("NRJ.TGBT.Puissance_Active", TagValue::Float(puissance_kw)),
                            ("NRJ.TGBT.Tension_Triphasee", TagValue::Float(tension_u)),
                            ("NRJ.TGBT.Cos_Phi", TagValue::Float(cos_phi)),
                        ];

                        for (tag_id, val) in updates {
                            values.insert(tag_id.to_string(), val.clone());
                            if core_tx.send(TagState {
                                tag_id: tag_id.to_string(),
                                value: val,
                                quality: Quality::Good,
                                timestamp: Utc::now(),
                            }).await.is_err() {
                                return;
                            }
                        }

                        core_ref.record_driver_poll(&driver_id, true, 1, None);
                    }
                }
            }
        });

        Ok(join_handle.abort_handle())
    }

    async fn write_tag(&self, address: &str, value: TagValue) -> Result<(), String> {
        self.write_tx
            .send(WriteCommand {
                tag_id: address.to_string(),
                protocol_address: address.to_string(),
                value,
            })
            .await
            .map_err(|e| e.to_string())
    }

    async fn browse_tags(&self) -> Result<Vec<RegisteredTag>, String> {
        Ok(self.config.tags.clone())
    }
}

fn round2(val: f64) -> f64 {
    (val * 100.0).round() / 100.0
}

// ============================================================================
// 2. DRIVER MODBUS TCP COMPLET (AVEC MÉTRIQUES DE LATENCE & TRAMES)
// ============================================================================

#[derive(Debug, Clone, Copy)]
enum ModbusAddressSpec {
    Coil(u16),
    DiscreteInput(u16),
    InputRegInt16(u16),
    InputRegFloat32(u16),
    HoldingInt16(u16),
    HoldingUint16(u16),
    HoldingInt32(u16),
    HoldingFloat32(u16),
    HoldingFloat32Swap(u16),
}

impl ModbusAddressSpec {
    fn parse(raw: &str) -> Result<Self, String> {
        let parts: Vec<&str> = raw.trim().split(':').collect();
        let parse_u16 = |s: &str| -> Result<u16, String> {
            s.parse::<u16>()
                .map_err(|_| format!("Adresse registre invalide dans '{}'", raw))
        };

        match parts.as_slice() {
            ["COIL", a] => Ok(Self::Coil(parse_u16(a)?)),
            ["DI", a] => Ok(Self::DiscreteInput(parse_u16(a)?)),
            ["IR", a] | ["IR", a, "INT16"] => Ok(Self::InputRegInt16(parse_u16(a)?)),
            ["IR", a, "FLOAT32"] => Ok(Self::InputRegFloat32(parse_u16(a)?)),
            ["HR", a] | ["HR", a, "INT16"] => Ok(Self::HoldingInt16(parse_u16(a)?)),
            ["HR", a, "UINT16"] => Ok(Self::HoldingUint16(parse_u16(a)?)),
            ["HR", a, "INT32"] => Ok(Self::HoldingInt32(parse_u16(a)?)),
            ["HR", a, "FLOAT32"] => Ok(Self::HoldingFloat32(parse_u16(a)?)),
            ["HR", a, "FLOAT32_SWAP"] => Ok(Self::HoldingFloat32Swap(parse_u16(a)?)),
            _ => Err(format!(
                "Format d'adresse Modbus invalide '{}' (attendu COIL:0, HR:0:FLOAT32, HR:10:INT16)",
                raw
            )),
        }
    }
}

pub fn decode_registers_f32_be(regs: &[u16], word_swap: bool) -> Result<f64, String> {
    if regs.len() < 2 {
        return Err("2 registres 16 bits requis pour Float32".to_string());
    }
    let mut buf = [0u8; 4];
    if word_swap {
        BigEndian::write_u16(&mut buf[0..2], regs[1]);
        BigEndian::write_u16(&mut buf[2..4], regs[0]);
    } else {
        BigEndian::write_u16(&mut buf[0..2], regs[0]);
        BigEndian::write_u16(&mut buf[2..4], regs[1]);
    }
    let f = BigEndian::read_f32(&buf);
    if !f.is_finite() {
        return Err("valeur Float32 non finie (NaN ou Infini)".to_string());
    }
    Ok(f as f64)
}

pub fn encode_f32_to_registers_be(val: f32, word_swap: bool) -> [u16; 2] {
    let mut buf = [0u8; 4];
    BigEndian::write_f32(&mut buf, val);
    let w0 = BigEndian::read_u16(&buf[0..2]);
    let w1 = BigEndian::read_u16(&buf[2..4]);
    if word_swap {
        [w1, w0]
    } else {
        [w0, w1]
    }
}

pub struct ModbusTcpDriver {
    config: DriverConfig,
    core: Arc<ScadaCore>,
    write_rx: Option<mpsc::Receiver<WriteCommand>>,
    write_tx: mpsc::Sender<WriteCommand>,
}

impl ModbusTcpDriver {
    pub fn new(config: DriverConfig, core: Arc<ScadaCore>) -> Self {
        let (write_tx, write_rx) = mpsc::channel::<WriteCommand>(128);
        Self {
            config,
            core,
            write_rx: Some(write_rx),
            write_tx,
        }
    }

    pub fn writer_sender(&self) -> mpsc::Sender<WriteCommand> {
        self.write_tx.clone()
    }
}

#[async_trait]
impl IndustrialDriver for ModbusTcpDriver {
    fn id(&self) -> &str {
        &self.config.id
    }

    async fn start(&mut self, core_tx: mpsc::Sender<TagState>) -> Result<AbortHandle, String> {
        let mut write_rx = self
            .write_rx
            .take()
            .ok_or_else(|| format!("Driver Modbus '{}' déjà démarré", self.config.id))?;

        let socket_addr: SocketAddr = self.config.endpoint.parse().map_err(|e| {
            format!(
                "Endpoint Modbus TCP invalide '{}' : {}",
                self.config.endpoint, e
            )
        })?;

        let config = self.config.clone();
        let core_ref = Arc::clone(&self.core);

        let join_handle = tokio::spawn(async move {
            info!(
                "Démarrage du driver Modbus TCP '{}' vers {} (Slave {})",
                config.id, socket_addr, config.unit_id
            );
            let poll_duration = Duration::from_millis(config.poll_interval_ms.max(250));
            // Adresses invalides ou refusées déjà signalées (un seul avertissement par adresse)
            let mut warned: std::collections::HashSet<String> = Default::default();

            loop {
                let connect_res = tokio::time::timeout(
                    Duration::from_secs(3),
                    tcp::connect_slave(socket_addr, Slave(config.unit_id)),
                )
                .await;

                // La liaison n'est déclarée établie qu'à la première réponse de l'équipement : une
                // passerelle ou un proxy peut accepter la connexion TCP sans que l'automate réponde.
                let mut ctx = match connect_res {
                    Ok(Ok(c)) => c,
                    Ok(Err(e)) => {
                        let msg = format!("Connexion refusée : {}", e);
                        core_ref.record_driver_poll(&config.id, false, 0, Some(msg));
                        publish_bad_disconnect_for_all(
                            &core_ref.get_tags_for_driver(&config.id),
                            &core_tx,
                        )
                        .await;
                        tokio::time::sleep(Duration::from_secs(4)).await;
                        continue;
                    }
                    Err(_) => {
                        core_ref.record_driver_poll(
                            &config.id,
                            false,
                            3000,
                            Some("Timeout TCP 3000ms".to_string()),
                        );
                        publish_bad_disconnect_for_all(
                            &core_ref.get_tags_for_driver(&config.id),
                            &core_tx,
                        )
                        .await;
                        tokio::time::sleep(Duration::from_secs(4)).await;
                        continue;
                    }
                };

                // Les commandes émises pendant une coupure sont obsolètes : elles ne sont pas rejouées
                let mut stale = 0usize;
                while write_rx.try_recv().is_ok() {
                    stale += 1;
                }
                if stale > 0 {
                    warn!(
                        "Driver Modbus '{}' : {} commande(s) obsolète(s) ignorée(s) à la reconnexion",
                        config.id, stale
                    );
                }

                let mut ticker = tokio::time::interval(poll_duration);
                // Après une attente (équipement lent), pas de rafale de lectures en retard
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

                loop {
                    tokio::select! {
                        Some(cmd) = write_rx.recv() => {
                            let start_w = Instant::now();
                            match execute_modbus_write(&mut ctx, &cmd).await {
                                Ok(Ok(())) => {
                                    let lat = start_w.elapsed().as_millis() as u64;
                                    core_ref.record_driver_poll(&config.id, true, lat, None);
                                    core_ref.record_driver_write_accepted(&config.id);
                                }
                                // Commande refusée (exception Modbus, adresse ou valeur invalide) :
                                // la liaison reste saine, ni coupure ni alarme de communication.
                                Ok(Err(err)) => {
                                    warn!(
                                        "Driver Modbus '{}' : écriture de '{}' ({}) refusée : {}",
                                        config.id, cmd.tag_id, cmd.protocol_address, err
                                    );
                                    core_ref.record_driver_write_rejected(&config.id, &cmd.tag_id, &err);
                                }
                                // Défaut de transport : reconnexion
                                Err(err) => {
                                    core_ref.record_driver_poll(&config.id, false, 0, Some(err));
                                    publish_bad_disconnect_for_all(&core_ref.get_tags_for_driver(&config.id), &core_tx).await;
                                    break;
                                }
                            }
                        }
                        _ = ticker.tick() => {
                            if core_ref.is_acquisition_paused() {
                                continue;
                            }
                            let start_r = Instant::now();
                            let mut broken = false;

                            // Récupération dynamique de la liste des tags du driver depuis ScadaCore,
                            // puis lecture par blocs de registres contigus (une trame par bloc).
                            let current_tags = core_ref.get_tags_for_driver(&config.id);
                            match poll_modbus_tags(&mut ctx, &current_tags, &mut warned).await {
                                Ok(values) => {
                                    for (tag_id, value, quality) in values {
                                        let _ = core_tx.send(TagState {
                                            tag_id,
                                            value,
                                            quality,
                                            timestamp: Utc::now(),
                                        }).await;
                                    }
                                    let lat = start_r.elapsed().as_millis() as u64;
                                    core_ref.record_driver_poll(&config.id, true, lat, None);
                                }
                                Err(err) => {
                                    core_ref.record_driver_poll(&config.id, false, 0, Some(err));
                                    broken = true;
                                }
                            }
                            if broken {
                                publish_bad_disconnect_for_all(&current_tags, &core_tx).await;
                                break;
                            }
                        }
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });

        Ok(join_handle.abort_handle())
    }

    async fn write_tag(&self, address: &str, value: TagValue) -> Result<(), String> {
        self.write_tx
            .send(WriteCommand {
                tag_id: address.to_string(),
                protocol_address: address.to_string(),
                value,
            })
            .await
            .map_err(|e| e.to_string())
    }

    async fn browse_tags(&self) -> Result<Vec<RegisteredTag>, String> {
        Ok(self.config.tags.clone())
    }
}

/// Zone mémoire Modbus (une requête de lecture ne couvre qu'une seule zone).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum RegisterArea {
    Coils,
    DiscreteInputs,
    InputRegisters,
    HoldingRegisters,
}

impl RegisterArea {
    /// Quantité maximale par requête (spécification Modbus : 2000 bits, 125 registres).
    fn max_count(self) -> u32 {
        match self {
            RegisterArea::Coils | RegisterArea::DiscreteInputs => 2000,
            RegisterArea::InputRegisters | RegisterArea::HoldingRegisters => 125,
        }
    }
}

impl ModbusAddressSpec {
    fn area(&self) -> RegisterArea {
        match self {
            Self::Coil(_) => RegisterArea::Coils,
            Self::DiscreteInput(_) => RegisterArea::DiscreteInputs,
            Self::InputRegInt16(_) | Self::InputRegFloat32(_) => RegisterArea::InputRegisters,
            Self::HoldingInt16(_)
            | Self::HoldingUint16(_)
            | Self::HoldingInt32(_)
            | Self::HoldingFloat32(_)
            | Self::HoldingFloat32Swap(_) => RegisterArea::HoldingRegisters,
        }
    }

    fn address(&self) -> u16 {
        match *self {
            Self::Coil(a)
            | Self::DiscreteInput(a)
            | Self::InputRegInt16(a)
            | Self::InputRegFloat32(a)
            | Self::HoldingInt16(a)
            | Self::HoldingUint16(a)
            | Self::HoldingInt32(a)
            | Self::HoldingFloat32(a)
            | Self::HoldingFloat32Swap(a) => a,
        }
    }

    /// Nombre de bits ou de registres occupés.
    fn width(&self) -> u32 {
        match self {
            Self::InputRegFloat32(_)
            | Self::HoldingInt32(_)
            | Self::HoldingFloat32(_)
            | Self::HoldingFloat32Swap(_) => 2,
            _ => 1,
        }
    }
}

/// Écart maximal (en bits ou registres) comblé pour fusionner deux lectures : lire quelques
/// registres inutiles coûte moins cher qu'un aller-retour réseau supplémentaire.
const MODBUS_MAX_GAP: u32 = 8;

/// Bloc de lecture : une requête Modbus couvrant plusieurs tags.
#[derive(Debug, Clone, PartialEq)]
struct ReadBlock {
    area: RegisterArea,
    start: u16,
    count: u16,
    /// Index des adresses (dans la liste fournie au planificateur) couvertes par le bloc.
    members: Vec<usize>,
}

/// Regroupe les adresses en blocs contigus par zone, dans la limite de taille Modbus.
fn plan_read_blocks(specs: &[ModbusAddressSpec]) -> Vec<ReadBlock> {
    let mut order: Vec<usize> = (0..specs.len()).collect();
    order.sort_by_key(|&i| (specs[i].area(), specs[i].address()));

    let mut blocks: Vec<ReadBlock> = Vec::new();
    for i in order {
        let spec = specs[i];
        let start = spec.address() as u32;
        let end = start + spec.width();
        if let Some(b) = blocks.last_mut() {
            let b_start = b.start as u32;
            let b_end = b_start + b.count as u32;
            let new_end = b_end.max(end);
            if b.area == spec.area()
                && start <= b_end + MODBUS_MAX_GAP
                && new_end - b_start <= spec.area().max_count()
                && new_end <= 65_536
            {
                b.count = (new_end - b_start) as u16;
                b.members.push(i);
                continue;
            }
        }
        blocks.push(ReadBlock {
            area: spec.area(),
            start: spec.address(),
            count: spec.width().min(65_536 - start) as u16,
            members: vec![i],
        });
    }
    blocks
}

enum BlockData {
    Bits(Vec<bool>),
    Registers(Vec<u16>),
}

/// Délai de réponse d'une requête Modbus. Un équipement qui ne répond plus sans fermer la
/// connexion (câble coupé, automate figé) est ainsi détecté : sans ce délai, la lecture
/// attendrait indéfiniment et les dernières valeurs resteraient affichées en qualité Good.
const MODBUS_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

fn no_reply_error() -> String {
    format!(
        "Pas de réponse de l'équipement en {} s",
        MODBUS_REQUEST_TIMEOUT.as_secs()
    )
}

/// Lit un bloc. Erreur externe : défaut de transport (connexion perdue).
/// Erreur interne : exception Modbus renvoyée par l'équipement (la liaison reste valide).
async fn read_block(
    ctx: &mut client::Context,
    block: &ReadBlock,
) -> Result<Result<BlockData, String>, String> {
    let (start, count) = (block.start, block.count);
    let request = async {
        match block.area {
            RegisterArea::Coils => ctx
                .read_coils(start, count)
                .await
                .map(|r| r.map(BlockData::Bits)),
            RegisterArea::DiscreteInputs => ctx
                .read_discrete_inputs(start, count)
                .await
                .map(|r| r.map(BlockData::Bits)),
            RegisterArea::InputRegisters => ctx
                .read_input_registers(start, count)
                .await
                .map(|r| r.map(BlockData::Registers)),
            RegisterArea::HoldingRegisters => ctx
                .read_holding_registers(start, count)
                .await
                .map(|r| r.map(BlockData::Registers)),
        }
    };
    let res = tokio::time::timeout(MODBUS_REQUEST_TIMEOUT, request)
        .await
        .map_err(|_| no_reply_error())?;
    match res {
        Err(e) => Err(e.to_string()),
        Ok(Err(exc)) => Ok(Err(format!("exception Modbus {:?}", exc))),
        Ok(Ok(data)) => Ok(Ok(data)),
    }
}

/// Décode la valeur d'une adresse à partir des données de son bloc.
fn decode_from_block(
    spec: &ModbusAddressSpec,
    block_start: u16,
    data: &BlockData,
) -> Result<TagValue, String> {
    let offset = (spec.address() - block_start) as usize;
    let missing = || {
        format!(
            "Réponse Modbus trop courte pour l'adresse {}",
            spec.address()
        )
    };
    match data {
        BlockData::Bits(bits) => bits
            .get(offset)
            .copied()
            .map(TagValue::Bool)
            .ok_or_else(missing),
        BlockData::Registers(regs) => {
            let one = || regs.get(offset).copied().ok_or_else(missing);
            let two = || regs.get(offset..offset + 2).ok_or_else(missing);
            match spec {
                ModbusAddressSpec::InputRegInt16(_) | ModbusAddressSpec::HoldingInt16(_) => {
                    Ok(TagValue::Int(one()? as i16 as i64))
                }
                ModbusAddressSpec::HoldingUint16(_) => Ok(TagValue::Int(one()? as i64)),
                ModbusAddressSpec::HoldingInt32(_) => {
                    let r = two()?;
                    let mut buf = [0u8; 4];
                    BigEndian::write_u16(&mut buf[0..2], r[0]);
                    BigEndian::write_u16(&mut buf[2..4], r[1]);
                    Ok(TagValue::Int(BigEndian::read_i32(&buf) as i64))
                }
                ModbusAddressSpec::InputRegFloat32(_) | ModbusAddressSpec::HoldingFloat32(_) => {
                    Ok(TagValue::Float(decode_registers_f32_be(two()?, false)?))
                }
                ModbusAddressSpec::HoldingFloat32Swap(_) => {
                    Ok(TagValue::Float(decode_registers_f32_be(two()?, true)?))
                }
                ModbusAddressSpec::Coil(_) | ModbusAddressSpec::DiscreteInput(_) => {
                    Err("Adresse binaire lue dans une zone de registres".to_string())
                }
            }
        }
    }
}

/// Scrute tous les tags d'un driver Modbus par blocs.
///
/// Renvoie `(tag, valeur, qualité)` pour chaque tag. Une adresse invalide ou refusée par
/// l'équipement (exception) ne marque que ses propres tags en défaut ; seule une erreur de
/// transport fait échouer la scrutation entière (et déclenche la reconnexion).
async fn poll_modbus_tags(
    ctx: &mut client::Context,
    tags: &[RegisteredTag],
    warned: &mut std::collections::HashSet<String>,
) -> Result<Vec<(String, TagValue, Quality)>, String> {
    let mut out = Vec::with_capacity(tags.len());
    let mut readable: Vec<&RegisteredTag> = Vec::with_capacity(tags.len());
    let mut specs: Vec<ModbusAddressSpec> = Vec::with_capacity(tags.len());

    for tag in tags {
        match ModbusAddressSpec::parse(&tag.protocol_address) {
            Ok(spec) => {
                readable.push(tag);
                specs.push(spec);
            }
            Err(e) => {
                if warned.insert(format!("{}|{}", tag.id, tag.protocol_address)) {
                    warn!("Tag '{}' ignoré : {}", tag.id, e);
                }
                out.push((
                    tag.id.clone(),
                    fallback_value(tag.data_type),
                    Quality::BadDisconnect,
                ));
            }
        }
    }

    for block in plan_read_blocks(&specs) {
        match read_block(ctx, &block).await? {
            Ok(data) => {
                for &i in &block.members {
                    let tag = readable[i];
                    match decode_from_block(&specs[i], block.start, &data) {
                        Ok(v) => {
                            let v = v.coerce_to(tag.data_type).unwrap_or(v);
                            out.push((tag.id.clone(), v, Quality::Good));
                        }
                        Err(_) => out.push((
                            tag.id.clone(),
                            fallback_value(tag.data_type),
                            Quality::BadDisconnect,
                        )),
                    }
                }
            }
            Err(exc) => {
                if block.members.len() > 1 {
                    for &i in &block.members {
                        let tag = readable[i];
                        let spec = specs[i];
                        let single_block = ReadBlock {
                            area: spec.area(),
                            start: spec.address(),
                            count: spec.width().min(65_536 - spec.address() as u32) as u16,
                            members: vec![i],
                        };
                        match read_block(ctx, &single_block).await? {
                            Ok(data) => match decode_from_block(&spec, single_block.start, &data) {
                                Ok(v) => {
                                    let v = v.coerce_to(tag.data_type).unwrap_or(v);
                                    out.push((tag.id.clone(), v, Quality::Good));
                                }
                                Err(_) => out.push((
                                    tag.id.clone(),
                                    fallback_value(tag.data_type),
                                    Quality::BadDisconnect,
                                )),
                            },
                            Err(single_exc) => {
                                let key = format!(
                                    "{:?}:{}:{}",
                                    single_block.area, single_block.start, single_block.count
                                );
                                if warned.insert(key) {
                                    warn!(
                                        "Lecture Modbus unitaire {:?} {}..{} refusée par l'équipement : {}",
                                        single_block.area,
                                        single_block.start,
                                        single_block.start as u32 + single_block.count as u32,
                                        single_exc
                                    );
                                }
                                out.push((
                                    tag.id.clone(),
                                    fallback_value(tag.data_type),
                                    Quality::BadDisconnect,
                                ));
                            }
                        }
                    }
                } else {
                    let key = format!("{:?}:{}:{}", block.area, block.start, block.count);
                    if warned.insert(key) {
                        warn!(
                            "Lecture Modbus {:?} {}..{} refusée par l'équipement : {}",
                            block.area,
                            block.start,
                            block.start as u32 + block.count as u32,
                            exc
                        );
                    }
                    for &i in &block.members {
                        let tag = readable[i];
                        out.push((
                            tag.id.clone(),
                            fallback_value(tag.data_type),
                            Quality::BadDisconnect,
                        ));
                    }
                }
            }
        }
    }
    Ok(out)
}

fn fallback_value(data_type: TagDataType) -> TagValue {
    match data_type {
        TagDataType::Bool => TagValue::Bool(false),
        TagDataType::Float => TagValue::Float(0.0),
        TagDataType::Int => TagValue::Int(0),
        TagDataType::Text => TagValue::Text("DISCONNECTED".to_string()),
    }
}

impl ModbusAddressSpec {
    /// Zones écrivables : bobines (COIL) et registres de maintien (HR).
    /// Les entrées TOR (DI) et les registres d'entrée (IR) sont en lecture seule.
    fn is_writable(&self) -> bool {
        matches!(
            self,
            Self::Coil(_)
                | Self::HoldingInt16(_)
                | Self::HoldingUint16(_)
                | Self::HoldingInt32(_)
                | Self::HoldingFloat32(_)
                | Self::HoldingFloat32Swap(_)
        )
    }
}

/// Trame d'écriture Modbus prête à l'envoi (adresse et données déjà contrôlées).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModbusWrite {
    /// Écriture d'une bobine (fonction 05).
    Coil(u16, bool),
    /// Écriture d'un registre (fonction 06).
    Register(u16, u16),
    /// Écriture de deux registres consécutifs, mot de poids fort en premier (fonction 16).
    Registers(u16, [u16; 2]),
}

/// Valeur entière arrondie, refusée hors de `[min, max]` : jamais de saturation silencieuse,
/// l'équipement reçoit exactement la valeur demandée ou rien.
fn integer_in_range(value: &TagValue, min: i64, max: i64, type_name: &str) -> Result<i64, String> {
    let n = value
        .as_f64()
        .filter(|n| n.is_finite())
        .ok_or_else(|| format!("Valeur numérique requise pour un registre {}", type_name))?;
    let r = n.round();
    if r < min as f64 || r > max as f64 {
        return Err(format!(
            "Valeur {} hors plage pour un registre {} ({}..={})",
            n, type_name, min, max
        ));
    }
    Ok(r as i64)
}

/// Convertit une valeur (brute équipement) en trame d'écriture pour une adresse donnée.
fn encode_modbus_write(spec: ModbusAddressSpec, value: &TagValue) -> Result<ModbusWrite, String> {
    match spec {
        ModbusAddressSpec::Coil(addr) => {
            let b = value
                .as_bool()
                .ok_or("Valeur booléenne requise pour une bobine (COIL)")?;
            Ok(ModbusWrite::Coil(addr, b))
        }
        ModbusAddressSpec::HoldingInt16(addr) => {
            let n = integer_in_range(value, i16::MIN as i64, i16::MAX as i64, "INT16")?;
            Ok(ModbusWrite::Register(addr, (n as i16) as u16))
        }
        ModbusAddressSpec::HoldingUint16(addr) => {
            let n = integer_in_range(value, 0, u16::MAX as i64, "UINT16")?;
            Ok(ModbusWrite::Register(addr, n as u16))
        }
        ModbusAddressSpec::HoldingInt32(addr) => {
            let n = integer_in_range(value, i32::MIN as i64, i32::MAX as i64, "INT32")?;
            let mut buf = [0u8; 4];
            BigEndian::write_i32(&mut buf, n as i32);
            Ok(ModbusWrite::Registers(
                addr,
                [
                    BigEndian::read_u16(&buf[0..2]),
                    BigEndian::read_u16(&buf[2..4]),
                ],
            ))
        }
        ModbusAddressSpec::HoldingFloat32(addr) | ModbusAddressSpec::HoldingFloat32Swap(addr) => {
            let n = value
                .as_f64()
                .filter(|n| n.is_finite())
                .ok_or("Valeur numérique requise pour un registre FLOAT32")?;
            if n.abs() > f32::MAX as f64 {
                return Err(format!("Valeur {} hors plage pour un registre FLOAT32", n));
            }
            let swap = matches!(spec, ModbusAddressSpec::HoldingFloat32Swap(_));
            Ok(ModbusWrite::Registers(
                addr,
                encode_f32_to_registers_be(n as f32, swap),
            ))
        }
        ModbusAddressSpec::DiscreteInput(_)
        | ModbusAddressSpec::InputRegInt16(_)
        | ModbusAddressSpec::InputRegFloat32(_) => Err(format!(
            "Adresse Modbus en lecture seule ({:?}) : seules les bobines (COIL) et les registres de maintien (HR) sont écrivables",
            spec.area()
        )),
    }
}

/// Contrôle, avant l'envoi, qu'une valeur brute peut être écrite à une adresse Modbus
/// (adresse écrivable, valeur dans la plage du type). Appelé par `ScadaCore::write_tag`
/// pour renvoyer l'erreur à l'opérateur ou au flux au lieu d'une erreur différée.
pub fn check_modbus_write(protocol_address: &str, value: &TagValue) -> Result<(), String> {
    let spec = ModbusAddressSpec::parse(protocol_address)?;
    encode_modbus_write(spec, value).map(|_| ())
}

/// Contrôle à l'enregistrement qu'une variable Modbus déclarée inscriptible vise une zone
/// écrivable (COIL ou HR) avec un type numérique ou booléen.
pub fn validate_modbus_tag(tag: &RegisteredTag) -> Result<(), String> {
    if !tag.writable {
        return Ok(());
    }
    let spec = ModbusAddressSpec::parse(&tag.protocol_address)
        .map_err(|e| format!("Variable inscriptible '{}' : {}", tag.id, e))?;
    if !spec.is_writable() {
        return Err(format!(
            "Variable '{}' : l'adresse '{}' est en lecture seule (entrées TOR « DI » et registres d'entrée « IR ») ; elle ne peut pas être déclarée inscriptible",
            tag.id, tag.protocol_address
        ));
    }
    if tag.data_type == TagDataType::Text {
        return Err(format!(
            "Variable '{}' : une variable texte ne peut pas être écrite dans un registre Modbus",
            tag.id
        ));
    }
    Ok(())
}

/// Exécute une écriture. Erreur externe : défaut de transport (connexion perdue).
/// Erreur interne : commande refusée (adresse ou valeur invalide, exception Modbus renvoyée par
/// l'équipement) : la liaison reste valide.
async fn execute_modbus_write(
    ctx: &mut client::Context,
    cmd: &WriteCommand,
) -> Result<Result<(), String>, String> {
    let frame = match ModbusAddressSpec::parse(&cmd.protocol_address)
        .and_then(|spec| encode_modbus_write(spec, &cmd.value))
    {
        Ok(frame) => frame,
        Err(e) => return Ok(Err(e)),
    };
    let request = async {
        match frame {
            ModbusWrite::Coil(addr, b) => ctx.write_single_coil(addr, b).await,
            ModbusWrite::Register(addr, word) => ctx.write_single_register(addr, word).await,
            ModbusWrite::Registers(addr, words) => ctx.write_multiple_registers(addr, &words).await,
        }
    };
    let res = tokio::time::timeout(MODBUS_REQUEST_TIMEOUT, request)
        .await
        .map_err(|_| no_reply_error())?;
    match res {
        Err(e) => Err(e.to_string()),
        Ok(Err(exc)) => Ok(Err(format!("exception Modbus {:?}", exc))),
        Ok(Ok(())) => Ok(Ok(())),
    }
}

async fn publish_bad_disconnect_for_all(tags: &[RegisteredTag], core_tx: &mpsc::Sender<TagState>) {
    for tag in tags {
        let fallback_val = fallback_value(tag.data_type);
        let _ = core_tx
            .send(TagState {
                tag_id: tag.id.clone(),
                value: fallback_val,
                quality: Quality::BadDisconnect,
                timestamp: Utc::now(),
            })
            .await;
    }
}

// ============================================================================
// 3. DRIVER MQTT INDUSTRIEL COMPLET
// ============================================================================

/// Adresse MQTT « sujet » ou « sujet#clé » (clé d'un objet JSON publié sur le sujet).
///
/// Le `#` d'un filtre multi-niveaux (« bat/# », « # ») fait partie du sujet : ce n'est une clé
/// que s'il suit un caractère autre que `/` et qu'un nom le suit.
fn mqtt_split_address(address: &str) -> (&str, Option<&str>) {
    match address.split_once('#') {
        Some((topic, key)) if !topic.is_empty() && !topic.ends_with('/') && !key.is_empty() => {
            (topic, Some(key))
        }
        _ => (address, None),
    }
}

fn mqtt_topic(address: &str) -> &str {
    mqtt_split_address(address).0
}

/// Correspondance d'un sujet publié avec un filtre d'abonnement (`+` : un niveau, `#` : tous
/// les niveaux suivants), selon MQTT 3.1.1 §4.7.
fn mqtt_topic_matches(filter: &str, topic: &str) -> bool {
    let mut f = filter.split('/');
    let mut t = topic.split('/');
    loop {
        match (f.next(), t.next()) {
            (Some("#"), _) => return true,
            (Some("+"), Some(_)) => {}
            (Some(a), Some(b)) if a == b => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

/// Encode la charge utile d'écriture pour un équipement MQTT.
/// Si l'adresse cible comporte une clé JSON (`sujet#clé`), la valeur est encapsulée dans un objet JSON `{"clé": valeur}`.
/// Sinon, la valeur scalaire ou textuelle est directement sérialisée en JSON.
fn encode_mqtt_write_payload(
    protocol_address: &str,
    value: &TagValue,
) -> Result<Vec<u8>, serde_json::Error> {
    let (_topic, json_key) = mqtt_split_address(protocol_address);
    if let Some(key) = json_key {
        let mut map = serde_json::Map::new();
        map.insert(key.to_string(), serde_json::to_value(value)?);
        serde_json::to_vec(&serde_json::Value::Object(map))
    } else {
        serde_json::to_vec(value)
    }
}

/// Extrait la valeur d'une variable depuis la charge utile d'un message MQTT reçu.
/// Si l'adresse spécifie une clé JSON (`sujet#clé`), la clé doit être présente dans l'objet JSON.
/// Si la clé est absente, aucun repli vers `Text` n'est effectué pour ne pas écraser les autres variables du sujet.
fn extract_mqtt_tag_value(protocol_address: &str, payload_str: &str) -> Option<TagValue> {
    let (_topic, json_key) = mqtt_split_address(protocol_address);
    if let Some(key) = json_key {
        let v = serde_json::from_str::<serde_json::Value>(payload_str).ok()?;
        let field = v.get(key)?.clone();
        serde_json::from_value::<TagValue>(field).ok()
    } else {
        serde_json::from_str::<TagValue>(payload_str)
            .ok()
            .or_else(|| Some(TagValue::Text(payload_str.to_string())))
    }
}

pub struct MqttDriver {
    config: DriverConfig,
    core: Arc<ScadaCore>,
    write_rx: Option<mpsc::Receiver<WriteCommand>>,
    write_tx: mpsc::Sender<WriteCommand>,
}

impl MqttDriver {
    pub fn new(config: DriverConfig, core: Arc<ScadaCore>) -> Self {
        let (write_tx, write_rx) = mpsc::channel::<WriteCommand>(128);
        Self {
            config,
            core,
            write_rx: Some(write_rx),
            write_tx,
        }
    }

    pub fn writer_sender(&self) -> mpsc::Sender<WriteCommand> {
        self.write_tx.clone()
    }

    fn parse_broker_endpoint(&self) -> (String, u16) {
        let cleaned = self
            .config
            .endpoint
            .trim_start_matches("mqtt://")
            .trim_start_matches("tcp://");
        if let Some((h, p_str)) = cleaned.rsplit_once(':') {
            if let Ok(port) = p_str.parse::<u16>() {
                return (h.to_string(), port);
            }
        }
        (cleaned.to_string(), 1883)
    }
}

#[async_trait]
impl IndustrialDriver for MqttDriver {
    fn id(&self) -> &str {
        &self.config.id
    }

    async fn start(&mut self, core_tx: mpsc::Sender<TagState>) -> Result<AbortHandle, String> {
        let mut write_rx = self
            .write_rx
            .take()
            .ok_or_else(|| "Driver MQTT déjà démarré".to_string())?;

        let (host, port) = self.parse_broker_endpoint();
        // Identifiant client vu par le broker (ACL éventuelles) : « apex-scada-<id du driver> »
        let client_id = format!("apex-scada-{}", self.config.id);

        let mut mqttoptions = MqttOptions::new(client_id, host.clone(), port);
        mqttoptions.set_keep_alive(Duration::from_secs(15));

        let (client, mut eventloop) = AsyncClient::new(mqttoptions, 64);
        let config = self.config.clone();
        let core_ref = Arc::clone(&self.core);

        let join_handle = tokio::spawn(async move {
            // Sujets abonnés sur la connexion courante ; les variables ajoutées ensuite sont
            // abonnées au tour suivant de `resync` (sans attendre une reconnexion).
            let mut subscribed: std::collections::HashSet<String> = Default::default();
            let mut connected = false;
            let mut resync = tokio::time::interval(Duration::from_secs(2));
            loop {
                tokio::select! {
                    Some(cmd) = write_rx.recv() => {
                        let topic = mqtt_topic(&cmd.protocol_address).to_string();
                        if topic.contains(['+', '#']) {
                            core_ref.record_driver_write_rejected(
                                &config.id,
                                &cmd.tag_id,
                                "sujet à jokers : publication impossible",
                            );
                            continue;
                        }
                        match encode_mqtt_write_payload(&cmd.protocol_address, &cmd.value) {
                            Ok(payload) => match client.publish(topic, QoS::AtLeastOnce, false, payload).await {
                                Ok(()) => core_ref.record_driver_write_accepted(&config.id),
                                Err(e) => core_ref.record_driver_write_rejected(&config.id, &cmd.tag_id, &e.to_string()),
                            },
                            Err(e) => core_ref.record_driver_write_rejected(&config.id, &cmd.tag_id, &e.to_string()),
                        }
                    }
                    _ = resync.tick(), if connected => {
                        let mut to_sub = Vec::new();
                        for tag in core_ref.get_tags_for_driver(&config.id) {
                            let topic = mqtt_topic(&tag.protocol_address).to_string();
                            if !subscribed.contains(&topic) {
                                subscribed.insert(topic.clone());
                                to_sub.push(topic);
                            }
                        }
                        if !to_sub.is_empty() {
                            let client_c = client.clone();
                            tokio::spawn(async move {
                                for t in to_sub {
                                    let _ = client_c.subscribe(t, QoS::AtLeastOnce).await;
                                }
                            });
                        }
                    }
                    ev = eventloop.poll() => {
                        match ev {
                            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                                core_ref.record_driver_poll(&config.id, true, 5, None);
                                connected = true;
                                let mut stale = 0usize;
                                while write_rx.try_recv().is_ok() {
                                    stale += 1;
                                }
                                if stale > 0 {
                                    warn!(
                                        "Driver MQTT '{}' : {} commande(s) d'écriture en attente purgée(s) à la reconnexion",
                                        config.id, stale
                                    );
                                }
                                // Nouvelle session : tous les abonnements sont à refaire
                                subscribed.clear();
                                resync.reset_immediately();
                            }
                            Ok(Event::Incoming(Packet::Publish(publish))) => {
                                core_ref.record_driver_poll(&config.id, true, 3, None);
                                if core_ref.is_acquisition_paused() {
                                    continue;
                                }
                                let payload_str = String::from_utf8_lossy(&publish.payload);
                                for tag in core_ref.get_tags_for_driver(&config.id) {
                                    let (topic, _) = mqtt_split_address(&tag.protocol_address);
                                    if mqtt_topic_matches(topic, &publish.topic) {
                                        if let Some(val) = extract_mqtt_tag_value(&tag.protocol_address, &payload_str) {
                                            if let Ok(coerced) = val.coerce_to(tag.data_type) {
                                                let _ = core_tx.send(TagState {
                                                    tag_id: tag.id.clone(),
                                                    value: coerced,
                                                    quality: Quality::Good,
                                                    timestamp: Utc::now(),
                                                }).await;
                                            }
                                        }
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(err) => {
                                connected = false;
                                core_ref.record_driver_poll(&config.id, false, 0, Some(err.to_string()));
                                let tags = core_ref.get_tags_for_driver(&config.id);
                                publish_bad_disconnect_for_all(&tags, &core_tx).await;
                                tokio::time::sleep(Duration::from_secs(5)).await;
                            }
                        }
                    }
                }
            }
        });

        Ok(join_handle.abort_handle())
    }

    async fn write_tag(&self, address: &str, value: TagValue) -> Result<(), String> {
        self.write_tx
            .send(WriteCommand {
                tag_id: address.to_string(),
                protocol_address: address.to_string(),
                value,
            })
            .await
            .map_err(|e| e.to_string())
    }

    async fn browse_tags(&self) -> Result<Vec<RegisteredTag>, String> {
        Ok(self.config.tags.clone())
    }
}

// ============================================================================
// 4. AUTO-DÉCOUVERTE
// ============================================================================

/// Refus de découverte propre au protocole, ou `None` si l'équipement sait décrire ses objets.
///
/// Seul BACnet/IP décrit son contenu (liste des objets de l'équipement). Pour les autres, des
/// variables inventées (dont des consignes inscriptibles) écriraient à des adresses inconnues.
pub fn discovery_refusal(protocol: &ProtocolType) -> Option<String> {
    match protocol {
        ProtocolType::BacnetIp => None,
        ProtocolType::Simulator => Some(
            "Auto-Découverte sans objet pour le simulateur GTB : son catalogue de variables est fixe"
                .to_string(),
        ),
        other => Some(format!(
            "Auto-Découverte non disponible pour le protocole {:?} : déclarez les variables d'après la table d'échange de l'équipement",
            other
        )),
    }
}

/// Interroge l'équipement et propose ses variables (BACnet/IP uniquement).
pub async fn discover_driver_tags(config: &DriverConfig) -> Result<Vec<RegisteredTag>, String> {
    if let Some(why) = discovery_refusal(&config.protocol) {
        return Err(why);
    }
    bacnet::discover(&config.id, &config.endpoint).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modbus_address_parsing() {
        assert!(matches!(
            ModbusAddressSpec::parse("COIL:0"),
            Ok(ModbusAddressSpec::Coil(0))
        ));
        assert!(matches!(
            ModbusAddressSpec::parse("HR:10:INT16"),
            Ok(ModbusAddressSpec::HoldingInt16(10))
        ));
        assert!(matches!(
            ModbusAddressSpec::parse("HR:2:FLOAT32"),
            Ok(ModbusAddressSpec::HoldingFloat32(2))
        ));
        assert!(matches!(
            ModbusAddressSpec::parse(" IR:4:FLOAT32 "),
            Ok(ModbusAddressSpec::InputRegFloat32(4))
        ));
        assert!(matches!(
            ModbusAddressSpec::parse("HR:6:FLOAT32_SWAP"),
            Ok(ModbusAddressSpec::HoldingFloat32Swap(6))
        ));
        assert!(ModbusAddressSpec::parse("HR:70000").is_err());
        assert!(ModbusAddressSpec::parse("XX:1").is_err());
    }

    #[test]
    fn float32_register_roundtrip() {
        for swap in [false, true] {
            let regs = encode_f32_to_registers_be(7.25, swap);
            assert_eq!(decode_registers_f32_be(&regs, swap).unwrap(), 7.25);
        }
        // 7.5 en IEEE-754 big-endian = 0x40F0_0000
        assert_eq!(encode_f32_to_registers_be(7.5, false), [0x40F0, 0x0000]);
        assert_eq!(encode_f32_to_registers_be(7.5, true), [0x0000, 0x40F0]);
        assert!(decode_registers_f32_be(&[1], false).is_err());
    }

    #[test]
    fn decode_registers_f32_be_rejects_nan_and_infinity() {
        assert!(decode_registers_f32_be(&[0x7FFF, 0xFFFF], false).is_err());
        assert!(decode_registers_f32_be(&[0xFFFF, 0xFFFF], false).is_err());
        assert!(decode_registers_f32_be(&[0xFFFF, 0x7FFF], true).is_err());
        assert!(decode_registers_f32_be(&[0x7F80, 0x0000], false).is_err());
        assert!(decode_registers_f32_be(&[0x0000, 0x7F80], true).is_err());
        assert!(decode_registers_f32_be(&[0xFF80, 0x0000], false).is_err());
        assert!(decode_registers_f32_be(&[0x0000, 0xFF80], true).is_err());
    }

    #[test]
    fn float32_preserves_precision_without_round2() {
        // S-29 : une petite valeur brute (ex. 0.004) ne doit pas être tronquée à 0.00
        let regs = encode_f32_to_registers_be(0.004, false);
        let decoded = decode_registers_f32_be(&regs, false).unwrap();
        assert!((decoded - 0.004).abs() < 1e-6);
        assert!(decoded > 0.0039 && decoded < 0.0041);
    }
    fn specs(addrs: &[&str]) -> Vec<ModbusAddressSpec> {
        addrs
            .iter()
            .map(|a| ModbusAddressSpec::parse(a).unwrap())
            .collect()
    }

    #[test]
    fn block_planning_groups_groupe_froid_tags() {
        // Les 6 tags du groupe froid MODBUS_GF01 : 2 trames au lieu de 6
        let s = specs(&[
            "COIL:0",
            "COIL:1",
            "HR:0:FLOAT32",
            "HR:2:FLOAT32",
            "HR:4:FLOAT32",
            "HR:10:INT16",
        ]);
        let blocks = plan_read_blocks(&s);
        assert_eq!(blocks.len(), 2);
        assert_eq!(
            (blocks[0].area, blocks[0].start, blocks[0].count),
            (RegisterArea::Coils, 0, 2)
        );
        assert_eq!(
            (blocks[1].area, blocks[1].start, blocks[1].count),
            (RegisterArea::HoldingRegisters, 0, 11)
        );
        assert_eq!(blocks[1].members.len(), 4);
    }

    #[test]
    fn block_planning_splits_on_gap_area_and_size() {
        let s = specs(&["HR:0", "HR:50", "IR:0", "HR:1"]);
        let blocks = plan_read_blocks(&s);
        assert_eq!(
            blocks.len(),
            3,
            "écart > 8 et zones différentes : blocs séparés"
        );
        // Limite de 125 registres par requête
        let many: Vec<String> = (0..130).map(|i| format!("HR:{}", i)).collect();
        let refs: Vec<&str> = many.iter().map(|s| s.as_str()).collect();
        let blocks = plan_read_blocks(&specs(&refs));
        assert_eq!(blocks.len(), 2);
        assert!(blocks.iter().all(|b| b.count <= 125));
        assert_eq!(blocks.iter().map(|b| b.members.len()).sum::<usize>(), 130);
    }

    #[test]
    fn decoding_values_inside_a_block() {
        let f = encode_f32_to_registers_be(7.5, false);
        let regs = BlockData::Registers(vec![f[0], f[1], 0xFFFE, 0x0001, 0x0002]);
        let d = |a: &str| decode_from_block(&ModbusAddressSpec::parse(a).unwrap(), 10, &regs);
        assert_eq!(d("HR:10:FLOAT32").unwrap(), TagValue::Float(7.5));
        assert_eq!(d("HR:12:INT16").unwrap(), TagValue::Int(-2));
        assert_eq!(d("HR:12:UINT16").unwrap(), TagValue::Int(65534));
        assert_eq!(d("HR:13:INT32").unwrap(), TagValue::Int(0x0001_0002));
        assert!(d("HR:14:FLOAT32").is_err(), "registre hors réponse");
        let bits = BlockData::Bits(vec![true, false]);
        assert_eq!(
            decode_from_block(&ModbusAddressSpec::Coil(1), 0, &bits).unwrap(),
            TagValue::Bool(false)
        );
    }

    fn encode(addr: &str, v: TagValue) -> Result<ModbusWrite, String> {
        encode_modbus_write(ModbusAddressSpec::parse(addr).unwrap(), &v)
    }

    #[test]
    fn int32_write_is_big_endian_and_decodes_back() {
        let w = encode("HR:20:INT32", TagValue::Int(-123_456)).unwrap();
        let ModbusWrite::Registers(20, regs) = w else {
            panic!("deux registres attendus : {:?}", w);
        };
        let block = BlockData::Registers(regs.to_vec());
        assert_eq!(
            decode_from_block(&ModbusAddressSpec::HoldingInt32(20), 20, &block).unwrap(),
            TagValue::Int(-123_456)
        );
        assert_eq!(
            encode("HR:0:INT32", TagValue::Int(0x0001_0002)).unwrap(),
            ModbusWrite::Registers(0, [0x0001, 0x0002])
        );
        assert!(encode("HR:0:INT32", TagValue::Float(3_000_000_000.0)).is_err());
    }

    #[test]
    fn int16_and_uint16_writes_are_range_checked() {
        // Plus aucune saturation silencieuse (40000 → 32767, -1 → 65535)
        assert!(encode("HR:20:UINT16", TagValue::Int(40_000)).is_ok());
        assert_eq!(
            encode("HR:20:UINT16", TagValue::Int(40_000)).unwrap(),
            ModbusWrite::Register(20, 40_000)
        );
        assert!(encode("HR:20:UINT16", TagValue::Int(-1)).is_err());
        assert!(encode("HR:20:UINT16", TagValue::Int(65_536)).is_err());
        assert!(encode("HR:10:INT16", TagValue::Int(50_000)).is_err());
        assert!(encode("HR:10:INT16", TagValue::Int(-32_769)).is_err());
        assert_eq!(
            encode("HR:10:INT16", TagValue::Int(-2)).unwrap(),
            ModbusWrite::Register(10, 0xFFFE)
        );
        assert_eq!(
            encode("HR:10", TagValue::Float(12.6)).unwrap(),
            ModbusWrite::Register(10, 13)
        );
        assert!(encode("HR:10:INT16", TagValue::Float(f64::NAN)).is_err());
        assert!(encode("HR:0:FLOAT32", TagValue::Float(1e300)).is_err());
        assert!(check_modbus_write("HR:20:UINT16", &TagValue::Int(65_535)).is_ok());
        assert!(check_modbus_write("HR:20:UINT16", &TagValue::Int(65_536)).is_err());
    }

    #[test]
    fn read_only_areas_cannot_be_written_nor_declared_writable() {
        assert!(encode("IR:4:FLOAT32", TagValue::Float(1.0)).is_err());
        assert!(encode("DI:0", TagValue::Bool(true)).is_err());
        assert!(check_modbus_write("IR:0", &TagValue::Int(1)).is_err());

        let mut t = build_local_modbus_plc_config(5502).tags[0].clone();
        assert!(t.writable);
        for tag in build_local_modbus_plc_config(5502).tags {
            assert!(validate_modbus_tag(&tag).is_ok(), "{}", tag.id);
        }
        t.protocol_address = "DI:3".to_string();
        assert!(validate_modbus_tag(&t).is_err());
        t.protocol_address = "IR:3:FLOAT32".to_string();
        assert!(validate_modbus_tag(&t).is_err());
        t.protocol_address = "HR:3:INT32".to_string();
        assert!(validate_modbus_tag(&t).is_ok());
        t.data_type = TagDataType::Text;
        assert!(validate_modbus_tag(&t).is_err());
        // Une variable en lecture seule peut viser n'importe quelle zone
        t.writable = false;
        t.protocol_address = "IR:3:FLOAT32".to_string();
        assert!(validate_modbus_tag(&t).is_ok());
    }

    #[test]
    fn discovery_never_invents_tags() {
        for p in [
            ProtocolType::ModbusTcp,
            ProtocolType::Mqtt,
            ProtocolType::SiemensS7,
            ProtocolType::OpcUa,
            ProtocolType::Simulator,
        ] {
            assert!(discovery_refusal(&p).is_some(), "{p:?}");
        }
        assert!(discovery_refusal(&ProtocolType::BacnetIp).is_none());
    }

    #[test]
    fn mqtt_addresses_and_wildcards() {
        assert_eq!(
            mqtt_split_address("bat/ta01#temp"),
            ("bat/ta01", Some("temp"))
        );
        assert_eq!(mqtt_split_address("bat/ta01"), ("bat/ta01", None));
        assert_eq!(mqtt_split_address("bat/#"), ("bat/#", None));
        assert_eq!(mqtt_split_address("#"), ("#", None));
        assert!(mqtt_topic_matches("bat/ta01", "bat/ta01"));
        assert!(!mqtt_topic_matches("bat/ta01", "bat/ta02"));
        assert!(mqtt_topic_matches("bat/+/temp", "bat/ta01/temp"));
        assert!(!mqtt_topic_matches("bat/+/temp", "bat/ta01/hygro"));
        assert!(mqtt_topic_matches("bat/#", "bat/ta01/temp"));
        assert!(mqtt_topic_matches("bat/#", "bat"));
        assert!(!mqtt_topic_matches("bat/ta01", "bat/ta01/temp"));
    }

    #[test]
    fn mqtt_json_key_payload_encoding_and_extraction() {
        // 1. Écriture avec clé JSON : encapsulée dans un objet {"clé": valeur}
        let payload_bytes =
            encode_mqtt_write_payload("bat/cta01#consigne", &TagValue::Float(21.5)).unwrap();
        let json_val: serde_json::Value = serde_json::from_slice(&payload_bytes).unwrap();
        assert_eq!(json_val, serde_json::json!({ "consigne": 21.5 }));

        // 2. Écriture sans clé JSON : scalaire direct
        let payload_bytes = encode_mqtt_write_payload("bat/cta01", &TagValue::Float(21.5)).unwrap();
        let json_val: serde_json::Value = serde_json::from_slice(&payload_bytes).unwrap();
        assert_eq!(json_val, serde_json::json!(21.5));

        // 3. Réception avec clé JSON présente : extraction correcte
        let raw_json = r#"{"consigne": 21.5, "temp_ext": 14.2}"#;
        assert_eq!(
            extract_mqtt_tag_value("bat/cta01#consigne", raw_json),
            Some(TagValue::Float(21.5))
        );
        assert_eq!(
            extract_mqtt_tag_value("bat/cta01#temp_ext", raw_json),
            Some(TagValue::Float(14.2))
        );

        // 4. Réception avec clé JSON absente : None (jamais de repli vers Text qui écraserait les variables)
        assert_eq!(
            extract_mqtt_tag_value("bat/cta01#autre_cle", raw_json),
            None
        );

        // 5. Réception sans clé JSON : parsing scalaire ou repli vers Text
        assert_eq!(
            extract_mqtt_tag_value("bat/cta01", "21.5"),
            Some(TagValue::Float(21.5))
        );
        assert_eq!(
            extract_mqtt_tag_value("bat/cta01", "texte brut"),
            Some(TagValue::Text("texte brut".into()))
        );
    }

    #[tokio::test]
    async fn modbus_block_failure_falls_back_to_individual_reads() {
        // S-30 : Un bloc fusionné refusé par l'équipement décompose la lecture en requêtes unitaires
        use super::modbus_slave::spawn_embedded_modbus_plc;
        use std::net::SocketAddr;
        use tokio_modbus::client::tcp;

        // Port temporaire non standard conforme AGENTS.md (autre que 3000 et 5502)
        let port = 5518;
        spawn_embedded_modbus_plc(port).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let socket_addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
        let mut ctx = tcp::connect_slave(socket_addr, tokio_modbus::Slave(1))
            .await
            .expect("connexion esclave modbus test");

        // HR:126 est valide (PlcMemory a 128 registres : 0..127)
        // HR:130 est hors mémoire (start 130 > 128)
        // L'écart entre 126 et 130 est 4 (<= MODBUS_MAX_GAP 8), ils sont donc fusionnés en un bloc 126..131 (count = 5).
        // Le bloc fusionné dépasse 128 et reçoit l'exception 0x02 Illegal Data Address.
        // Avec S-30, il retente unitairement : HR:126 réussit (Good), HR:130 échoue (BadDisconnect).
        let tags = vec![
            RegisteredTag {
                id: "TAG_OK".into(),
                label: "TAG_OK".into(),
                module_source: "MODBUS_TEST".into(),
                protocol_address: "HR:126:INT16".into(),
                data_type: TagDataType::Int,
                unit: None,
                writable: false,
                deadband: 0.0,
                scale_factor: 1.0,
                scale_offset: 0.0,
                historize: false,
                alarm_config: Default::default(),
            },
            RegisteredTag {
                id: "TAG_ERR".into(),
                label: "TAG_ERR".into(),
                module_source: "MODBUS_TEST".into(),
                protocol_address: "HR:130:INT16".into(),
                data_type: TagDataType::Int,
                unit: None,
                writable: false,
                deadband: 0.0,
                scale_factor: 1.0,
                scale_offset: 0.0,
                historize: false,
                alarm_config: Default::default(),
            },
        ];

        let mut warned = std::collections::HashSet::new();
        let results = poll_modbus_tags(&mut ctx, &tags, &mut warned)
            .await
            .expect("poll_modbus_tags réussit sans erreur de transport");

        assert_eq!(results.len(), 2);
        let ok_tag = results.iter().find(|(id, _, _)| id == "TAG_OK").unwrap();
        let err_tag = results.iter().find(|(id, _, _)| id == "TAG_ERR").unwrap();

        assert_eq!(ok_tag.2, Quality::Good);
        assert_eq!(err_tag.2, Quality::BadDisconnect);
    }
}
