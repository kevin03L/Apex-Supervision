//! # apex-build — outils des scripts de compilation (`build.rs`)
//!
//! Embarque dans les exécutables Windows une ressource commune : l'icône du produit
//! (`packaging/icons/apex-supervision.ico`) et le bloc `VERSIONINFO` (onglet « Détails » des
//! propriétés du fichier, Gestionnaire des tâches, installateur).
//!
//! La version vient du `Cargo.toml` (`[workspace.package]`) ; la version numérique Windows
//! (4 entiers) suit la même règle que `packaging/windows/build-installer.ps1` :
//! `0.4.0-alpha.1` → `0.4.0.1`, `1.0.0` → `1.0.0.0`.
//!
//! Le choix dépend de la **cible** de compilation (`CARGO_CFG_TARGET_OS`) et non de l'hôte :
//! rien n'est fait pour Linux, y compris en compilation croisée depuis Windows.
//! Le compilateur de ressources est `rc.exe` (Windows SDK) pour les chaînes MSVC et `windres`
//! (binutils MinGW) pour les chaînes GNU, via la crate `embed-resource`.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// Nom commercial, identique à `apex_config::version::PRODUCT_NAME`.
pub const NOM_PRODUIT: &str = "Apex Supervision";
/// Éditeur (« CompanyName »).
pub const EDITEUR: &str = "kevin03L";
/// Mention de droits (« LegalCopyright »).
pub const DROITS: &str = "© 2026 kevin03L";

/// Description d'un exécutable pour son bloc `VERSIONINFO`.
#[derive(Debug, Clone, Copy)]
pub struct Executable<'a> {
    /// Nom du binaire sans extension (`apex-scada`).
    pub nom: &'a str,
    /// Description affichée par Windows (« FileDescription », Gestionnaire des tâches).
    pub description: &'a str,
}

/// Chemin de l'icône du produit (fichier versionné, produit par
/// `packaging/icons/generer-icones.ps1`).
pub fn chemin_icone() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("packaging")
        .join("icons")
        .join("apex-supervision.ico")
}

/// Point d'entrée des `build.rs` : compile et lie la ressource Windows de l'exécutable.
///
/// Sans effet si la cible n'est pas Windows. Si aucun compilateur de ressources n'est
/// disponible (chaîne GNU sans `windres`), la compilation continue avec un avertissement :
/// l'exécutable n'aura ni icône ni version. L'installateur refuse de tels binaires
/// (`build-installer.ps1` contrôle la version de fichier).
pub fn ressources_windows(exe: Executable<'_>) {
    let icone = chemin_icone();
    println!("cargo:rerun-if-changed={}", icone.display());
    println!("cargo:rerun-if-env-changed=RC");
    println!("cargo:rerun-if-env-changed=WINDRES");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let version = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION absent");
    let numerique = version_numerique(&version)
        .unwrap_or_else(|| panic!("version « {version} » non convertible en version Windows"));
    let debug = env::var("PROFILE").as_deref() == Ok("debug");
    let rc = script_rc(exe, &version, numerique, debug, &icone);

    let sortie =
        PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR absent")).join(format!("{}.rc", exe.nom));
    fs::write(&sortie, rc).expect("écriture du script de ressources");
    match embed_resource::compile(&sortie, embed_resource::NONE) {
        embed_resource::CompilationResult::Ok | embed_resource::CompilationResult::NotWindows => {}
        embed_resource::CompilationResult::NotAttempted(raison) => println!(
            "cargo:warning=ressources Windows non embarquées (icône, version) : {raison}. \
             Installez windres (binutils MinGW) ou le Windows SDK."
        ),
        embed_resource::CompilationResult::Failed(erreur) => {
            panic!("échec de la compilation des ressources Windows : {erreur}")
        }
    }
}

/// Version numérique Windows (4 entiers de 16 bits) dérivée d'une version SemVer :
/// `majeur.mineur.correctif.N`, où `N` est le dernier nombre de la pré-version
/// (`alpha.1` → 1) et 0 pour une version finale.
pub fn version_numerique(version: &str) -> Option<[u16; 4]> {
    // Les métadonnées de construction (« +… ») n'entrent pas dans la version
    let version = version.split('+').next()?;
    let (coeur, pre) = match version.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (version, None),
    };
    let mut parties = coeur.split('.').map(|p| p.parse::<u16>().ok());
    let majeur = parties.next()??;
    let mineur = parties.next()??;
    let correctif = parties.next()??;
    if parties.next().is_some() {
        return None;
    }
    // Même règle que build-installer.ps1 (`\.(\d+)$`) : seul un nombre précédé d'un point
    // compte (`alpha.1` → 1 ; `1`, `rc` ou `alpha1` → 0)
    let n = pre
        .and_then(|p| p.rsplit_once('.'))
        .and_then(|(_, d)| d.parse::<u16>().ok())
        .unwrap_or(0);
    Some([majeur, mineur, correctif, n])
}

/// Chaîne d'un script de ressources : guillemets doublés, barres obliques inverses échappées.
fn chaine_rc(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\"\""))
}

