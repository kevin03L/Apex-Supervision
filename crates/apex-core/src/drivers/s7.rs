//! # Driver Siemens S7 (S7comm sur ISO-on-TCP)
//!
//! Client S7comm écrit sans bibliothèque externe, pour S7-300, S7-400, S7-1200 et S7-1500 :
//! - transport ISO-on-TCP (RFC 1006 : TPKT + COTP classe 0), port 102 ;
//! - connexion de type PG vers le TSAP distant « rack / emplacement » ;
//! - négociation de la taille de PDU (« Setup Communication ») ;
//! - lecture groupée (« Read Var ») et écriture (« Write Var ») des blocs de données (DB),
//!   mémentos (M), entrées (I/E) et sorties (Q/A).
//!
//! Sur S7-1200/1500, l'accès PUT/GET doit être autorisé dans la configuration de l'automate
//! (TIA Portal) et les blocs de données lus doivent être en accès standard (non optimisé).
//!
//! **Endpoint** : `IP[:port][?rack=R&slot=S]` (port 102, rack 0 et emplacement 1 par défaut ;
//! emplacement 2 pour un S7-300).
//!
//! **Adresses** (notation Siemens, insensible à la casse) :
//! - `DB10.DBX4.0` (bit), `DB10.DBB1` (octet), `DB10.DBW2` (mot), `DB10.DBD0` (double mot) ;
//! - `M0.1`, `MB10`, `MW10`, `MD20` ; `I0.0` / `E0.0`, `IW64` ; `Q0.0` / `A0.0`, `QW80` ;
//! - type facultatif après `:` : `BOOL`, `BYTE`, `INT`, `WORD`, `DINT`, `DWORD`, `REAL`.
//!   Sans type : bit → BOOL, octet → BYTE, mot → INT, double mot → REAL pour une variable
//!   décimale, DINT sinon.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::Utc;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
    task::AbortHandle,
};
use tracing::{info, warn};

use crate::{
    core::ScadaCore,
    models::{DriverConfig, Quality, RegisteredTag, TagDataType, TagState, TagValue, WriteCommand},
};

use super::{fallback_value, publish_bad_disconnect_for_all};

/// Délai de réponse d'un échange avec l'automate (connexion comprise).
const S7_TIMEOUT: Duration = Duration::from_secs(3);
/// Taille de PDU demandée (l'automate peut en accorder moins : 240 pour un S7-300).
const S7_REQUESTED_PDU: u16 = 960;
/// Nombre maximal de variables par requête de lecture (limite du protocole : 20).
const S7_MAX_ITEMS: usize = 18;

// ============================================================================
// ADRESSES
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum S7Area {
    Inputs,
    Outputs,
    Merkers,
    DataBlock(u16),
}

impl S7Area {
    fn code(&self) -> u8 {
        match self {
            S7Area::Inputs => 0x81,
            S7Area::Outputs => 0x82,
            S7Area::Merkers => 0x83,
            S7Area::DataBlock(_) => 0x84,
        }
    }

