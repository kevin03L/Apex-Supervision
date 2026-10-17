//! # Module `drivers::modbus_slave` — Automate Modbus TCP Esclave Embarqué (`127.0.0.1:5502`)
//!
//! Fournit un véritable serveur automate (PLC) Modbus TCP local conforme au standard MBAP :
//! - Écoute sur `127.0.0.1:5502`.
//! - Implémente les codes fonctions officiels :
//!   - `FC01` (Read Coils), `FC02` (Read Discrete Inputs)
//!   - `FC03` (Read Holding Registers), `FC04` (Read Input Registers)
//!   - `FC05` (Write Single Coil), `FC06` (Write Single Register), `FC16` (`0x10` Write Multiple Registers)
//! - Simule la physique d'un **Groupe Froid Eau Glacée (Chiller)** afin que le client `ModbusTcpDriver`
//!   échange de vraies trames TCP/IP Modbus dès le lancement de l'application.

use std::{sync::Arc, time::Duration};

use byteorder::{BigEndian, ByteOrder};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::RwLock,
};
use tracing::{info, warn};

/// Mémoire interne de l'automate Modbus TCP (Coils et Registres 16 bits).
struct PlcMemory {
    coils: [bool; 64],
    discrete_inputs: [bool; 64],
    holding_registers: [u16; 128],
    input_registers: [u16; 128],
}

impl PlcMemory {
    fn new() -> Self {
        let mut mem = Self {
            coils: [false; 64],
            discrete_inputs: [false; 64],
            holding_registers: [0u16; 128],
            input_registers: [0u16; 128],
        };
        // Initialisation Groupe Froid :
        // COIL:0 = Compresseur Marche (true)
        // COIL:1 = Pompe Eau Glacée Marche (true)
        mem.coils[0] = true;
        mem.coils[1] = true;
        // HR:0..1 (FLOAT32) = Température Départ Eau Glacée (7.5 °C)
        write_f32_be(&mut mem.holding_registers, 0, 7.5);
        // HR:2..3 (FLOAT32) = Consigne Eau Glacée (7.0 °C)
        write_f32_be(&mut mem.holding_registers, 2, 7.0);
        // HR:4..5 (FLOAT32) = Pression Fréon Condenseur (14.2 bar)
        write_f32_be(&mut mem.holding_registers, 4, 14.2);
        // HR:10 (INT16) = Intensité Compresseur (38 A)
        mem.holding_registers[10] = 38;

        mem
    }
}

fn write_f32_be(regs: &mut [u16], offset: usize, val: f32) {
    let mut buf = [0u8; 4];
    BigEndian::write_f32(&mut buf, val);
    regs[offset] = BigEndian::read_u16(&buf[0..2]);
    regs[offset + 1] = BigEndian::read_u16(&buf[2..4]);
}

fn read_f32_be(regs: &[u16], offset: usize) -> f32 {
    let mut buf = [0u8; 4];
    BigEndian::write_u16(&mut buf[0..2], regs[offset]);
    BigEndian::write_u16(&mut buf[2..4], regs[offset + 1]);
    BigEndian::read_f32(&buf)
}

