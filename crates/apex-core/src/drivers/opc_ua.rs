//! # Driver OPC-UA (client)
//!
//! Client OPC-UA reposant sur la bibliothèque `async-opcua` (cryptographie en Rust pur) :
//! - connexion à l'endpoint `opc.tcp://hôte:port[/chemin]`, sans chiffrement, session anonyme ;
//! - lecture groupée de l'attribut `Value` des nœuds à chaque scrutation ;
//! - écriture typée : la valeur est convertie dans le type du nœud sur le serveur (Double,
//!   Float, Int16…), appris à la lecture, car un serveur refuse une écriture d'un autre type.
//!
//! **Adresses** : identifiant de nœud OPC-UA (`ns=2;s=Chiller01.LWT`, `ns=3;i=1002`…).
//! La qualité OPC-UA est reprise : Good → Good, Uncertain → Uncertain, Bad → défaut.
//!
//! Les certificats d'instance (inutiles sans chiffrement mais exigés par la bibliothèque) sont
//! rangés dans `<données>/opcua-pki`, jamais dans le répertoire courant du service.

use std::{
    path::PathBuf,
    str::FromStr,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use dashmap::DashMap;

use ::opcua::{
    client::{ClientBuilder, IdentityToken, Session},
    types::{
        AttributeId, DataValue, EndpointDescription, MessageSecurityMode, NodeId, ReadValueId,
        StatusCode, TimestampsToReturn, UserTokenPolicy, Variant, WriteValue,
    },
};
use chrono::Utc;
use tokio::{sync::mpsc, task::AbortHandle};
use tracing::{info, warn};

use crate::{
    core::ScadaCore,
    models::{DriverConfig, Quality, RegisteredTag, TagState, TagValue, WriteCommand},
};

use super::{fallback_value, publish_bad_disconnect_for_all};

/// Délai d'établissement de la session et de réponse d'une requête.
const OPCUA_TIMEOUT: Duration = Duration::from_secs(5);
/// Nœuds par requête de lecture (limite courante des serveurs : MaxNodesPerRead).
const OPCUA_READ_CHUNK: usize = 200;

// ============================================================================
// ENDPOINT ET ADRESSES
// ============================================================================

/// Contrôle d'un endpoint `opc.tcp://hôte[:port][/chemin]`.
pub fn validate_endpoint(raw: &str) -> Result<(), String> {
    let err = |why: &str| {
        format!(
            "Endpoint OPC-UA invalide '{}' : {} (attendu opc.tcp://hôte:4840)",
            raw, why
        )
    };
    let rest = raw
        .trim()
        .strip_prefix("opc.tcp://")
        .ok_or_else(|| err("préfixe « opc.tcp:// » manquant"))?;
    let authority = rest.split('/').next().unwrap_or("");
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (authority, None),
    };
    if host.is_empty() || host.contains(char::is_whitespace) {
        return Err(err("hôte manquant"));
    }
    if let Some(p) = port {
        p.parse::<u16>().map_err(|_| err("port"))?;
    }
    Ok(())
}

pub fn parse_node_id(raw: &str) -> Result<NodeId, String> {
    NodeId::from_str(raw.trim()).map_err(|_| {
        format!(
            "Identifiant de nœud OPC-UA invalide '{}' (ex. ns=2;s=Chiller01.LWT ou ns=3;i=1002)",
            raw
        )
    })
}

/// Contrôle d'une variable OPC-UA à l'enregistrement.
pub fn validate_opcua_tag(tag: &RegisteredTag) -> Result<(), String> {
    parse_node_id(&tag.protocol_address).map(|_| ())
}

// ============================================================================
// CONVERSIONS
// ============================================================================

