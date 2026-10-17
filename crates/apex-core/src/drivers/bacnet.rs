//! # Driver BACnet/IP (client)
//!
//! Client BACnet/IP écrit sans bibliothèque externe (ASHRAE 135, annexe J) :
//! - trames BVLC « Original-Unicast-NPDU » sur UDP (port 47808 par défaut) ;
//! - services confirmés ReadProperty (lecture) et WriteProperty (écriture avec priorité) ;
//! - auto-découverte : lecture de la liste des objets de l'équipement (Object_List), puis du nom
//!   (Object_Name) et de l'unité de chaque objet.
//!
//! L'équipement est joint directement à son adresse IP. Les équipements situés derrière un
//! routeur BACnet (MS/TP) ne sont pas encore pris en charge.
//!
//! **Endpoint** : `IP[:port]` (47808 par défaut).
//!
//! **Adresses** : `BACNET:<type>:<instance>[:<propriété>][:<priorité>]`
//! - type : `AI`, `AO`, `AV`, `BI`, `BO`, `BV`, `MSI`, `MSO`, `MSV` (ou leur numéro BACnet) ;
//! - propriété : `Present_Value` (par défaut) ou un numéro de propriété ;
//! - priorité d'écriture : 1 à 16 (8, « opérateur manuel », par défaut).

use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::Utc;
use tokio::{net::UdpSocket, sync::mpsc, task::AbortHandle};
use tracing::{info, warn};

use crate::{
    core::ScadaCore,
    models::{
        DriverConfig, Quality, RegisteredTag, TagAlarmConfig, TagDataType, TagState, TagValue,
        WriteCommand,
    },
};

use super::{fallback_value, publish_bad_disconnect_for_all};

/// Délai de réponse d'une requête confirmée.
const BACNET_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_PORT: u16 = 47808;
const PROP_PRESENT_VALUE: u32 = 85;
const PROP_OBJECT_LIST: u32 = 76;
const PROP_OBJECT_NAME: u32 = 77;
const PROP_UNITS: u32 = 117;
/// Instance « joker » qui désigne l'équipement interrogé lui-même (objet Device).
const DEVICE_WILDCARD: u32 = 4_194_303;
const OBJECT_DEVICE: u16 = 8;
const DEFAULT_PRIORITY: u8 = 8;

// ============================================================================
// ADRESSES
// ============================================================================

const OBJECT_TYPES: [(&str, u16); 9] = [
    ("AI", 0),
    ("AO", 1),
    ("AV", 2),
    ("BI", 3),
    ("BO", 4),
    ("BV", 5),
    ("MSI", 13),
    ("MSO", 14),
    ("MSV", 19),
];

fn type_name(object_type: u16) -> Option<&'static str> {
    OBJECT_TYPES
        .iter()
        .find(|(_, n)| *n == object_type)
        .map(|(s, _)| *s)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BacnetAddress {
    pub object_type: u16,
    pub instance: u32,
    pub property: u32,
    pub priority: u8,
}

impl BacnetAddress {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let err = |why: &str| {
            format!(
                "Adresse BACnet invalide '{}' : {} (ex. BACNET:AV:201:Present_Value ou BACNET:BO:3:Present_Value:16)",
                raw, why
            )
        };
        let upper = raw.trim().to_ascii_uppercase();
        let parts: Vec<&str> = upper.split(':').collect();
        if parts.len() < 3 || parts.len() > 5 || parts[0] != "BACNET" {
            return Err(err("format"));
        }
        let object_type = match OBJECT_TYPES.iter().find(|(s, _)| *s == parts[1]) {
            Some((_, n)) => *n,
            None => parts[1]
                .parse::<u16>()
                .ok()
                .filter(|n| *n < 1024)
                .ok_or_else(|| err("type d'objet inconnu"))?,
        };
        let instance: u32 = parts[2].parse().map_err(|_| err("instance"))?;
        if instance >= DEVICE_WILDCARD {
            return Err(err("instance de 0 à 4194302"));
        }
        let property = match parts.get(3) {
            None | Some(&"PRESENT_VALUE") | Some(&"PV") => PROP_PRESENT_VALUE,
            Some(p) => p.parse::<u32>().map_err(|_| err("propriété"))?,
        };
        let priority = match parts.get(4) {
            None => DEFAULT_PRIORITY,
            Some(p) => p
                .parse::<u8>()
                .ok()
                .filter(|p| (1..=16).contains(p))
                .ok_or_else(|| err("priorité de 1 à 16"))?,
        };
        Ok(BacnetAddress {
            object_type,
            instance,
            property,
            priority,
        })
    }

    fn object_id(&self) -> u32 {
        ((self.object_type as u32) << 22) | self.instance
    }

    fn is_input(&self) -> bool {
        matches!(self.object_type, 0 | 3 | 13)
    }

    fn is_binary(&self) -> bool {
        matches!(self.object_type, 3..=5)
    }
}

/// Contrôle d'une variable BACnet à l'enregistrement.
pub fn validate_bacnet_tag(tag: &RegisteredTag) -> Result<(), String> {
    let addr = BacnetAddress::parse(&tag.protocol_address)?;
    if tag.writable && addr.is_input() && addr.property == PROP_PRESENT_VALUE {
        return Err(format!(
            "Variable '{}' : la valeur d'une entrée BACnet (AI, BI, MSI) n'est pas inscriptible",
            tag.id
        ));
    }
    Ok(())
}

