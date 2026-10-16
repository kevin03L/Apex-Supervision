//! # apex-config — Gestionnaire de configuration d'Apex Supervision
//!
//! Crate légère partagée par le moteur (`apex-scada`) et l'utilitaire desktop
//! (`apex-scada-admin`) :
//!
//! - [`model`] : format de configuration v1, valeurs par défaut sûres, validation stricte ;
//! - [`migrate`] : migrations versionnées du format ;
//! - [`store`] : lecture, écriture atomique, verrou, détection de conflits, sauvegardes ;
//! - [`paths`] : couche système (emplacements Windows / Linux / portable) ;
//! - [`version`] : identité et version du produit ;
//! - [`exit`] : codes de sortie du moteur.

pub mod exit;
pub mod migrate;
pub mod model;
pub mod paths;
pub mod store;
pub mod version;

pub use model::{has_errors, AppConfig, Issue, Severity, CONFIG_FILE_NAME, CONFIG_VERSION};
pub use paths::{Layout, LayoutKind};
pub use store::{ConfigError, Loaded, SaveReport};