/// Script de ressources (UTF-8) : icône n° 1 et bloc `VERSIONINFO` en français.
fn script_rc(
    exe: Executable<'_>,
    version: &str,
    numerique: [u16; 4],
    debug: bool,
    icone: &Path,
) -> String {
    let [a, b, c, d] = numerique;
    let numerique_texte = format!("{a}.{b}.{c}.{d}");
    // VS_FF_DEBUG = 0x1, VS_FF_PRERELEASE = 0x2
    let drapeaux = u8::from(debug) | (u8::from(version.contains('-')) << 1);
    // Barres obliques : acceptées par rc.exe comme par windres
    let icone = icone.to_string_lossy().replace('\\', "/");
    let valeurs = [
        ("CompanyName", EDITEUR.to_string()),
        ("FileDescription", exe.description.to_string()),
        ("FileVersion", numerique_texte),
        ("InternalName", exe.nom.to_string()),
        ("LegalCopyright", DROITS.to_string()),
        ("OriginalFilename", format!("{}.exe", exe.nom)),
        ("ProductName", NOM_PRODUIT.to_string()),
        ("ProductVersion", version.to_string()),
    ];

    let mut rc = String::new();
    rc.push_str("// Généré par apex-build (build.rs) : ne pas modifier.\n");
    rc.push_str("#pragma code_page(65001)\n\n");
    let _ = writeln!(rc, "1 ICON {}\n", chaine_rc(&icone));
    let _ = writeln!(rc, "1 VERSIONINFO");
    let _ = writeln!(rc, "FILEVERSION {a},{b},{c},{d}");
    let _ = writeln!(rc, "PRODUCTVERSION {a},{b},{c},{d}");
    rc.push_str("FILEFLAGSMASK 0x3F\n");
    let _ = writeln!(rc, "FILEFLAGS 0x{drapeaux:X}");
    rc.push_str("FILEOS 0x40004\n"); // VOS_NT_WINDOWS32
    rc.push_str("FILETYPE 0x1\n"); // VFT_APP
    rc.push_str("FILESUBTYPE 0x0\n");
    rc.push_str("BEGIN\n  BLOCK \"StringFileInfo\"\n  BEGIN\n");
    // 040C : français (France) ; 04B0 : Unicode (1200)
    rc.push_str("    BLOCK \"040C04B0\"\n    BEGIN\n");
    for (cle, valeur) in valeurs {
        let _ = writeln!(rc, "      VALUE \"{cle}\", {}", chaine_rc(&valeur));
    }
    rc.push_str("    END\n  END\n");
    rc.push_str(
        "  BLOCK \"VarFileInfo\"\n  BEGIN\n    VALUE \"Translation\", 0x040C, 1200\n  END\n",
    );
    rc.push_str("END\n");
    rc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_numerique_suit_la_convention_de_l_installateur() {
        assert_eq!(version_numerique("0.4.0-alpha.1"), Some([0, 4, 0, 1]));
        assert_eq!(version_numerique("0.5.0-beta.12"), Some([0, 5, 0, 12]));
        assert_eq!(version_numerique("1.0.0"), Some([1, 0, 0, 0]));
        assert_eq!(version_numerique("1.0.0-rc"), Some([1, 0, 0, 0]));
        assert_eq!(version_numerique("1.0.0-1"), Some([1, 0, 0, 0]));
        assert_eq!(version_numerique("1.0.0-alpha1"), Some([1, 0, 0, 0]));
        assert_eq!(version_numerique("1.2.3+abc"), Some([1, 2, 3, 0]));
        assert_eq!(version_numerique("1.2"), None);
        assert_eq!(version_numerique("1.2.3.4"), None);
        assert_eq!(version_numerique("70000.0.0"), None);
    }

    #[test]
    fn version_du_workspace_convertible() {
        assert!(version_numerique(env!("CARGO_PKG_VERSION")).is_some());
    }

    #[test]
    fn icone_versionnee_presente() {
        let octets = fs::read(chemin_icone()).expect("icône absente");
        // En-tête ICO : réservé 0, type 1, 6 images (16, 24, 32, 48, 64, 256)
        assert_eq!(&octets[..6], &[0, 0, 1, 0, 6, 0]);
    }

    #[test]
    fn script_rc_complet_et_echappe() {
        let exe = Executable {
            nom: "apex-scada",
            description: "Moteur \"GTC\" — essai",
        };
        let rc = script_rc(
            exe,
            "0.4.0-alpha.1",
            [0, 4, 0, 1],
            false,
            Path::new(r"C:\dépôt\icone.ico"),
        );
        assert!(rc.contains("1 ICON \"C:/dépôt/icone.ico\""));
        assert!(rc.contains("FILEVERSION 0,4,0,1"));
        assert!(rc.contains("FILEFLAGS 0x2"));
        assert!(rc.contains("VALUE \"FileVersion\", \"0.4.0.1\""));
        assert!(rc.contains("VALUE \"ProductVersion\", \"0.4.0-alpha.1\""));
        assert!(rc.contains("VALUE \"ProductName\", \"Apex Supervision\""));
        assert!(rc.contains("VALUE \"CompanyName\", \"kevin03L\""));
        assert!(rc.contains("VALUE \"OriginalFilename\", \"apex-scada.exe\""));
        assert!(rc.contains("VALUE \"FileDescription\", \"Moteur \"\"GTC\"\" — essai\""));
        let debug = script_rc(exe, "1.0.0", [1, 0, 0, 0], true, Path::new("i.ico"));
        assert!(debug.contains("FILEFLAGS 0x1"));
    }
}