    fn db_number(&self) -> u16 {
        match self {
            S7Area::DataBlock(n) => *n,
            _ => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum S7Type {
    Bool,
    Byte,
    Int,
    Word,
    DInt,
    DWord,
    Real,
}

impl S7Type {
    fn size(&self) -> usize {
        match self {
            S7Type::Bool | S7Type::Byte => 1,
            S7Type::Int | S7Type::Word => 2,
            S7Type::DInt | S7Type::DWord | S7Type::Real => 4,
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "BOOL" => S7Type::Bool,
            "BYTE" => S7Type::Byte,
            "INT" => S7Type::Int,
            "WORD" => S7Type::Word,
            "DINT" => S7Type::DInt,
            "DWORD" => S7Type::DWord,
            "REAL" => S7Type::Real,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct S7Address {
    pub area: S7Area,
    pub byte: u32,
    pub bit: u8,
    pub ty: S7Type,
}

impl S7Address {
    /// Analyse une adresse S7. `data_type` (type de la variable) choisit l'interprétation
    /// d'un double mot sans type explicite (REAL pour une variable décimale, DINT sinon).
    pub fn parse(raw: &str, data_type: TagDataType) -> Result<Self, String> {
        let err = |why: &str| {
            format!(
                "Adresse S7 invalide '{}' : {} (exemples : DB10.DBD0:REAL, DB10.DBX4.0, MW10, I0.0)",
                raw, why
            )
        };
        let upper = raw.trim().to_ascii_uppercase();
        let (body, explicit) = match upper.split_once(':') {
            Some((b, t)) => (
                b.trim(),
                Some(S7Type::parse(t.trim()).ok_or_else(|| err("type inconnu"))?),
            ),
            None => (upper.as_str(), None),
        };

        // Zone, puis lettre de taille (X, B, W, D) et position
        let (area, rest) = if let Some(after_db) = body.strip_prefix("DB") {
            let (num, tail) = after_db
                .split_once('.')
                .ok_or_else(|| err("« DBn. » attendu"))?;
            let n: u16 = num.parse().map_err(|_| err("numéro de DB"))?;
            if n == 0 {
                return Err(err("le numéro de DB commence à 1"));
            }
            let tail = tail
                .strip_prefix("DB")
                .ok_or_else(|| err("« DBX », « DBB », « DBW » ou « DBD » attendu après le DB"))?;
            (S7Area::DataBlock(n), tail)
        } else {
            let mut chars = body.chars();
            let area = match chars.next() {
                Some('I') | Some('E') => S7Area::Inputs,
                Some('Q') | Some('A') => S7Area::Outputs,
                Some('M') => S7Area::Merkers,
                _ => return Err(err("zone inconnue")),
            };
            (area, chars.as_str())
        };

        let (size_letter, position) = match rest.chars().next() {
            Some(c @ ('X' | 'B' | 'W' | 'D')) => (c, &rest[1..]),
            Some(c) if c.is_ascii_digit() => ('X', rest),
            _ => return Err(err("position manquante")),
        };
        let (byte_s, bit_s) = match position.split_once('.') {
            Some((b, bit)) => (b, Some(bit)),
            None => (position, None),
        };
        let byte: u32 = byte_s.parse().map_err(|_| err("octet"))?;
        if byte >= 1 << 21 {
            return Err(err("octet hors plage"));
        }

        let (bit, natural) = match size_letter {
            'X' => {
                let bit: u8 = bit_s
                    .ok_or_else(|| err("numéro de bit manquant (ex. .0)"))?
                    .parse()
                    .map_err(|_| err("numéro de bit"))?;
                if bit > 7 {
                    return Err(err("bit de 0 à 7"));
                }
                (bit, S7Type::Bool)
            }
            other => {
                if bit_s.is_some() {
                    return Err(err("numéro de bit sur un octet, un mot ou un double mot"));
                }
                let natural = match other {
                    'B' => S7Type::Byte,
                    'W' => S7Type::Int,
                    _ if data_type == TagDataType::Float => S7Type::Real,
                    _ => S7Type::DInt,
                };
                (0, natural)
            }
        };

        let ty = match explicit {
            None => natural,
            Some(t)
                if t.size() == natural.size()
                    && (t == S7Type::Bool) == (natural == S7Type::Bool) =>
            {
                t
            }
            Some(_) => return Err(err("le type ne correspond pas à la taille de l'adresse")),
        };
        Ok(S7Address {
            area,
            byte,
            bit,
            ty,
        })
    }

    /// Spécification de l'adresse dans une requête Read Var / Write Var (12 octets).
    fn item_spec(&self) -> [u8; 12] {
        let (transport, count, start) = if self.ty == S7Type::Bool {
            (0x01u8, 1u16, self.byte * 8 + self.bit as u32)
        } else {
            (0x02u8, self.ty.size() as u16, self.byte * 8)
        };
        let db = self.area.db_number();
        [
            0x12,
            0x0A,
            0x10,
            transport,
            (count >> 8) as u8,
            count as u8,
            (db >> 8) as u8,
            db as u8,
            self.area.code(),
            (start >> 16) as u8,
            (start >> 8) as u8,
            start as u8,
        ]
    }

    /// Décode la valeur lue (octets gros-boutistes de l'automate).
    pub fn decode(&self, data: &[u8]) -> Result<TagValue, String> {
        if data.len() < self.ty.size() {
            return Err(format!(
                "réponse trop courte ({} octet(s) pour {:?})",
                data.len(),
                self.ty
            ));
        }
        Ok(match self.ty {
            S7Type::Bool => TagValue::Bool(data[0] & 0x01 != 0),
            S7Type::Byte => TagValue::Int(data[0] as i64),
            S7Type::Int => TagValue::Int(i16::from_be_bytes([data[0], data[1]]) as i64),
            S7Type::Word => TagValue::Int(u16::from_be_bytes([data[0], data[1]]) as i64),
            S7Type::DInt => {
                TagValue::Int(i32::from_be_bytes([data[0], data[1], data[2], data[3]]) as i64)
            }
            S7Type::DWord => {
                TagValue::Int(u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as i64)
            }
            S7Type::Real => {
                let f = f32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                if !f.is_finite() {
                    return Err("valeur REAL non finie".to_string());
                }
                // Représentation décimale la plus courte du REAL (7,1 et non 7,099999904…)
                TagValue::Float(f.to_string().parse::<f64>().unwrap_or(f as f64))
            }
        })
    }

    /// Encode une valeur à écrire, avec contrôle de la plage du type.
    pub fn encode(&self, value: &TagValue) -> Result<Vec<u8>, String> {
        let int = |min: i64, max: i64| -> Result<i64, String> {
            let v = match value {
                TagValue::Int(i) => *i,
                TagValue::Bool(b) => *b as i64,
                TagValue::Float(f) if f.is_finite() => f.round() as i64,
                _ => return Err(format!("valeur entière attendue pour {:?}", self.ty)),
            };
            if v < min || v > max {
                return Err(format!(
                    "valeur {} hors de la plage de {:?} ({} à {})",
                    v, self.ty, min, max
                ));
            }
            Ok(v)
        };
        Ok(match self.ty {
            S7Type::Bool => vec![value
                .as_bool()
                .ok_or_else(|| "valeur booléenne attendue".to_string())?
                as u8],
            S7Type::Byte => vec![int(0, 255)? as u8],
            S7Type::Int => (int(i16::MIN as i64, i16::MAX as i64)? as i16)
                .to_be_bytes()
                .to_vec(),
            S7Type::Word => (int(0, u16::MAX as i64)? as u16).to_be_bytes().to_vec(),
            S7Type::DInt => (int(i32::MIN as i64, i32::MAX as i64)? as i32)
                .to_be_bytes()
                .to_vec(),
            S7Type::DWord => (int(0, u32::MAX as i64)? as u32).to_be_bytes().to_vec(),
            S7Type::Real => {
                let f = value
                    .as_f64()
                    .ok_or_else(|| "valeur numérique attendue".to_string())?;
                if !f.is_finite() || f.abs() > f32::MAX as f64 {
                    return Err(format!("valeur {} hors de la plage de REAL", f));
                }
                (f as f32).to_be_bytes().to_vec()
            }
        })
    }
}

/// Contrôle d'une variable S7 à l'enregistrement.
pub fn validate_s7_tag(tag: &RegisteredTag) -> Result<(), String> {
    if tag.data_type == TagDataType::Text {
        return Err(format!(
            "Variable '{}' : le type Texte n'est pas pris en charge en S7",
            tag.id
        ));
    }
    let addr = S7Address::parse(&tag.protocol_address, tag.data_type)?;
    if (tag.data_type == TagDataType::Bool) != (addr.ty == S7Type::Bool) {
        return Err(format!(
            "Variable '{}' : une variable booléenne doit viser un bit (ex. DB1.DBX0.0) et inversement",
            tag.id
        ));
    }
    if tag.writable && addr.area == S7Area::Inputs {
        return Err(format!(
            "Variable '{}' : les entrées (I/E) de l'automate ne sont pas inscriptibles",
            tag.id
        ));
    }
    Ok(())
}

// ============================================================================
// ENDPOINT
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S7Endpoint {
    pub host: String,
    pub port: u16,
    pub rack: u8,
    pub slot: u8,
}

impl S7Endpoint {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let err = |why: &str| {
            format!(
                "Endpoint S7 invalide '{}' : {} (attendu IP[:port][?rack=0&slot=1], ex. 192.168.0.10?rack=0&slot=1)",
                raw, why
            )
        };
        let raw_t = raw.trim().trim_start_matches("s7://");
        let (addr, query) = match raw_t.split_once('?') {
            Some((a, q)) => (a, Some(q)),
            None => (raw_t, None),
        };
        let (host, port) = match addr.rsplit_once(':') {
            Some((h, p)) => (h, p.parse::<u16>().map_err(|_| err("port"))?),
            None => (addr, 102),
        };
        if host.is_empty() || host.contains(char::is_whitespace) {
            return Err(err("adresse de l'automate"));
        }
        let (mut rack, mut slot) = (0u8, 1u8);
        for pair in query.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').ok_or_else(|| err("paramètre"))?;
            let n: u8 = v
                .parse()
                .map_err(|_| err("valeur de rack ou d'emplacement"))?;
            match k.to_ascii_lowercase().as_str() {
                "rack" if n <= 7 => rack = n,
                "slot" if (1..=31).contains(&n) => slot = n,
                _ => return Err(err("rack de 0 à 7, emplacement de 1 à 31")),
            }
        }
        Ok(S7Endpoint {
            host: host.to_string(),
            port,
            rack,
            slot,
        })
    }

    /// TSAP distant d'une connexion de type PG : 0x01, puis rack × 32 + emplacement.
    fn remote_tsap(&self) -> u16 {
        0x0100 | (self.rack as u16 * 0x20 + self.slot as u16)
    }
}

// ============================================================================
// CLIENT S7COMM
// ============================================================================

/// Défaut d'une variable signalé par l'automate (la liaison reste valide).
type ItemResult = Result<Vec<u8>, String>;

pub struct S7Client {
    stream: TcpStream,
    pdu_size: usize,
    pdu_ref: u16,
}

fn tpkt(payload: &[u8]) -> Vec<u8> {
    let len = payload.len() + 4;
    let mut v = Vec::with_capacity(len);
    v.extend_from_slice(&[0x03, 0x00, (len >> 8) as u8, len as u8]);
    v.extend_from_slice(payload);
    v
}

fn item_error(code: u8) -> String {
    match code {
        0x01 => "erreur matérielle de l'automate".to_string(),
        0x03 => "accès refusé (protection de l'automate)".to_string(),
        0x05 => "adresse hors de la zone (DB trop court ou adresse inexistante)".to_string(),
        0x06 => "type de donnée non pris en charge".to_string(),
        0x07 => "type de donnée incohérent".to_string(),
        0x0A => "objet inexistant (DB absent ou en accès optimisé)".to_string(),
        other => format!("code d'erreur 0x{:02X}", other),
    }
}

impl S7Client {
    /// Ouvre la connexion ISO-on-TCP puis négocie la taille de PDU.
    pub async fn connect(ep: &S7Endpoint) -> Result<Self, String> {
        let stream =
            tokio::time::timeout(S7_TIMEOUT, TcpStream::connect((ep.host.as_str(), ep.port)))
                .await
                .map_err(|_| format!("Pas de réponse de {}:{} en 3 s", ep.host, ep.port))?
                .map_err(|e| format!("Connexion à {}:{} impossible : {}", ep.host, ep.port, e))?;
        let _ = stream.set_nodelay(true);
        let mut client = S7Client {
            stream,
            pdu_size: 240,
            pdu_ref: 0,
        };

        // COTP Connection Request : TSAP local 0x0100, TSAP distant rack/emplacement, TPDU 1024
        let tsap = ep.remote_tsap();
        let cr = [
            0x11,
            0xE0,
            0x00,
            0x00,
            0x00,
            0x01,
            0x00,
            0xC0,
            0x01,
            0x0A,
            0xC1,
            0x02,
            0x01,
            0x00,
            0xC2,
            0x02,
            (tsap >> 8) as u8,
            tsap as u8,
        ];
        let cc = client.exchange(&tpkt(&cr)).await?;
        if cc.get(1) != Some(&0xD0) {
            return Err(format!(
                "Connexion refusée par l'automate (rack {}, emplacement {}) : vérifiez le rack et l'emplacement",
                ep.rack, ep.slot
            ));
        }

        // Setup Communication
        let params = [
            0xF0,
            0x00,
            0x00,
            0x01,
            0x00,
            0x01,
            (S7_REQUESTED_PDU >> 8) as u8,
            S7_REQUESTED_PDU as u8,
        ];
        let (resp_params, _) = client.request(&params, &[]).await?;
        if resp_params.len() >= 8 {
            let pdu = u16::from_be_bytes([resp_params[6], resp_params[7]]) as usize;
            client.pdu_size = pdu.clamp(240, S7_REQUESTED_PDU as usize);
        }
        Ok(client)
    }

    /// Envoie une trame et lit la trame TPKT complète en réponse (délai : 3 s).
    async fn exchange(&mut self, frame: &[u8]) -> Result<Vec<u8>, String> {
        let io = async {
            self.stream
                .write_all(frame)
                .await
                .map_err(|e| format!("Envoi impossible : {}", e))?;
            let mut header = [0u8; 4];
            self.stream
                .read_exact(&mut header)
                .await
                .map_err(|e| format!("Connexion perdue : {}", e))?;
            if header[0] != 0x03 {
                return Err("Trame ISO-on-TCP invalide".to_string());
            }
            let len = u16::from_be_bytes([header[2], header[3]]) as usize;
            if !(7..=4096).contains(&len) {
                return Err(format!("Longueur de trame invalide ({})", len));
            }
            let mut payload = vec![0u8; len - 4];
            self.stream
                .read_exact(&mut payload)
                .await
                .map_err(|e| format!("Connexion perdue : {}", e))?;
            Ok(payload)
        };
        tokio::time::timeout(S7_TIMEOUT, io)
            .await
            .map_err(|_| "Pas de réponse de l'automate en 3 s".to_string())?
    }

    /// Requête S7 (ROSCTR « Job ») ; renvoie les paramètres et les données de la réponse.
    async fn request(&mut self, params: &[u8], data: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
        self.pdu_ref = self.pdu_ref.wrapping_add(1);
        let mut p = Vec::with_capacity(13 + params.len() + data.len());
        // COTP données (DT, dernière unité), puis en-tête S7
        p.extend_from_slice(&[0x02, 0xF0, 0x80, 0x32, 0x01, 0x00, 0x00]);
        p.extend_from_slice(&self.pdu_ref.to_be_bytes());
        p.extend_from_slice(&(params.len() as u16).to_be_bytes());
        p.extend_from_slice(&(data.len() as u16).to_be_bytes());
        p.extend_from_slice(params);
        p.extend_from_slice(data);
        let resp = self.exchange(&tpkt(&p)).await?;
        Self::parse_s7_response(&resp, self.pdu_ref)
    }

    /// Analyse et valide une trame de réponse S7 brute (en-tête COTP DT + PDU S7 Ack_Data).
    pub(crate) fn parse_s7_response(
        resp: &[u8],
        expected_pdu_ref: u16,
    ) -> Result<(Vec<u8>, Vec<u8>), String> {
        // COTP DT (3 octets) puis en-tête S7 de réponse (12 octets)
        if resp.len() < 15 || resp[1] != 0xF0 || resp[3] != 0x32 {
            return Err("Réponse S7 invalide".to_string());
        }
        let s7 = &resp[3..];
        // Contrôle du champ ROSCTR (S-34) : attendu 0x03 (Ack_Data)
        if s7[1] != 0x03 {
            return Err(format!(
                "Réponse S7 invalide : type de message ROSCTR 0x{:02X} inattendu (attendu Ack_Data 0x03)",
                s7[1]
            ));
        }
        if u16::from_be_bytes([s7[4], s7[5]]) != expected_pdu_ref {
            return Err("Réponse S7 hors séquence".to_string());
        }
        let (err_class, err_code) = (s7[10], s7[11]);
        if err_class != 0 || err_code != 0 {
            return Err(match (err_class, err_code) {
                (0x81, 0x04) | (0x85, 0x00) => {
                    "Requête refusée par l'automate : autorisez l'accès PUT/GET (TIA Portal) ou réduisez la requête".to_string()
                }
                (c, e) => format!("Erreur S7 0x{:02X}{:02X}", c, e),
            });
        }
        let plen = u16::from_be_bytes([s7[6], s7[7]]) as usize;
        let dlen = u16::from_be_bytes([s7[8], s7[9]]) as usize;
        if s7.len() < 12 + plen + dlen {
            return Err("Réponse S7 tronquée".to_string());
        }
        Ok((
            s7[12..12 + plen].to_vec(),
            s7[12 + plen..12 + plen + dlen].to_vec(),
        ))
    }

    /// Lit des adresses, en autant de requêtes que la taille de PDU l'impose.
    /// Erreur externe : défaut de liaison. Erreur par variable : refus de l'automate.
    pub async fn read(&mut self, addrs: &[S7Address]) -> Result<Vec<ItemResult>, String> {
        let mut out = Vec::with_capacity(addrs.len());
        let mut start = 0;
        while start < addrs.len() {
            // Lot : au plus S7_MAX_ITEMS variables, et une réponse qui tient dans le PDU
            let mut end = start;
            let mut resp_bytes = 14;
            while end < addrs.len() && end - start < S7_MAX_ITEMS {
                let item = 4 + addrs[end].ty.size() + 1;
                if 14 + 12 * (end - start + 1) > self.pdu_size || resp_bytes + item > self.pdu_size
                {
                    break;
                }
                resp_bytes += item;
                end += 1;
            }
            let batch = &addrs[start..end.max(start + 1)];
            let mut params = vec![0x04, batch.len() as u8];
            for a in batch {
                params.extend_from_slice(&a.item_spec());
            }
            let (_, data) = self.request(&params, &[]).await?;
            let mut pos = 0;
            for i in 0..batch.len() {
                if pos + 4 > data.len() {
                    return Err("Réponse de lecture S7 tronquée".to_string());
                }
                let (code, ts) = (data[pos], data[pos + 1]);
                let len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
                // Longueur exprimée en bits (BIT, BYTE/WORD/DWORD, INTEGER), sinon en octets
                let bytes = match ts {
                    0x03 => len.div_ceil(8),
                    0x04 | 0x05 => len / 8,
                    _ => len,
                };
                pos += 4;
                if code != 0xFF {
                    out.push(Err(item_error(code)));
                    continue;
                }
                if pos + bytes > data.len() {
                    return Err("Réponse de lecture S7 tronquée".to_string());
                }
                out.push(Ok(data[pos..pos + bytes].to_vec()));
                pos += bytes;
                // Remplissage à un nombre pair d'octets, sauf après la dernière variable
                if bytes % 2 == 1 && i + 1 < batch.len() {
                    pos += 1;
                }
            }
            start += batch.len();
        }
        Ok(out)
    }

    /// Écrit une adresse. Erreur externe : défaut de liaison. Interne : refus de l'automate.
    pub async fn write(
        &mut self,
        addr: &S7Address,
        bytes: &[u8],
    ) -> Result<Result<(), String>, String> {
        let mut params = vec![0x05, 0x01];
        params.extend_from_slice(&addr.item_spec());
        let (ts, bits) = if addr.ty == S7Type::Bool {
            (0x03u8, 1u16)
        } else {
            (0x04u8, (bytes.len() * 8) as u16)
        };
        let mut data = vec![0x00, ts, (bits >> 8) as u8, bits as u8];
        data.extend_from_slice(bytes);
        let (_, resp) = self.request(&params, &data).await?;
        match resp.first() {
            Some(0xFF) => Ok(Ok(())),
            Some(code) => Ok(Err(item_error(*code))),
            None => Err("Réponse d'écriture S7 vide".to_string()),
        }
    }
}

// ============================================================================
// DRIVER
// ============================================================================

pub struct S7Driver {
    config: DriverConfig,
    core: Arc<ScadaCore>,
    write_rx: Option<mpsc::Receiver<WriteCommand>>,
    write_tx: mpsc::Sender<WriteCommand>,
}

impl S7Driver {
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
            .ok_or_else(|| format!("Driver S7 '{}' déjà démarré", self.config.id))?;
        let endpoint = S7Endpoint::parse(&self.config.endpoint)?;
        let config = self.config.clone();
        let core_ref = Arc::clone(&self.core);

        let join_handle = tokio::spawn(async move {
            info!(
                "Démarrage du driver Siemens S7 '{}' vers {}:{} (rack {}, emplacement {})",
                config.id, endpoint.host, endpoint.port, endpoint.rack, endpoint.slot
            );
            let poll_duration = Duration::from_millis(config.poll_interval_ms.max(250));
            let mut warned: std::collections::HashSet<String> = Default::default();

            loop {
                let mut client = match S7Client::connect(&endpoint).await {
                    Ok(c) => c,
                    Err(e) => {
                        core_ref.record_driver_poll(&config.id, false, 0, Some(e));
                        let tags = core_ref.get_tags_for_driver(&config.id);
                        publish_bad_disconnect_for_all(&tags, &core_tx).await;
                        tokio::time::sleep(Duration::from_secs(4)).await;
                        continue;
                    }
                };

                // Commandes émises pendant la coupure : obsolètes, non rejouées
                let mut stale = 0usize;
                while write_rx.try_recv().is_ok() {
                    stale += 1;
                }
                if stale > 0 {
                    warn!(
                        "Driver S7 '{}' : {} commande(s) obsolète(s) ignorée(s) à la reconnexion",
                        config.id, stale
                    );
                }

                let mut ticker = tokio::time::interval(poll_duration);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        Some(cmd) = write_rx.recv() => {
                        if core_ref.safe_mode {
                            core_ref.record_driver_write_rejected(&config.id, &cmd.tag_id, "Mode sûr actif côté driver");
                            continue;
                        }
                            let started = Instant::now();
                            // Interprétation de l'adresse selon le type de la variable (comme en lecture)
                            let data_type = core_ref
                                .tags_registry
                                .get(&cmd.tag_id)
                                .map(|t| t.data_type)
                                .unwrap_or_else(|| cmd.value.data_type());
                            let prepared = S7Address::parse(&cmd.protocol_address, data_type)
                                .and_then(|a| a.encode(&cmd.value).map(|b| (a, b)));
                            let (addr, bytes) = match prepared {
                                Ok(p) => p,
                                Err(e) => {
                                    core_ref.record_driver_write_rejected(&config.id, &cmd.tag_id, &e);
                                    continue;
                                }
                            };
                            match client.write(&addr, &bytes).await {
                                Ok(Ok(())) => {
                                    core_ref.record_driver_poll(&config.id, true, started.elapsed().as_millis() as u64, None);
                                    core_ref.record_driver_write_accepted(&config.id);
                                }
                                Ok(Err(e)) => {
                                    warn!("Driver S7 '{}' : écriture de '{}' ({}) refusée : {}", config.id, cmd.tag_id, cmd.protocol_address, e);
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
                            match poll_s7_tags(&mut client, &tags, &mut warned).await {
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

/// Lit toutes les variables du driver. Une adresse invalide ou refusée par l'automate donne
/// une qualité dégradée pour cette seule variable (signalée une fois dans le journal).
async fn poll_s7_tags(
    client: &mut S7Client,
    tags: &[RegisteredTag],
    warned: &mut std::collections::HashSet<String>,
) -> Result<Vec<(String, TagValue, Quality)>, String> {
    let mut out = Vec::with_capacity(tags.len());
    let mut readable: Vec<(&RegisteredTag, S7Address)> = Vec::with_capacity(tags.len());
    for tag in tags {
        match S7Address::parse(&tag.protocol_address, tag.data_type) {
            Ok(a) => readable.push((tag, a)),
            Err(e) => {
                if warned.insert(format!("{}|{}", tag.id, tag.protocol_address)) {
                    warn!("Variable S7 '{}' ignorée : {}", tag.id, e);
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
    let addrs: Vec<S7Address> = readable.iter().map(|(_, a)| *a).collect();
    let results = client.read(&addrs).await?;
    for ((tag, addr), res) in readable.into_iter().zip(results) {
        let decoded = res.and_then(|bytes| addr.decode(&bytes));
        match decoded {
            Ok(v) => match v.coerce_to(tag.data_type) {
                Ok(coerced) => out.push((tag.id.clone(), coerced, Quality::Good)),
                Err(e) => {
                    warn!(
                        "Valeur brute S7 incompatible pour '{}' ({:?} attendu) : {}",
                        tag.id, tag.data_type, e
                    );
                    out.push((
                        tag.id.clone(),
                        fallback_value(tag.data_type),
                        Quality::BadDisconnect,
                    ));
                }
            },
            Err(e) => {
                if warned.insert(format!("{}|{}|{}", tag.id, tag.protocol_address, e)) {
                    warn!(
                        "Lecture S7 de '{}' ({}) refusée : {}",
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

    fn addr(s: &str) -> S7Address {
        S7Address::parse(s, TagDataType::Float).unwrap()
    }

    #[test]
    fn addresses_follow_siemens_notation() {
        assert_eq!(
            addr("DB10.DBD0:REAL"),
            S7Address {
                area: S7Area::DataBlock(10),
                byte: 0,
                bit: 0,
                ty: S7Type::Real
            }
        );
        assert_eq!(
            addr("db10.dbx4.3"),
            S7Address {
                area: S7Area::DataBlock(10),
                byte: 4,
                bit: 3,
                ty: S7Type::Bool
            }
        );
        assert_eq!(addr("MW10").ty, S7Type::Int);
        assert_eq!(addr("MW10:WORD").ty, S7Type::Word);
        assert_eq!(addr("M0.1").area, S7Area::Merkers);
        assert_eq!(addr("E0.0").area, S7Area::Inputs);
        assert_eq!(addr("A1.7").area, S7Area::Outputs);
        assert_eq!(addr("QB3").ty, S7Type::Byte);
        // Double mot sans type : REAL pour une variable décimale, DINT pour un entier
        assert_eq!(addr("MD20").ty, S7Type::Real);
        assert_eq!(
            S7Address::parse("MD20", TagDataType::Int).unwrap().ty,
            S7Type::DInt
        );
        for bad in [
            "DB0.DBX0.0",
            "DB1.DBX0.8",
            "DB1.DBW0.1",
            "MW1:REAL",
            "Z0.0",
            "M0",
            "DB1.DBQ0",
            "M0.0:INT",
        ] {
            assert!(S7Address::parse(bad, TagDataType::Float).is_err(), "{bad}");
        }
    }

    #[test]
    fn item_spec_and_values() {
        // DB10.DBX4.3 : transport BIT, adresse en bits 4*8+3 = 35
        assert_eq!(
            addr("DB10.DBX4.3").item_spec(),
            [0x12, 0x0A, 0x10, 0x01, 0, 1, 0, 10, 0x84, 0, 0, 35]
        );
        assert_eq!(
            addr("MD100:REAL").item_spec(),
            [0x12, 0x0A, 0x10, 0x02, 0, 4, 0, 0, 0x83, 0, 0x03, 0x20]
        );
        let real = addr("DB1.DBD0:REAL");
        let bytes = real.encode(&TagValue::Float(7.1)).unwrap();
        assert_eq!(real.decode(&bytes).unwrap(), TagValue::Float(7.1));
        let int = addr("DB1.DBW0:INT");
        assert_eq!(int.decode(&[0xFF, 0xFE]).unwrap(), TagValue::Int(-2));
        assert_eq!(
            int.encode(&TagValue::Float(215.3)).unwrap(),
            215i16.to_be_bytes().to_vec()
        );
        assert!(int.encode(&TagValue::Int(40000)).is_err());
        assert_eq!(
            addr("DB1.DBW0:WORD").decode(&[0xFF, 0xFE]).unwrap(),
            TagValue::Int(65534)
        );
        assert_eq!(
            addr("DB1.DBX0.0").encode(&TagValue::Bool(true)).unwrap(),
            vec![1]
        );
    }

    #[test]
    fn endpoints() {
        assert_eq!(
            S7Endpoint::parse("192.168.0.10").unwrap(),
            S7Endpoint {
                host: "192.168.0.10".into(),
                port: 102,
                rack: 0,
                slot: 1
            }
        );
        let ep = S7Endpoint::parse("10.0.0.5:1102?rack=0&slot=2").unwrap();
        assert_eq!((ep.port, ep.rack, ep.slot), (1102, 0, 2));
        assert_eq!(ep.remote_tsap(), 0x0102);
        assert_eq!(
            S7Endpoint::parse("plc?rack=1&slot=3")
                .unwrap()
                .remote_tsap(),
            0x0123
        );
        for bad in ["", "plc:abc", "plc?slot=0", "plc?rack=9", "plc?foo=1"] {
            assert!(S7Endpoint::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn tag_validation() {
        let mut t = RegisteredTag {
            id: "T".into(),
            label: "T".into(),
            module_source: "S7".into(),
            protocol_address: "DB1.DBX0.0".into(),
            data_type: TagDataType::Bool,
            unit: None,
            writable: true,
            deadband: 0.0,
            scale_factor: 1.0,
            scale_offset: 0.0,
            historize: false,
            alarm_config: Default::default(),
        };
        assert!(validate_s7_tag(&t).is_ok());
        t.protocol_address = "DB1.DBW0".into();
        assert!(validate_s7_tag(&t).is_err(), "booléen sur un mot");
        t.data_type = TagDataType::Int;
        assert!(validate_s7_tag(&t).is_ok());
        t.protocol_address = "IW0".into();
        assert!(validate_s7_tag(&t).is_err(), "entrée inscriptible");
        t.writable = false;
        assert!(validate_s7_tag(&t).is_ok());
    }

    #[test]
    fn s7_response_rosctr_and_sequence_validation() {
        let pdu_ref = 0x1234u16;

        // 1. Trame Ack_Data valide (ROSCTR = 0x03)
        // COTP: 0x02, 0xF0, 0x80
        // S7: 0x32, 0x03 (ROSCTR Ack_Data), 0x00, 0x00, 0x12, 0x34 (pdu_ref),
        //     0x00, 0x02 (plen=2), 0x00, 0x01 (dlen=1), 0x00, 0x00 (err_class=0, err_code=0)
        // params: [0xAA, 0xBB], data: [0xCC]
        let mut valid_resp = vec![
            0x02, 0xF0, 0x80, 0x32, 0x03, 0x00, 0x00, 0x12, 0x34, 0x00, 0x02, 0x00, 0x01, 0x00,
            0x00, 0xAA, 0xBB, 0xCC,
        ];
        let (params, data) = S7Client::parse_s7_response(&valid_resp, pdu_ref).unwrap();
        assert_eq!(params, vec![0xAA, 0xBB]);
        assert_eq!(data, vec![0xCC]);

        // 2. Trame avec ROSCTR invalide (ex: 0x01 Job au lieu de 0x03 Ack_Data) -> rejeté (S-34)
        valid_resp[4] = 0x01;
        let res = S7Client::parse_s7_response(&valid_resp, pdu_ref);
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("ROSCTR 0x01 inattendu"));

        // 3. Trame hors séquence (pdu_ref différent) -> rejeté
        valid_resp[4] = 0x03;
        let res = S7Client::parse_s7_response(&valid_resp, pdu_ref + 1);
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("hors séquence"));

        // 4. Trame avec erreur S7 renvoyée par l'automate (ex: 0x81, 0x04 PUT/GET)
        valid_resp[13] = 0x81;
        valid_resp[14] = 0x04;
        let res = S7Client::parse_s7_response(&valid_resp, pdu_ref);
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("PUT/GET"));
    }

    #[test]
    fn vague3_s7_valeur_brute_incompatible_degradee_en_baddisconnect() {
        let addr = S7Address::parse("DB1.DBD0", TagDataType::Float).unwrap();
        // 4 octets correspondant à 1e25f32 en IEEE-754 big-endian
        let bytes = 1e25f32.to_be_bytes().to_vec();
        let decoded = addr.decode(&bytes).unwrap();
        assert!(matches!(decoded, TagValue::Float(f) if f > 1e24));

        // Un tag configuré avec TagDataType::Int ne peut pas coercer Float(1e25) :
        // la valeur doit échouer à la coercition et être dégradée en BadDisconnect.
        let coerced = decoded.coerce_to(TagDataType::Int);
        assert!(
            coerced.is_err(),
            "Float(1e25) ne doit pas être coercible en Int"
        );
    }
}