pub fn parse_endpoint(raw: &str) -> Result<SocketAddr, String> {
    let t = raw.trim();
    let with_port = if t.contains(':') {
        t.to_string()
    } else {
        format!("{}:{}", t, DEFAULT_PORT)
    };
    with_port.parse::<SocketAddr>().map_err(|_| {
        format!(
            "Endpoint BACnet/IP invalide '{}' (attendu IP[:port], ex. 192.168.1.40:47808)",
            raw
        )
    })
}

// ============================================================================
// ENCODAGE DES VALEURS (étiquettes applicatives et contextuelles)
// ============================================================================

#[derive(Debug, Clone, PartialEq)]
pub enum AppValue {
    Null,
    Boolean(bool),
    Unsigned(u64),
    Signed(i64),
    Real(f32),
    Double(f64),
    CharString(String),
    Enumerated(u32),
    ObjectId(u16, u32),
    Other(u8),
}

fn unsigned_bytes(v: u64) -> Vec<u8> {
    let b = v.to_be_bytes();
    let first = b.iter().position(|x| *x != 0).unwrap_or(7);
    b[first..].to_vec()
}

fn signed_bytes(v: i64) -> Vec<u8> {
    let b = v.to_be_bytes();
    let mut start = 0;
    // Retire les octets de signe redondants (complément à deux minimal)
    while start < 7 {
        let (cur, next) = (b[start], b[start + 1]);
        if (cur == 0x00 && next & 0x80 == 0) || (cur == 0xFF && next & 0x80 != 0) {
            start += 1;
        } else {
            break;
        }
    }
    b[start..].to_vec()
}

/// Étiquette : numéro, classe (contexte ou applicative) et longueur.
fn push_tag(out: &mut Vec<u8>, number: u8, context: bool, len: usize) {
    let class = if context { 0x08 } else { 0x00 };
    if len <= 4 {
        out.push((number << 4) | class | len as u8);
    } else {
        out.push((number << 4) | class | 5);
        if len <= 253 {
            out.push(len as u8);
        } else {
            out.push(254);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        }
    }
}

fn push_context_unsigned(out: &mut Vec<u8>, number: u8, v: u32) {
    let b = unsigned_bytes(v as u64);
    push_tag(out, number, true, b.len());
    out.extend_from_slice(&b);
}

fn push_app_value(out: &mut Vec<u8>, v: &AppValue) {
    match v {
        AppValue::Null => out.push(0x00),
        AppValue::Boolean(b) => out.push(0x10 | *b as u8),
        AppValue::Unsigned(u) => {
            let b = unsigned_bytes(*u);
            push_tag(out, 2, false, b.len());
            out.extend_from_slice(&b);
        }
        AppValue::Signed(i) => {
            let b = signed_bytes(*i);
            push_tag(out, 3, false, b.len());
            out.extend_from_slice(&b);
        }
        AppValue::Real(f) => {
            out.push(0x44);
            out.extend_from_slice(&f.to_be_bytes());
        }
        AppValue::Double(f) => {
            out.extend_from_slice(&[0x55, 0x08]);
            out.extend_from_slice(&f.to_be_bytes());
        }
        AppValue::CharString(s) => {
            push_tag(out, 7, false, s.len() + 1);
            out.push(0x00); // UTF-8
            out.extend_from_slice(s.as_bytes());
        }
        AppValue::Enumerated(e) => {
            let b = unsigned_bytes(*e as u64);
            push_tag(out, 9, false, b.len());
            out.extend_from_slice(&b);
        }
        AppValue::ObjectId(t, i) => {
            out.push(0xC4);
            out.extend_from_slice(&(((*t as u32) << 22) | i).to_be_bytes());
        }
        AppValue::Other(_) => out.push(0x00),
    }
}

/// Étiquette lue : numéro, contexte, et longueur/valeur (LVT) ou ouverture / fermeture.
#[derive(Debug, PartialEq)]
enum Tag {
    Data {
        number: u8,
        context: bool,
        len: usize,
    },
    Opening(u8),
    Closing(u8),
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn byte(&mut self) -> Result<u8, String> {
        let b = *self.buf.get(self.pos).ok_or("trame BACnet tronquée")?;
        self.pos += 1;
        Ok(b)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let s = self
            .buf
            .get(self.pos..self.pos + n)
            .ok_or("trame BACnet tronquée")?;
        self.pos += n;
        Ok(s)
    }