/// Valeur OPC-UA → valeur Apex. Les types structurés et tableaux ne sont pas pris en charge.
fn from_variant(v: &Variant) -> Result<TagValue, String> {
    Ok(match v {
        Variant::Boolean(b) => TagValue::Bool(*b),
        Variant::SByte(i) => TagValue::Int(*i as i64),
        Variant::Byte(i) => TagValue::Int(*i as i64),
        Variant::Int16(i) => TagValue::Int(*i as i64),
        Variant::UInt16(i) => TagValue::Int(*i as i64),
        Variant::Int32(i) => TagValue::Int(*i as i64),
        Variant::UInt32(i) => TagValue::Int(*i as i64),
        Variant::Int64(i) => TagValue::Int(*i),
        Variant::UInt64(i) => {
            TagValue::Int(i64::try_from(*i).map_err(|_| "UInt64 hors plage".to_string())?)
        }
        // Représentation décimale la plus courte d'un Float (7,1 et non 7,099999904…)
        Variant::Float(f) => TagValue::Float(f.to_string().parse::<f64>().unwrap_or(*f as f64)),
        Variant::Double(f) => TagValue::Float(*f),
        Variant::String(s) => TagValue::Text(s.as_ref().to_string()),
        Variant::LocalizedText(t) => TagValue::Text(t.text.as_ref().to_string()),
        Variant::Empty => return Err("valeur vide".to_string()),
        other => {
            return Err(format!(
                "type OPC-UA non pris en charge ({:?})",
                other.type_id()
            ))
        }
    })
}

/// Type OPC-UA d'un nœud, appris à la lecture, pour écrire dans ce même type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeKind {
    Boolean,
    SByte,
    Byte,
    Int16,
    UInt16,
    Int32,
    UInt32,
    Int64,
    UInt64,
    Float,
    Double,
    String,
}

impl NodeKind {
    fn of(v: &Variant) -> Option<Self> {
        Some(match v {
            Variant::Boolean(_) => NodeKind::Boolean,
            Variant::SByte(_) => NodeKind::SByte,
            Variant::Byte(_) => NodeKind::Byte,
            Variant::Int16(_) => NodeKind::Int16,
            Variant::UInt16(_) => NodeKind::UInt16,
            Variant::Int32(_) => NodeKind::Int32,
            Variant::UInt32(_) => NodeKind::UInt32,
            Variant::Int64(_) => NodeKind::Int64,
            Variant::UInt64(_) => NodeKind::UInt64,
            Variant::Float(_) => NodeKind::Float,
            Variant::Double(_) => NodeKind::Double,
            Variant::String(_) => NodeKind::String,
            _ => return None,
        })
    }

    /// Valeur Apex → valeur OPC-UA du type du nœud, avec contrôle de plage.
    fn to_variant(self, value: &TagValue) -> Result<Variant, String> {
        let int = |min: i128, max: i128| -> Result<i128, String> {
            let v: i128 = match value {
                TagValue::Int(i) => *i as i128,
                TagValue::Bool(b) => *b as i128,
                TagValue::Float(f) if f.is_finite() => f.round() as i128,
                _ => return Err(format!("valeur entière attendue pour un nœud {:?}", self)),
            };
            if v < min || v > max {
                return Err(format!(
                    "valeur {} hors de la plage du nœud {:?} ({} à {})",
                    v, self, min, max
                ));
            }
            Ok(v)
        };
        Ok(match self {
            NodeKind::Boolean => Variant::Boolean(
                value
                    .as_bool()
                    .ok_or_else(|| "valeur booléenne attendue".to_string())?,
            ),
            NodeKind::SByte => Variant::SByte(int(i8::MIN as i128, i8::MAX as i128)? as i8),
            NodeKind::Byte => Variant::Byte(int(0, u8::MAX as i128)? as u8),
            NodeKind::Int16 => Variant::Int16(int(i16::MIN as i128, i16::MAX as i128)? as i16),
            NodeKind::UInt16 => Variant::UInt16(int(0, u16::MAX as i128)? as u16),
            NodeKind::Int32 => Variant::Int32(int(i32::MIN as i128, i32::MAX as i128)? as i32),
            NodeKind::UInt32 => Variant::UInt32(int(0, u32::MAX as i128)? as u32),
            NodeKind::Int64 => Variant::Int64(int(i64::MIN as i128, i64::MAX as i128)? as i64),
            NodeKind::UInt64 => Variant::UInt64(int(0, u64::MAX as i128)? as u64),
            NodeKind::Float | NodeKind::Double => {
                let f = value
                    .as_f64()
                    .ok_or_else(|| "valeur numérique attendue".to_string())?;
                if !f.is_finite() {
                    return Err("valeur non finie".to_string());
                }
                if self == NodeKind::Float {
                    if f.abs() > f32::MAX as f64 {
                        return Err(format!("valeur {} hors de la plage d'un Float", f));
                    }
                    Variant::Float(f as f32)
                } else {
                    Variant::Double(f)
                }
            }
            NodeKind::String => Variant::String(match value {
                TagValue::Text(s) => s.as_str().into(),
                other => serde_json::to_string(other)
                    .unwrap_or_default()
                    .as_str()
                    .into(),
            }),
        })
    }
}

