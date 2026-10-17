//! # apex-core — Moteur GTC/SCADA d'Apex Supervision
//!
//! Logique métier indépendante de toute interface :
//!
//! - [`core`] : noyau temps réel `ScadaCore` (tags, drivers, alarmes ISA-18.2, planificateur,
//!   historique, audit, utilisateurs, réglages) ;
//! - [`drivers`] : drivers industriels (Modbus TCP, MQTT, OPC-UA, BACnet/IP, Siemens S7,
//!   simulateur GTB) et automate Modbus esclave embarqué ;
//! - [`flow_engine`] : moteur de flux logiques et scripts Rhai ;
//! - [`models`] : types de données partagés ;
//! - [`security`] : mots de passe, jetons, validation des identifiants, assainissement SVG.

pub mod core;
pub mod drivers;
pub mod flow_engine;
pub mod models;
pub mod security;

pub use crate::core::{EngineOptions, ScadaCore, APP_VERSION};