    fn at_end(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn tag(&mut self) -> Result<Tag, String> {
        let first = self.byte()?;
        let mut number = first >> 4;
        if number == 0x0F {
            number = self.byte()?;
        }
        let context = first & 0x08 != 0;
        let lvt = first & 0x07;
        if context && lvt == 6 {
            return Ok(Tag::Opening(number));
        }
        if context && lvt == 7 {
            return Ok(Tag::Closing(number));
        }
        // Booléen applicatif : la valeur est dans le champ longueur
        if !context && number == 1 {
            return Ok(Tag::Data {
                number,
                context,
                len: lvt as usize,
            });
        }
        let len = match lvt {
            0..=4 => lvt as usize,
            _ => match self.byte()? {
                254 => u16::from_be_bytes([self.byte()?, self.byte()?]) as usize,
                255 => {
                    let b = self.take(4)?;
                    u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize
                }
                n => n as usize,
            },
        };
        Ok(Tag::Data {
            number,
            context,
            len,
        })
    }

    fn unsigned(&mut self, len: usize) -> Result<u64, String> {
        if len > 8 {
            return Err("entier BACnet trop long".to_string());
        }
        Ok(self
            .take(len)?
            .iter()
            .fold(0u64, |acc, b| (acc << 8) | *b as u64))
    }

    /// Valeur applicative suivante.
    fn app_value(&mut self) -> Result<AppValue, String> {
        match self.tag()? {
            Tag::Data {
                number,
                context: false,
                len,
            } => Ok(match number {
                0 => AppValue::Null,
                1 => AppValue::Boolean(len != 0),
                2 => AppValue::Unsigned(self.unsigned(len)?),
                3 => {
                    let raw = self.unsigned(len)?;
                    let shift = 64 - 8 * len as u32;
                    AppValue::Signed(if len == 0 {
                        0
                    } else {
                        ((raw << shift) as i64) >> shift
                    })
                }
                4 => {
                    let b = self.take(4)?;
                    AppValue::Real(f32::from_be_bytes([b[0], b[1], b[2], b[3]]))
                }
                5 => {
                    let b = self.take(8)?;
                    let mut a = [0u8; 8];
                    a.copy_from_slice(b);
                    AppValue::Double(f64::from_be_bytes(a))
                }
                7 => {
                    let b = self.take(len)?;
                    let text = match b.split_first() {
                        // Jeux de caractères : 0 = UTF-8 ; ISO 8859-1 (5) converti octet par octet
                        Some((5, rest)) => rest.iter().map(|c| *c as char).collect(),
                        Some((_, rest)) => String::from_utf8_lossy(rest).into_owned(),
                        None => String::new(),
                    };
                    AppValue::CharString(text)
                }
                9 => AppValue::Enumerated(self.unsigned(len)? as u32),
                12 => {
                    let v = self.unsigned(len)? as u32;
                    AppValue::ObjectId((v >> 22) as u16, v & 0x3F_FFFF)
                }
                other => {
                    self.take(len)?;
                    AppValue::Other(other)
                }
            }),
            other => Err(format!("valeur BACnet inattendue ({:?})", other)),
        }
    }
}

// ============================================================================
// CLIENT
// ============================================================================

/// Défaut renvoyé par l'équipement pour une requête (la liaison reste valide).
#[derive(Debug)]
pub enum RequestError {
    /// Pas de réponse ou socket en défaut : liaison perdue.
    Link(String),
    /// Erreur, rejet ou abandon renvoyé par l'équipement.
    Device(String),
}

fn error_text(class: u32, code: u32) -> String {
    let what = match (class, code) {
        (1, 31) => "objet inexistant",
        (2, 32) => "propriété inexistante",
        (2, 40) => "écriture refusée (propriété en lecture seule)",
        (2, 37) => "valeur hors plage",
        (2, 9) => "type de donnée incorrect",
        (2, 41) => "écriture refusée",
        (4, 27) | (4, 0) => "accès refusé (sécurité de l'équipement)",
        _ => "",
    };
    if what.is_empty() {
        format!("erreur BACnet classe {} code {}", class, code)
    } else {
        format!("{} (classe {}, code {})", what, class, code)
    }
}

pub struct BacnetClient {
    socket: UdpSocket,
    invoke_id: u8,
}

impl BacnetClient {
    pub async fn open(device: SocketAddr) -> Result<Self, String> {
        let bind: SocketAddr = if device.is_ipv4() {
            "0.0.0.0:0".parse().unwrap()
        } else {
            "[::]:0".parse().unwrap()
        };
        let socket = UdpSocket::bind(bind)
            .await
            .map_err(|e| format!("Socket UDP indisponible : {}", e))?;
        socket
            .connect(device)
            .await
            .map_err(|e| format!("Équipement BACnet {} injoignable : {}", device, e))?;
        Ok(BacnetClient {
            socket,
            invoke_id: 0,
        })
    }

    /// Envoie une requête confirmée et attend la réponse portant le même identifiant.
    /// Renvoie le corps de la réponse (après l'en-tête APDU), vide pour un SimpleACK.
    async fn confirmed(&mut self, service: u8, body: &[u8]) -> Result<Vec<u8>, RequestError> {
        self.invoke_id = self.invoke_id.wrapping_add(1);
        let invoke = self.invoke_id;
        // NPDU : version 1, réponse attendue ; APDU : requête confirmée, APDU max 1476 octets
        let mut npdu = vec![0x01, 0x04, 0x00, 0x05, invoke, service];
        npdu.extend_from_slice(body);
        let len = npdu.len() + 4;
        let mut frame = vec![0x81, 0x0A, (len >> 8) as u8, len as u8];
        frame.extend_from_slice(&npdu);
        self.socket
            .send(&frame)
            .await
            .map_err(|e| RequestError::Link(format!("Envoi impossible : {}", e)))?;

        let deadline = Instant::now() + BACNET_TIMEOUT;
        let mut buf = [0u8; 1500];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let n = match tokio::time::timeout(remaining, self.socket.recv(&mut buf)).await {
                Err(_) => {
                    return Err(RequestError::Link(format!(
                        "Pas de réponse de l'équipement BACnet en {} s",
                        BACNET_TIMEOUT.as_secs()
                    )))
                }
                // Port injoignable (ICMP) : signalé par la socket connectée
                Ok(Err(e)) => {
                    return Err(RequestError::Link(format!(
                        "Équipement injoignable : {}",
                        e
                    )))
                }
                Ok(Ok(n)) => n,
            };
            if let Some(apdu) = Self::parse_reply(&buf[..n]) {
                if let Some(res) = Self::handle_apdu_reply(apdu, invoke, service) {
                    return res;
                }
            }
        }
    }