/// Démarre l'automate Modbus TCP Esclave embarqué sur `127.0.0.1:5502`.
pub async fn spawn_embedded_modbus_plc(port: u16) {
    let addr = format!("127.0.0.1:{}", port);
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => {
            info!(
                "Automate Modbus TCP Esclave embarqué actif sur {} (Groupe Froid)",
                addr
            );
            l
        }
        Err(err) => {
            warn!(
                "Impossible de lier l'automate Modbus esclave sur {} : {}",
                addr, err
            );
            return;
        }
    };

    let memory = Arc::new(RwLock::new(PlcMemory::new()));

    // Tâche d'évolution physique des registres de l'automate Groupe Froid
    let mem_physics = Arc::clone(&memory);
    tokio::spawn(async move {
        let mut step: f32 = 0.0;
        let mut ticker = tokio::time::interval(Duration::from_millis(800));
        loop {
            ticker.tick().await;
            step += 0.15;
            let mut mem = mem_physics.write().await;

            let comp_on = mem.coils[0];
            let pump_on = mem.coils[1];
            let setpoint = finite_or(read_f32_be(&mem.holding_registers, 2), 7.0).clamp(4.0, 15.0);
            let curr_temp = finite_or(read_f32_be(&mem.holding_registers, 0), 16.5);

            let target_temp = if comp_on && pump_on {
                setpoint + (step.sin() * 0.25)
            } else {
                16.5
            };
            let next_temp = curr_temp + (target_temp - curr_temp) * 0.25;
            write_f32_be(&mut mem.holding_registers, 0, next_temp);

            let freon_bar = if comp_on {
                14.2 + (step * 0.7).cos() * 0.6
            } else {
                5.5
            };
            write_f32_be(&mut mem.holding_registers, 4, freon_bar);

            mem.holding_registers[10] = if comp_on {
                (36.0 + (step.sin() * 3.0).abs()) as u16
            } else {
                2
            };
        }
    });

    // Boucle d'acceptation des connexions maîtres Modbus TCP
    tokio::spawn(async move {
        while let Ok((stream, _peer)) = listener.accept().await {
            let mem_conn = Arc::clone(&memory);
            tokio::spawn(async move {
                handle_modbus_stream(stream, mem_conn).await;
            });
        }
    });
}

async fn handle_modbus_stream(mut stream: TcpStream, memory: Arc<RwLock<PlcMemory>>) {
    let mut buf = [0u8; 260];
    loop {
        // Lecture de l'en-tête MBAP (7 octets)
        if stream.read_exact(&mut buf[0..7]).await.is_err() {
            break;
        }
        let tid = BigEndian::read_u16(&buf[0..2]);
        let pid = BigEndian::read_u16(&buf[2..4]);
        let len = BigEndian::read_u16(&buf[4..6]) as usize;
        let unit_id = buf[6];

        if pid != 0 || !(2..=254).contains(&len) {
            break;
        }

        // Lecture du PDU (len - 1 octets : code fonction + données)
        let pdu_len = len - 1;
        if stream.read_exact(&mut buf[7..7 + pdu_len]).await.is_err() {
            break;
        }

        let fc = buf[7];
        let resp_pdu = {
            let mut mem = memory.write().await;
            process_pdu(&mut mem, fc, &buf[8..7 + pdu_len])
        };

        let mut resp_frame = Vec::with_capacity(7 + resp_pdu.len());
        resp_frame.extend_from_slice(&tid.to_be_bytes());
        resp_frame.extend_from_slice(&0u16.to_be_bytes());
        resp_frame.extend_from_slice(&((1 + resp_pdu.len()) as u16).to_be_bytes());
        resp_frame.push(unit_id);
        resp_frame.extend_from_slice(&resp_pdu);

        if stream.write_all(&resp_frame).await.is_err() {
            break;
        }
    }
}

/// Codes d'exception Modbus.
const EXC_ILLEGAL_FUNCTION: u8 = 0x01;
const EXC_ILLEGAL_DATA_ADDRESS: u8 = 0x02;
const EXC_ILLEGAL_DATA_VALUE: u8 = 0x03;

fn exception(fc: u8, code: u8) -> Vec<u8> {
    vec![fc | 0x80, code]
}