/// Type des nœuds sur les serveurs, appris à la lecture (clé : « driver|adresse »). Partagé
/// avec le moteur pour refuser avant l'envoi une consigne hors de la plage du nœud.
fn node_kinds() -> &'static DashMap<String, NodeKind> {
    static KINDS: OnceLock<DashMap<String, NodeKind>> = OnceLock::new();
    KINDS.get_or_init(DashMap::new)
}

fn kind_key(driver_id: &str, address: &str) -> String {
    format!("{}|{}", driver_id, address.trim())
}

/// Contrôle d'une consigne avant l'envoi, quand le type du nœud est déjà connu.
pub fn precheck_write(driver_id: &str, address: &str, value: &TagValue) -> Result<(), String> {
    match node_kinds().get(&kind_key(driver_id, address)) {
        Some(kind) => kind.to_variant(value).map(|_| ()),
        None => Ok(()),
    }
}

fn quality_of(status: Option<StatusCode>) -> Quality {
    match status {
        None => Quality::Good,
        Some(s) if s.is_good() => Quality::Good,
        Some(s) if s.is_bad() => Quality::BadDisconnect,
        Some(_) => Quality::Uncertain,
    }
}

// ============================================================================
// CONNEXION
// ============================================================================

struct Connection {
    session: Arc<Session>,
    event_loop: tokio::task::JoinHandle<StatusCode>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.event_loop.abort();
    }
}

impl Connection {
    async fn open(endpoint_url: &str, pki_dir: PathBuf) -> Result<Self, String> {
        let mut client = ClientBuilder::new()
            .application_name("Apex Supervision")
            .application_uri("urn:apex-supervision:client")
            .product_uri("urn:apex-supervision")
            .create_sample_keypair(false)
            .trust_server_certs(true)
            .pki_dir(pki_dir)
            // Reconnexion gérée par le driver (qualité dégradée, puis nouvelle session)
            .session_retry_limit(0)
            .request_timeout(OPCUA_TIMEOUT)
            .keep_alive_interval(Duration::from_secs(5))
            .client()
            .map_err(|e| format!("Configuration du client OPC-UA invalide : {}", e.join(", ")))?;
        let endpoint: EndpointDescription = (
            endpoint_url,
            "None",
            MessageSecurityMode::None,
            UserTokenPolicy::anonymous(),
        )
            .into();
        let (session, event_loop) = client
            .connect_to_endpoint_directly(endpoint, IdentityToken::Anonymous)
            .map_err(|e| format!("Endpoint OPC-UA refusé : {}", e))?;
        session.disable_reconnects();
        let mut event_loop = event_loop.spawn();
        tokio::select! {
            status = &mut event_loop => {
                let status = status.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
                Err(format!("Connexion à {} impossible : {}", endpoint_url, status))
            }
            connected = tokio::time::timeout(OPCUA_TIMEOUT, session.wait_for_connection()) => {
                match connected {
                    Ok(true) => Ok(Connection { session, event_loop }),
                    _ => {
                        event_loop.abort();
                        Err(format!("Pas de réponse de {} en {} s", endpoint_url, OPCUA_TIMEOUT.as_secs()))
                    }
                }
            }
        }
    }

    fn is_lost(&self) -> bool {
        self.event_loop.is_finished()
    }

    async fn close(self) {
        let _ = tokio::time::timeout(Duration::from_secs(2), self.session.disconnect()).await;
        self.event_loop.abort();
    }

