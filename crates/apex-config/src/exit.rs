//! Codes de sortie du moteur `apex-scada`, partagés avec l'utilitaire desktop qui les traduit
//! en messages pour l'administrateur.

pub const OK: i32 = 0;
pub const FAILURE: i32 = 1;
pub const USAGE: i32 = 2;
pub const CONFIG: i32 = 3;
pub const ALREADY_RUNNING: i32 = 4;
pub const BIND: i32 = 5;
pub const DATABASE: i32 = 6;

/// Explication d'un code de sortie du moteur.
pub fn describe(code: i32) -> &'static str {
    match code {
        OK => "arrêt normal",
        FAILURE => "erreur (voir les journaux)",
        USAGE => "ligne de commande invalide",
        CONFIG => "configuration invalide",
        ALREADY_RUNNING => "une autre instance utilise déjà ce répertoire de données",
        BIND => "adresse ou port d'écoute indisponible",
        DATABASE => "base de données inaccessible",
        _ => "code inconnu",
    }
}