/// Traite un PDU Modbus (code fonction + données) et renvoie le PDU de réponse,
/// avec les bornes et exceptions de la spécification Modbus Application Protocol v1.1b3.
fn process_pdu(mem: &mut PlcMemory, fc: u8, pdu: &[u8]) -> Vec<u8> {
    let bits_len = mem.coils.len();
    let regs_len = mem.holding_registers.len();

    match fc {
        // FC01 (Read Coils) & FC02 (Read Discrete Inputs) : 1 à 2000 bits
        0x01 | 0x02 => {
            if pdu.len() < 4 {
                return exception(fc, EXC_ILLEGAL_DATA_VALUE);
            }
            let start = BigEndian::read_u16(&pdu[0..2]) as usize;
            let count = BigEndian::read_u16(&pdu[2..4]) as usize;
            if count == 0 || count > 2000 {
                return exception(fc, EXC_ILLEGAL_DATA_VALUE);
            }
            if start + count > bits_len {
                return exception(fc, EXC_ILLEGAL_DATA_ADDRESS);
            }
            let byte_count = count.div_ceil(8);
            let mut resp = Vec::with_capacity(2 + byte_count);
            resp.push(fc);
            resp.push(byte_count as u8);
            for b_idx in 0..byte_count {
                let mut byte_val = 0u8;
                for bit in 0..8 {
                    let offset = b_idx * 8 + bit;
                    if offset < count {
                        let idx = start + offset;
                        let state = if fc == 0x01 {
                            mem.coils[idx]
                        } else {
                            mem.discrete_inputs[idx]
                        };
                        if state {
                            byte_val |= 1 << bit;
                        }
                    }
                }
                resp.push(byte_val);
            }
            resp
        }
        // FC03 (Read Holding Registers) & FC04 (Read Input Registers) : 1 à 125 registres
        0x03 | 0x04 => {
            if pdu.len() < 4 {
                return exception(fc, EXC_ILLEGAL_DATA_VALUE);
            }
            let start = BigEndian::read_u16(&pdu[0..2]) as usize;
            let count = BigEndian::read_u16(&pdu[2..4]) as usize;
            if count == 0 || count > 125 {
                return exception(fc, EXC_ILLEGAL_DATA_VALUE);
            }
            if start + count > regs_len {
                return exception(fc, EXC_ILLEGAL_DATA_ADDRESS);
            }
            let mut resp = Vec::with_capacity(2 + count * 2);
            resp.push(fc);
            resp.push((count * 2) as u8);
            for idx in start..start + count {
                let val = if fc == 0x03 {
                    mem.holding_registers[idx]
                } else {
                    mem.input_registers[idx]
                };
                resp.extend_from_slice(&val.to_be_bytes());
            }
            resp
        }
        // FC05 (Write Single Coil) : valeur 0xFF00 (ON) ou 0x0000 (OFF) uniquement
        0x05 => {
            if pdu.len() < 4 {
                return exception(fc, EXC_ILLEGAL_DATA_VALUE);
            }
            let addr = BigEndian::read_u16(&pdu[0..2]) as usize;
            let raw_val = BigEndian::read_u16(&pdu[2..4]);
            if raw_val != 0xFF00 && raw_val != 0x0000 {
                return exception(fc, EXC_ILLEGAL_DATA_VALUE);
            }
            if addr >= bits_len {
                return exception(fc, EXC_ILLEGAL_DATA_ADDRESS);
            }
            mem.coils[addr] = raw_val == 0xFF00;
            let mut resp = vec![fc];
            resp.extend_from_slice(&pdu[0..4]);
            resp
        }
        // FC06 (Write Single Register)
        0x06 => {
            if pdu.len() < 4 {
                return exception(fc, EXC_ILLEGAL_DATA_VALUE);
            }
            let addr = BigEndian::read_u16(&pdu[0..2]) as usize;
            if addr >= regs_len {
                return exception(fc, EXC_ILLEGAL_DATA_ADDRESS);
            }
            mem.holding_registers[addr] = BigEndian::read_u16(&pdu[2..4]);
            let mut resp = vec![fc];
            resp.extend_from_slice(&pdu[0..4]);
            resp
        }
        // FC16 (Write Multiple Registers) : 1 à 123 registres, octets cohérents avec la quantité
        0x10 => {
            if pdu.len() < 5 {
                return exception(fc, EXC_ILLEGAL_DATA_VALUE);
            }
            let start = BigEndian::read_u16(&pdu[0..2]) as usize;
            let count = BigEndian::read_u16(&pdu[2..4]) as usize;
            let byte_count = pdu[4] as usize;
            if count == 0 || count > 123 || byte_count != count * 2 || pdu.len() < 5 + byte_count {
                return exception(fc, EXC_ILLEGAL_DATA_VALUE);
            }
            if start + count > regs_len {
                return exception(fc, EXC_ILLEGAL_DATA_ADDRESS);
            }
            for i in 0..count {
                let off = 5 + i * 2;
                mem.holding_registers[start + i] = BigEndian::read_u16(&pdu[off..off + 2]);
            }
            let mut resp = vec![fc];
            resp.extend_from_slice(&pdu[0..4]);
            resp
        }
        _ => exception(fc, EXC_ILLEGAL_FUNCTION),
    }
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(start: u16, count: u16) -> Vec<u8> {
        let mut v = start.to_be_bytes().to_vec();
        v.extend_from_slice(&count.to_be_bytes());
        v
    }

    #[test]
    fn read_holding_float_setpoint() {
        let mut mem = PlcMemory::new();
        let resp = process_pdu(&mut mem, 0x03, &req(2, 2));
        assert_eq!(resp[0], 0x03);
        assert_eq!(resp[1], 4);
        let regs = [
            BigEndian::read_u16(&resp[2..4]),
            BigEndian::read_u16(&resp[4..6]),
        ];
        assert_eq!(read_f32_be(&regs, 0), 7.0);
    }

    #[test]
    fn read_coils_bits() {
        let mut mem = PlcMemory::new();
        assert_eq!(
            process_pdu(&mut mem, 0x01, &req(0, 3)),
            vec![0x01, 1, 0b011]
        );
    }

    #[test]
    fn quantity_and_address_exceptions() {
        let mut mem = PlcMemory::new();
        // 126 registres : au-delà de la limite Modbus (125) → exception 0x03
        assert_eq!(process_pdu(&mut mem, 0x03, &req(0, 126)), vec![0x83, 0x03]);
        // Hors mémoire (128 registres) → exception 0x02
        assert_eq!(process_pdu(&mut mem, 0x03, &req(120, 10)), vec![0x83, 0x02]);
        assert_eq!(process_pdu(&mut mem, 0x01, &req(0, 0)), vec![0x81, 0x03]);
        assert_eq!(process_pdu(&mut mem, 0x2B, &[]), vec![0xAB, 0x01]);
    }

    #[test]
    fn write_single_coil_and_multiple_registers() {
        let mut mem = PlcMemory::new();
        let mut pdu = 0u16.to_be_bytes().to_vec();
        pdu.extend_from_slice(&0x0000u16.to_be_bytes());
        assert_eq!(process_pdu(&mut mem, 0x05, &pdu)[0], 0x05);
        assert!(!mem.coils[0]);
        // Valeur de bobine invalide
        let mut bad = 1u16.to_be_bytes().to_vec();
        bad.extend_from_slice(&0x1234u16.to_be_bytes());
        assert_eq!(process_pdu(&mut mem, 0x05, &bad), vec![0x85, 0x03]);

        let mut fc16 = req(2, 2);
        fc16.push(4);
        fc16.extend_from_slice(&[0x41, 0x20, 0x00, 0x00]); // 10.0f32
        assert_eq!(process_pdu(&mut mem, 0x10, &fc16), vec![0x10, 0, 2, 0, 2]);
        assert_eq!(read_f32_be(&mem.holding_registers, 2), 10.0);
        // Nombre d'octets incohérent
        let mut bad16 = req(2, 2);
        bad16.push(3);
        bad16.extend_from_slice(&[0, 0, 0]);
        assert_eq!(process_pdu(&mut mem, 0x10, &bad16), vec![0x90, 0x03]);
    }

    #[test]
    fn vague2_simulation_recupere_non_finis() {
        for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(finite_or(v, 7.0), 7.0);
            assert_eq!(finite_or(v, 16.5), 16.5);
        }
        assert_eq!(finite_or(9.0, 7.0), 9.0);
    }
}