    /// Lit l'attribut Value de nœuds. Erreur : défaut de liaison (service refusé ou expiré).
    async fn read(&self, nodes: &[NodeId]) -> Result<Vec<DataValue>, String> {
        let mut out = Vec::with_capacity(nodes.len());
        for chunk in nodes.chunks(OPCUA_READ_CHUNK) {
            let ids: Vec<ReadValueId> = chunk
                .iter()
                .map(|n| ReadValueId {
                    node_id: n.clone(),
                    attribute_id: AttributeId::Value as u32,
                    ..Default::default()
                })
                .collect();
            let values = tokio::time::timeout(
                OPCUA_TIMEOUT,
                self.session.read(&ids, TimestampsToReturn::Neither, 0.0),
            )
            .await
            .map_err(|_| "Pas de réponse du serveur OPC-UA".to_string())?
            .map_err(|e| format!("Lecture OPC-UA impossible : {}", e))?;
            if values.len() != chunk.len() {
                return Err("Réponse de lecture OPC-UA incomplète".to_string());
            }
            out.extend(values);
        }
        Ok(out)
    }

    /// Écrit la valeur d'un nœud. Erreur externe : liaison ; interne : refus du serveur.
    async fn write(&self, node: &NodeId, value: Variant) -> Result<Result<(), String>, String> {
        let wv = WriteValue {
            node_id: node.clone(),
            attribute_id: AttributeId::Value as u32,
            value: DataValue::value_only(value),
            ..Default::default()
        };
        let results = tokio::time::timeout(OPCUA_TIMEOUT, self.session.write(&[wv]))
            .await
            .map_err(|_| "Pas de réponse du serveur OPC-UA".to_string())?
            .map_err(|e| format!("Écriture OPC-UA impossible : {}", e))?;
        match results.first() {
            Some(s) if s.is_good() => Ok(Ok(())),
            Some(s) => Ok(Err(format!("refusée par le serveur ({})", s))),
            None => Err("Réponse d'écriture OPC-UA vide".to_string()),
        }
    }
}

// ============================================================================
// DRIVER
// ============================================================================

pub struct OpcUaDriver {
    config: DriverConfig,
    core: Arc<ScadaCore>,
    write_rx: Option<mpsc::Receiver<WriteCommand>>,
    write_tx: mpsc::Sender<WriteCommand>,
}

impl OpcUaDriver {
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