    /// Décode la réponse APDU reçue suite à une requête confirmée.
    /// Renvoie `None` s'il s'agit d'une réponse tardive ou hors séquence,
    /// `Some(Ok(payload))` en cas de succès, ou `Some(Err(e))` en cas d'erreur/rejet/anomalie.
    pub(crate) fn handle_apdu_reply(
        apdu: &[u8],
        invoke: u8,
        service: u8,
    ) -> Option<Result<Vec<u8>, RequestError>> {
        if apdu.len() < 2 || apdu[1] != invoke {
            return None; // réponse tardive à une requête précédente
        }
        let pdu_type = apdu[0] >> 4;
        Some(match pdu_type {
            2 => {
                // SimpleACK
                if apdu.len() >= 3 && apdu[2] != service {
                    Err(RequestError::Device(format!(
                        "service BACnet inattendu (reçu {}, attendu {})",
                        apdu[2], service
                    )))
                } else {
                    Ok(Vec::new())
                }
            }
            3 => {
                // ComplexACK : vérification du bit de segmentation SEG et du code de service (S-34)
                if (apdu[0] & 0x08) != 0 {
                    Err(RequestError::Device(
                        "réponse segmentée non prise en charge".to_string(),
                    ))
                } else if apdu.len() < 3 {
                    Err(RequestError::Device(
                        "trame ComplexACK tronquée".to_string(),
                    ))
                } else if apdu[2] != service {
                    Err(RequestError::Device(format!(
                        "service BACnet inattendu (reçu {}, attendu {})",
                        apdu[2], service
                    )))
                } else {
                    Ok(apdu[3..].to_vec())
                }
            }
            5 => {
                let mut r = Reader::new(apdu.get(3..).unwrap_or(&[]));
                let class = match r.app_value() {
                    Ok(AppValue::Enumerated(c)) => c,
                    _ => u32::MAX,
                };
                let code = match r.app_value() {
                    Ok(AppValue::Enumerated(c)) => c,
                    _ => u32::MAX,
                };
                Err(RequestError::Device(error_text(class, code)))
            }
            6 => Err(RequestError::Device(format!(
                "requête rejetée par l'équipement (motif {})",
                apdu.get(2).copied().unwrap_or(0)
            ))),
            7 => Err(RequestError::Device(format!(
                "requête abandonnée par l'équipement (motif {}{})",
                apdu.get(2).copied().unwrap_or(0),
                if apdu.get(2) == Some(&4) {
                    " : réponse segmentée non prise en charge"
                } else {
                    ""
                }
            ))),
            _ => return None,
        })
    }