    pub async fn start(&mut self, core_tx: mpsc::Sender<TagState>) -> Result<AbortHandle, String> {
        let mut write_rx = self
            .write_rx
            .take()
            .ok_or_else(|| format!("Driver OPC-UA '{}' déjà démarré", self.config.id))?;
        validate_endpoint(&self.config.endpoint)?;
        let config = self.config.clone();
        let core_ref = Arc::clone(&self.core);
        let pki_dir = core_ref
            .data_dir
            .clone()
            .unwrap_or_else(std::env::temp_dir)
            .join("opcua-pki");

        let join_handle = tokio::spawn(async move {
            let endpoint = config.endpoint.trim().to_string();
            info!(
                "Démarrage du driver OPC-UA '{}' vers {}",
                config.id, endpoint
            );
            let poll_duration = Duration::from_millis(config.poll_interval_ms.max(250));
            let mut warned: std::collections::HashSet<String> = Default::default();

            loop {
                let conn = match Connection::open(&endpoint, pki_dir.clone()).await {
                    Ok(c) => c,
                    Err(e) => {
                        core_ref.record_driver_poll(&config.id, false, 0, Some(e));
                        let tags = core_ref.get_tags_for_driver(&config.id);
                        publish_bad_disconnect_for_all(&tags, &core_tx).await;
                        tokio::time::sleep(Duration::from_secs(4)).await;
                        continue;
                    }
                };

                let mut stale = 0usize;
                while write_rx.try_recv().is_ok() {
                    stale += 1;
                }
                if stale > 0 {
                    warn!(
                        "Driver OPC-UA '{}' : {} commande(s) obsolète(s) ignorée(s) à la reconnexion",
                        config.id, stale
                    );
                }

                let mut ticker = tokio::time::interval(poll_duration);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    if conn.is_lost() {
                        core_ref.record_driver_poll(
                            &config.id,
                            false,
                            0,
                            Some("Session OPC-UA perdue".to_string()),
                        );
                        let tags = core_ref.get_tags_for_driver(&config.id);
                        publish_bad_disconnect_for_all(&tags, &core_tx).await;
                        break;
                    }
                    tokio::select! {
                        Some(cmd) = write_rx.recv() => {
                            let started = Instant::now();
                            let node = match parse_node_id(&cmd.protocol_address) {
                                Ok(n) => n,
                                Err(e) => {
                                    core_ref.record_driver_write_rejected(&config.id, &cmd.tag_id, &e);
                                    continue;
                                }
                            };
                            // Type du nœud : appris à la lecture, sinon lu maintenant
                            let kind = match node_kinds().get(&kind_key(&config.id, &cmd.protocol_address)) {
                                Some(k) => Some(*k),
                                None => conn
                                    .read(std::slice::from_ref(&node))
                                    .await
                                    .ok()
                                    .and_then(|v| v.first().and_then(|dv| dv.value.as_ref()).and_then(NodeKind::of)),
                            };
                            let Some(kind) = kind else {
                                core_ref.record_driver_write_rejected(
                                    &config.id,
                                    &cmd.tag_id,
                                    "type du nœud inconnu ou non pris en charge",
                                );
                                continue;
                            };
                            let variant = match kind.to_variant(&cmd.value) {
                                Ok(v) => v,
                                Err(e) => {
                                    core_ref.record_driver_write_rejected(&config.id, &cmd.tag_id, &e);
                                    continue;
                                }
                            };
                            match conn.write(&node, variant).await {
                                Ok(Ok(())) => {
                                    core_ref.record_driver_poll(&config.id, true, started.elapsed().as_millis() as u64, None);
                                    core_ref.record_driver_write_accepted(&config.id);
                                }
                                Ok(Err(e)) => {
                                    warn!("Driver OPC-UA '{}' : écriture de '{}' ({}) {}", config.id, cmd.tag_id, cmd.protocol_address, e);
                                    core_ref.record_driver_write_rejected(&config.id, &cmd.tag_id, &e);
                                }
                                Err(e) => {
                                    core_ref.record_driver_poll(&config.id, false, 0, Some(e));
                                    let tags = core_ref.get_tags_for_driver(&config.id);
                                    publish_bad_disconnect_for_all(&tags, &core_tx).await;
                                    break;
                                }
                            }
                        }
                        _ = ticker.tick() => {
                            if core_ref.is_acquisition_paused() {
                                continue;
                            }
                            let started = Instant::now();
                            let tags = core_ref.get_tags_for_driver(&config.id);
                            match poll_opcua_tags(&conn, &config.id, &tags, &mut warned).await {
                                Ok(values) => {
                                    for (tag_id, value, quality) in values {
                                        let _ = core_tx.send(TagState { tag_id, value, quality, timestamp: Utc::now() }).await;
                                    }
                                    core_ref.record_driver_poll(&config.id, true, started.elapsed().as_millis() as u64, None);
                                }
                                Err(e) => {
                                    core_ref.record_driver_poll(&config.id, false, 0, Some(e));
                                    publish_bad_disconnect_for_all(&tags, &core_tx).await;
                                    break;
                                }
                            }
                        }
                    }
                }
                conn.close().await;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
        Ok(join_handle.abort_handle())
    }
}

async fn poll_opcua_tags(
    conn: &Connection,
    driver_id: &str,
    tags: &[RegisteredTag],
    warned: &mut std::collections::HashSet<String>,
) -> Result<Vec<(String, TagValue, Quality)>, String> {
    let mut out = Vec::with_capacity(tags.len());
    let mut readable: Vec<(&RegisteredTag, NodeId)> = Vec::with_capacity(tags.len());
    for tag in tags {
        match parse_node_id(&tag.protocol_address) {
            Ok(n) => readable.push((tag, n)),
            Err(e) => {
                if warned.insert(format!("{}|{}", tag.id, tag.protocol_address)) {
                    warn!("Variable OPC-UA '{}' ignorée : {}", tag.id, e);
                }
                out.push((
                    tag.id.clone(),
                    fallback_value(tag.data_type),
                    Quality::BadDisconnect,
                ));
            }
        }
    }
    if readable.is_empty() {
        return Ok(out);
    }
    let nodes: Vec<NodeId> = readable.iter().map(|(_, n)| n.clone()).collect();
    let values = conn.read(&nodes).await?;
    for ((tag, _), dv) in readable.into_iter().zip(values) {
        let quality = quality_of(dv.status);
        let decoded = match (&dv.value, quality) {
            (_, Quality::BadDisconnect) => Err(format!(
                "état {}",
                dv.status.map(|s| s.to_string()).unwrap_or_default()
            )),
            (Some(v), _) => {
                if let Some(k) = NodeKind::of(v) {
                    node_kinds().insert(kind_key(driver_id, &tag.protocol_address), k);
                }
                from_variant(v)
            }
            (None, _) => Err("valeur absente".to_string()),
        };
        match decoded.and_then(|v| {
            v.coerce_to(tag.data_type)
                .map_err(|_| format!("valeur incompatible avec le type {:?}", tag.data_type))
        }) {
            Ok(v) => out.push((tag.id.clone(), v, quality)),
            Err(e) => {
                if warned.insert(format!("{}|{}|{}", tag.id, tag.protocol_address, e)) {
                    warn!(
                        "Lecture OPC-UA de '{}' ({}) : {}",
                        tag.id, tag.protocol_address, e
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
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_and_node_ids() {
        assert!(validate_endpoint("opc.tcp://192.168.1.30:4840").is_ok());
        assert!(validate_endpoint("opc.tcp://serveur/UA/Chiller").is_ok());
        for bad in [
            "192.168.1.30:4840",
            "opc.tcp://",
            "opc.tcp://h:port",
            "http://h:4840",
        ] {
            assert!(validate_endpoint(bad).is_err(), "{bad}");
        }
        assert!(parse_node_id("ns=2;s=Chiller01.LWT").is_ok());
        assert!(parse_node_id("ns=3;i=1002").is_ok());
        assert!(parse_node_id("pas un nœud").is_err());
    }

    #[test]
    fn conversions_keep_the_node_type_and_check_ranges() {
        assert_eq!(
            from_variant(&Variant::Float(7.1)).unwrap(),
            TagValue::Float(7.1)
        );
        assert_eq!(
            from_variant(&Variant::UInt16(65534)).unwrap(),
            TagValue::Int(65534)
        );
        assert!(from_variant(&Variant::Empty).is_err());
        assert_eq!(
            NodeKind::Int16.to_variant(&TagValue::Int(-300)).unwrap(),
            Variant::Int16(-300)
        );
        assert_eq!(
            NodeKind::Int16.to_variant(&TagValue::Float(215.3)).unwrap(),
            Variant::Int16(215)
        );
        assert!(NodeKind::Int16.to_variant(&TagValue::Int(70000)).is_err());
        assert!(NodeKind::UInt16.to_variant(&TagValue::Int(-1)).is_err());
        assert_eq!(
            NodeKind::UInt16
                .to_variant(&TagValue::Float(215.7))
                .unwrap(),
            Variant::UInt16(216)
        );
        assert_eq!(
            NodeKind::Double.to_variant(&TagValue::Int(21)).unwrap(),
            Variant::Double(21.0)
        );
        assert_eq!(
            NodeKind::Boolean.to_variant(&TagValue::Bool(true)).unwrap(),
            Variant::Boolean(true)
        );
        assert_eq!(quality_of(Some(StatusCode::Good)), Quality::Good);
        assert_eq!(
            quality_of(Some(StatusCode::BadNodeIdUnknown)),
            Quality::BadDisconnect
        );
        assert_eq!(quality_of(Some(StatusCode::Uncertain)), Quality::Uncertain);
    }

    #[tokio::test]
    async fn connection_drop_aborts_event_loop() {
        let task: tokio::task::JoinHandle<StatusCode> = tokio::spawn(async {
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
        let handle = task.abort_handle();
        assert!(!handle.is_finished());

        // On vérifie que le drop de Connection appelle bien abort()
        // Créons un client minimal sans connexion réseau
        let pki = std::env::temp_dir().join("apex-test-opcua-drop");
        let client = ClientBuilder::new()
            .create_sample_keypair(false)
            .trust_server_certs(true)
            .pki_dir(pki)
            .client();
        if let Ok(mut client) = client {
            let endpoint: EndpointDescription = (
                "opc.tcp://127.0.0.1:4840",
                "None",
                MessageSecurityMode::None,
                UserTokenPolicy::anonymous(),
            )
                .into();
            if let Ok((session, _)) =
                client.connect_to_endpoint_directly(endpoint, IdentityToken::Anonymous)
            {
                let conn = Connection {
                    session,
                    event_loop: task,
                };
                drop(conn);
                tokio::time::sleep(Duration::from_millis(20)).await;
                assert!(handle.is_finished());
            }
        }
    }
}