    /// Numéro d'instance de l'objet Device : Who-Is envoyé à l'équipement, réponse I-Am.
    pub async fn who_is(&mut self) -> Result<u32, String> {
        // NPDU sans réponse attendue ; APDU non confirmée (0x10), service Who-Is (8)
        let frame = [0x81, 0x0A, 0x00, 0x08, 0x01, 0x00, 0x10, 0x08];
        self.socket
            .send(&frame)
            .await
            .map_err(|e| format!("Envoi impossible : {}", e))?;
        let deadline = Instant::now() + BACNET_TIMEOUT;
        let mut buf = [0u8; 1500];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let n = tokio::time::timeout(remaining, self.socket.recv(&mut buf))
                .await
                .map_err(|_| "pas de réponse I-Am de l'équipement".to_string())?
                .map_err(|e| format!("Équipement injoignable : {}", e))?;
            // I-Am : APDU non confirmée (0x10), service 0, puis l'identifiant d'objet Device
            if let Some(apdu) = Self::parse_reply(&buf[..n]) {
                if apdu.len() >= 3 && apdu[0] == 0x10 && apdu[1] == 0x00 {
                    if let Ok(AppValue::ObjectId(OBJECT_DEVICE, instance)) =
                        Reader::new(&apdu[2..]).app_value()
                    {
                        return Ok(instance);
                    }
                }
            }
        }
    }

    /// Extrait l'APDU d'une trame BVLC / NPDU reçue (sans message réseau).
    fn parse_reply(frame: &[u8]) -> Option<&[u8]> {
        if frame.len() < 6 || frame[0] != 0x81 {
            return None;
        }
        // Original-Unicast (0x0A), Original-Broadcast (0x0B) ou Forwarded-NPDU (0x04, +6 octets)
        let mut pos = match frame[1] {
            0x0A | 0x0B => 4,
            0x04 => 10,
            _ => return None,
        };
        if frame.get(pos) != Some(&0x01) {
            return None;
        }
        let control = *frame.get(pos + 1)?;
        pos += 2;
        if control & 0x80 != 0 {
            return None; // message réseau, pas d'APDU
        }
        if control & 0x20 != 0 {
            // DNET (2) + DLEN (1) + DADR, puis compteur de sauts après la source éventuelle
            let dlen = *frame.get(pos + 2)? as usize;
            pos += 3 + dlen;
        }
        if control & 0x08 != 0 {
            let slen = *frame.get(pos + 2)? as usize;
            pos += 3 + slen;
        }
        if control & 0x20 != 0 {
            pos += 1;
        }
        frame.get(pos..).filter(|a| !a.is_empty())
    }

    /// Lit une propriété (et un indice de tableau éventuel).
    pub async fn read_property(
        &mut self,
        object_type: u16,
        instance: u32,
        property: u32,
        index: Option<u32>,
    ) -> Result<AppValue, RequestError> {
        let mut body = Vec::with_capacity(12);
        body.push(0x0C);
        body.extend_from_slice(&(((object_type as u32) << 22) | instance).to_be_bytes());
        push_context_unsigned(&mut body, 1, property);
        if let Some(i) = index {
            push_context_unsigned(&mut body, 2, i);
        }
        let resp = self.confirmed(12, &body).await?;
        // Objet (ctx 0), propriété (ctx 1), indice (ctx 2) éventuel, puis valeur entre [3 … 3]
        let mut r = Reader::new(&resp);
        loop {
            match r.tag().map_err(RequestError::Device)? {
                Tag::Opening(3) => break,
                Tag::Data { len, .. } => {
                    r.take(len).map_err(RequestError::Device)?;
                }
                other => {
                    return Err(RequestError::Device(format!(
                        "réponse ReadProperty inattendue ({:?})",
                        other
                    )))
                }
            }
            if r.at_end() {
                return Err(RequestError::Device(
                    "réponse ReadProperty sans valeur".into(),
                ));
            }
        }
        r.app_value().map_err(RequestError::Device)
    }

    /// Écrit une propriété (priorité de commande des objets commandables).
    pub async fn write_property(
        &mut self,
        addr: &BacnetAddress,
        value: &AppValue,
    ) -> Result<(), RequestError> {
        let mut body = Vec::with_capacity(16);
        body.push(0x0C);
        body.extend_from_slice(&addr.object_id().to_be_bytes());
        push_context_unsigned(&mut body, 1, addr.property);
        body.push(0x3E);
        push_app_value(&mut body, value);
        body.push(0x3F);
        push_context_unsigned(&mut body, 4, addr.priority as u32);
        self.confirmed(15, &body).await.map(|_| ())
    }
}

// ============================================================================
// CONVERSIONS
// ============================================================================

fn to_tag_value(addr: &BacnetAddress, v: AppValue) -> Result<TagValue, String> {
    Ok(match v {
        AppValue::Boolean(b) => TagValue::Bool(b),
        AppValue::Enumerated(e) if addr.is_binary() => TagValue::Bool(e != 0),
        AppValue::Enumerated(e) => TagValue::Int(e as i64),
        AppValue::Unsigned(u) => TagValue::Int(i64::try_from(u).map_err(|_| "valeur hors plage")?),
        AppValue::Signed(i) => TagValue::Int(i),
        AppValue::Real(f) if f.is_finite() => {
            TagValue::Float(f.to_string().parse::<f64>().unwrap_or(f as f64))
        }
        AppValue::Double(f) if f.is_finite() => TagValue::Float(f),
        AppValue::CharString(s) => TagValue::Text(s),
        AppValue::Null => return Err("valeur nulle".to_string()),
        other => return Err(format!("type de valeur non pris en charge ({:?})", other)),
    })
}

/// Valeur à écrire, dans le type attendu par l'objet (REAL, ENUMERATED ou UNSIGNED).
pub fn to_app_value(addr: &BacnetAddress, value: &TagValue) -> Result<AppValue, String> {
    if addr.property != PROP_PRESENT_VALUE {
        return Ok(match value {
            TagValue::Bool(b) => AppValue::Boolean(*b),
            TagValue::Int(i) if *i >= 0 => AppValue::Unsigned(*i as u64),
            TagValue::Int(i) => AppValue::Signed(*i),
            TagValue::Float(f) => AppValue::Real(*f as f32),
            TagValue::Text(s) => AppValue::CharString(s.clone()),
        });
    }
    match addr.object_type {
        0..=2 => {
            let f = value
                .as_f64()
                .ok_or_else(|| "valeur numérique attendue".to_string())?;
            if !f.is_finite() || f.abs() > f32::MAX as f64 {
                return Err(format!("valeur {} hors de la plage d'un REAL", f));
            }
            Ok(AppValue::Real(f as f32))
        }
        3..=5 => Ok(AppValue::Enumerated(
            value
                .as_bool()
                .ok_or_else(|| "valeur booléenne attendue".to_string())? as u32,
        )),
        13 | 14 | 19 => {
            let n = match value {
                TagValue::Int(i) => *i,
                TagValue::Float(f) if f.fract() == 0.0 => *f as i64,
                _ => return Err("état entier attendu (1, 2, 3…)".to_string()),
            };
            if !(1..=u32::MAX as i64).contains(&n) {
                return Err(format!("état {} invalide : les états commencent à 1", n));
            }
            Ok(AppValue::Unsigned(n as u64))
        }
        _ => Err("écriture non prise en charge pour ce type d'objet".to_string()),
    }
}

// ============================================================================
// DRIVER
// ============================================================================

pub struct BacnetDriver {
    config: DriverConfig,
    core: Arc<ScadaCore>,
    write_rx: Option<mpsc::Receiver<WriteCommand>>,
    write_tx: mpsc::Sender<WriteCommand>,
}

impl BacnetDriver {
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
            .ok_or_else(|| format!("Driver BACnet '{}' déjà démarré", self.config.id))?;
        let device = parse_endpoint(&self.config.endpoint)?;
        let config = self.config.clone();
        let core_ref = Arc::clone(&self.core);

        let join_handle = tokio::spawn(async move {
            info!(
                "Démarrage du driver BACnet/IP '{}' vers {}",
                config.id, device
            );
            let poll_duration = Duration::from_millis(config.poll_interval_ms.max(250));
            let mut warned: std::collections::HashSet<String> = Default::default();

            loop {
                let mut client = match BacnetClient::open(device).await {
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
                        "Driver BACnet '{}' : {} commande(s) obsolète(s) ignorée(s) à la reprise",
                        config.id, stale
                    );
                }

                let mut ticker = tokio::time::interval(poll_duration);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        Some(cmd) = write_rx.recv() => {
                            let started = Instant::now();
                            let prepared = BacnetAddress::parse(&cmd.protocol_address)
                                .and_then(|a| to_app_value(&a, &cmd.value).map(|v| (a, v)));
                            let (addr, value) = match prepared {
                                Ok(p) => p,
                                Err(e) => {
                                    core_ref.record_driver_write_rejected(&config.id, &cmd.tag_id, &e);
                                    continue;
                                }
                            };
                            match client.write_property(&addr, &value).await {
                                Ok(()) => {
                                    core_ref.record_driver_poll(&config.id, true, started.elapsed().as_millis() as u64, None);
                                    core_ref.record_driver_write_accepted(&config.id);
                                }
                                Err(RequestError::Device(e)) => {
                                    warn!("Driver BACnet '{}' : écriture de '{}' ({}) refusée : {}", config.id, cmd.tag_id, cmd.protocol_address, e);
                                    core_ref.record_driver_write_rejected(&config.id, &cmd.tag_id, &e);
                                }
                                Err(RequestError::Link(e)) => {
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
                            match poll_bacnet_tags(&mut client, &tags, &mut warned).await {
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
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
        Ok(join_handle.abort_handle())
    }
}

/// Lit toutes les variables. Une erreur de l'équipement (objet absent…) ne dégrade que la
/// variable concernée ; une absence de réponse coupe la liaison.
async fn poll_bacnet_tags(
    client: &mut BacnetClient,
    tags: &[RegisteredTag],
    warned: &mut std::collections::HashSet<String>,
) -> Result<Vec<(String, TagValue, Quality)>, String> {
    let mut out = Vec::with_capacity(tags.len());
    for tag in tags {
        let res = match BacnetAddress::parse(&tag.protocol_address) {
            Err(e) => Err(e),
            Ok(addr) => match client
                .read_property(addr.object_type, addr.instance, addr.property, None)
                .await
            {
                Ok(v) => to_tag_value(&addr, v).and_then(|v| {
                    v.coerce_to(tag.data_type).map_err(|_| {
                        format!("valeur incompatible avec le type {:?}", tag.data_type)
                    })
                }),
                Err(RequestError::Device(e)) => Err(e),
                Err(RequestError::Link(e)) => return Err(e),
            },
        };
        match res {
            Ok(v) => out.push((tag.id.clone(), v, Quality::Good)),
            Err(e) => {
                if warned.insert(format!("{}|{}|{}", tag.id, tag.protocol_address, e)) {
                    warn!(
                        "Lecture BACnet de '{}' ({}) : {}",
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

// ============================================================================
// AUTO-DÉCOUVERTE
// ============================================================================

/// Nombre maximal d'objets parcourus à la découverte.
const DISCOVERY_MAX_OBJECTS: u32 = 500;

/// Identifiant de variable lisible : lettres, chiffres, `_`, `.`, `-` (128 caractères au plus).
fn sanitize(name: &str) -> String {
    let s: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    s.chars().take(80).collect()
}

/// Unités BACnet courantes en GTB (énumération ASHRAE 135, extrait).
fn unit_label(code: u32) -> Option<&'static str> {
    Some(match code {
        62 => "°C",
        64 => "°F",
        98 => "%",
        29 => "%HR",
        47 => "W",
        48 => "kW",
        19 => "kWh",
        53 => "Pa",
        55 => "bar",
        3 => "A",
        5 => "V",
        135 => "m³/h",
        87 => "L/s",
        _ => return None,
    })
}

/// Parcourt les objets de l'équipement et propose une variable par objet analogique, binaire
/// ou multi-états (nom BACnet de l'objet, unité, écriture pour les sorties et les valeurs).
pub async fn discover(driver_id: &str, endpoint: &str) -> Result<Vec<RegisteredTag>, String> {
    let device = parse_endpoint(endpoint)?;
    let mut client = BacnetClient::open(device).await?;
    let link = |e: RequestError| match e {
        RequestError::Link(e) | RequestError::Device(e) => e,
    };
    // Objet Device : instance « joker » (révisions récentes de la norme), sinon instance réelle
    // obtenue par Who-Is / I-Am (tous les équipements)
    let (device_instance, first) = match client
        .read_property(OBJECT_DEVICE, DEVICE_WILDCARD, PROP_OBJECT_LIST, Some(0))
        .await
    {
        Ok(v) => (DEVICE_WILDCARD, v),
        Err(RequestError::Link(e)) => return Err(e),
        Err(RequestError::Device(_)) => {
            let instance = client.who_is().await?;
            let v = client
                .read_property(OBJECT_DEVICE, instance, PROP_OBJECT_LIST, Some(0))
                .await
                .map_err(link)?;
            (instance, v)
        }
    };
    let count = match first {
        AppValue::Unsigned(n) => n as u32,
        other => return Err(format!("liste d'objets illisible ({:?})", other)),
    };
    let mut tags = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for i in 1..=count.min(DISCOVERY_MAX_OBJECTS) {
        let (object_type, instance) = match client
            .read_property(OBJECT_DEVICE, device_instance, PROP_OBJECT_LIST, Some(i))
            .await
            .map_err(link)?
        {
            AppValue::ObjectId(t, n) => (t, n),
            _ => continue,
        };
        let Some(short) = type_name(object_type) else {
            continue;
        };
        let name = match client
            .read_property(object_type, instance, PROP_OBJECT_NAME, None)
            .await
        {
            Ok(AppValue::CharString(s)) if !s.trim().is_empty() => s,
            _ => format!("{}{}", short, instance),
        };
        let addr = BacnetAddress {
            object_type,
            instance,
            property: PROP_PRESENT_VALUE,
            priority: DEFAULT_PRIORITY,
        };
        let (data_type, unit) = match object_type {
            0..=2 => {
                let unit = match client
                    .read_property(object_type, instance, PROP_UNITS, None)
                    .await
                {
                    Ok(AppValue::Enumerated(u)) => unit_label(u).map(str::to_string),
                    _ => None,
                };
                (TagDataType::Float, unit)
            }
            3..=5 => (TagDataType::Bool, None),
            _ => (TagDataType::Int, None),
        };
        let mut id = format!("{}.{}", driver_id, sanitize(&name));
        if !seen.insert(id.clone()) {
            id = format!("{}.{}_{}", driver_id, short, instance);
            seen.insert(id.clone());
        }
        tags.push(RegisteredTag {
            id,
            label: name,
            module_source: driver_id.to_string(),
            protocol_address: format!("BACNET:{}:{}:Present_Value", short, instance),
            data_type,
            unit,
            writable: !addr.is_input(),
            deadband: if data_type == TagDataType::Float {
                0.05
            } else {
                0.0
            },
            scale_factor: 1.0,
            scale_offset: 0.0,
            historize: data_type != TagDataType::Int,
            alarm_config: TagAlarmConfig::default(),
        });
    }
    if tags.is_empty() {
        return Err(
            "aucun objet analogique, binaire ou multi-états trouvé sur l'équipement".into(),
        );
    }
    Ok(tags)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        let a = BacnetAddress::parse("BACNET:AV:201:Present_Value").unwrap();
        assert_eq!(
            (a.object_type, a.instance, a.property, a.priority),
            (2, 201, 85, 8)
        );
        let b = BacnetAddress::parse("bacnet:bo:3:present_value:16").unwrap();
        assert_eq!((b.object_type, b.priority), (4, 16));
        assert_eq!(
            BacnetAddress::parse("BACNET:MSV:3").unwrap().object_type,
            19
        );
        assert_eq!(BacnetAddress::parse("BACNET:2:5:77").unwrap().property, 77);
        assert_eq!(a.object_id(), (2 << 22) | 201);
        for bad in [
            "AV:1",
            "BACNET:XX:1",
            "BACNET:AV:-1",
            "BACNET:AV:4194303",
            "BACNET:AV:1:PV:0",
            "BACNET:AV:1:PV:17",
        ] {
            assert!(BacnetAddress::parse(bad).is_err(), "{bad}");
        }
        assert!(parse_endpoint("192.168.1.40").unwrap().port() == 47808);
        assert!(parse_endpoint("pas une ip:abc").is_err());
    }

    #[test]
    fn read_property_frames_follow_the_standard() {
        // ReadProperty AI:101 Present_Value : 0C 00 00 00 65  19 55
        let mut body = vec![0x0C];
        body.extend_from_slice(&101u32.to_be_bytes()); // objet 0 (AI), instance 101
        push_context_unsigned(&mut body, 1, 85);
        assert_eq!(body, vec![0x0C, 0, 0, 0, 0x65, 0x19, 0x55]);
        // Réponse ComplexACK : objet, propriété, [3] REAL 21.5 [3]
        let resp = [
            0x0C, 0, 0, 0, 0x65, 0x19, 0x55, 0x3E, 0x44, 0x41, 0xAC, 0x00, 0x00, 0x3F,
        ];
        let mut r = Reader::new(&resp);
        assert!(matches!(
            r.tag().unwrap(),
            Tag::Data {
                number: 0,
                context: true,
                len: 4
            }
        ));
        r.take(4).unwrap();
        assert!(matches!(
            r.tag().unwrap(),
            Tag::Data {
                number: 1,
                context: true,
                len: 1
            }
        ));
        r.take(1).unwrap();
        assert_eq!(r.tag().unwrap(), Tag::Opening(3));
        assert_eq!(r.app_value().unwrap(), AppValue::Real(21.5));
    }

    #[test]
    fn values_round_trip() {
        for v in [
            AppValue::Null,
            AppValue::Boolean(true),
            AppValue::Unsigned(0),
            AppValue::Unsigned(300),
            AppValue::Unsigned(70_000),
            AppValue::Signed(-1),
            AppValue::Signed(-300),
            AppValue::Signed(200),
            AppValue::Real(19.5),
            AppValue::Double(-2.25),
            AppValue::CharString("Pompe P1 — chaufferie".into()),
            AppValue::Enumerated(1),
            AppValue::ObjectId(19, 3),
        ] {
            let mut buf = Vec::new();
            push_app_value(&mut buf, &v);
            assert_eq!(Reader::new(&buf).app_value().unwrap(), v, "{buf:02X?}");
        }
    }

    #[test]
    fn write_values_follow_the_object_type() {
        let av = BacnetAddress::parse("BACNET:AV:1").unwrap();
        assert_eq!(
            to_app_value(&av, &TagValue::Int(22)).unwrap(),
            AppValue::Real(22.0)
        );
        let bv = BacnetAddress::parse("BACNET:BV:2").unwrap();
        assert_eq!(
            to_app_value(&bv, &TagValue::Bool(true)).unwrap(),
            AppValue::Enumerated(1)
        );
        let msv = BacnetAddress::parse("BACNET:MSV:3").unwrap();
        assert_eq!(
            to_app_value(&msv, &TagValue::Int(4)).unwrap(),
            AppValue::Unsigned(4)
        );
        assert!(to_app_value(&msv, &TagValue::Int(0)).is_err());
        assert_eq!(
            to_tag_value(&bv, AppValue::Enumerated(1)).unwrap(),
            TagValue::Bool(true)
        );
        assert_eq!(
            to_tag_value(&av, AppValue::Real(7.1)).unwrap(),
            TagValue::Float(7.1)
        );
    }

    #[test]
    fn replies_are_extracted_from_bvlc_frames() {
        // BVLC unicast + NPDU sans adresse + SimpleACK (invoke 7, WriteProperty)
        let frame = [0x81, 0x0A, 0x00, 0x09, 0x01, 0x00, 0x20, 0x07, 0x0F];
        assert_eq!(
            BacnetClient::parse_reply(&frame).unwrap(),
            &[0x20, 0x07, 0x0F]
        );
        // Message réseau : ignoré
        let net = [0x81, 0x0A, 0x00, 0x07, 0x01, 0x80, 0x00];
        assert!(BacnetClient::parse_reply(&net).is_none());
    }

    #[test]
    fn complex_ack_header_and_service_validation() {
        let invoke = 42u8;
        let service = 12u8; // ReadProperty
        let payload = vec![0x3E, 0x44, 0x41, 0xAC, 0x00, 0x00, 0x3F];

        // 1. ComplexACK valide et non segmenté avec le bon service
        let mut valid_apdu = vec![0x30, invoke, service];
        valid_apdu.extend_from_slice(&payload);
        let res = BacnetClient::handle_apdu_reply(&valid_apdu, invoke, service).unwrap();
        assert_eq!(res.unwrap(), payload);

        // 2. ComplexACK avec bit SEG activé (0x38) -> rejeté comme réponse segmentée
        let mut seg_apdu = vec![0x38, invoke, service];
        seg_apdu.extend_from_slice(&payload);
        let res = BacnetClient::handle_apdu_reply(&seg_apdu, invoke, service).unwrap();
        assert!(matches!(res, Err(RequestError::Device(ref msg)) if msg.contains("segmentée")));

        // 3. ComplexACK avec mauvais numéro de service (ex: 15 WriteProperty au lieu de 12) -> rejeté
        let bad_service_apdu = vec![0x30, invoke, 15, 0x01, 0x02];
        let res = BacnetClient::handle_apdu_reply(&bad_service_apdu, invoke, service).unwrap();
        assert!(
            matches!(res, Err(RequestError::Device(ref msg)) if msg.contains("service BACnet inattendu"))
        );

        // 4. ComplexACK tronqué (< 3 octets) -> rejeté
        let trunc_apdu = vec![0x30, invoke];
        let res = BacnetClient::handle_apdu_reply(&trunc_apdu, invoke, service).unwrap();
        assert!(matches!(res, Err(RequestError::Device(ref msg)) if msg.contains("tronquée")));

        // 5. Invoke ID différent -> ignoré (None)
        let other_apdu = vec![0x30, invoke + 1, service, 0x00];
        assert!(BacnetClient::handle_apdu_reply(&other_apdu, invoke, service).is_none());
    }
}
