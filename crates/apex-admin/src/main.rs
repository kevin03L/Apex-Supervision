//! # apex-scada-admin — utilitaire desktop d'Apex Supervision
//!
//! Configuration, état, démarrage et arrêt du moteur, journaux, diagnostics, restauration
//! des sauvegardes et assistant de configuration initiale, sans navigateur.
//!
//! L'utilitaire ne dépend pas du moteur : il lit et écrit la configuration via `apex-config`,
//! interroge le moteur en boucle locale (`/api/health`) et pilote le service système.
//! Le fermer n'arrête jamais le moteur.

// Pas de console derrière la fenêtre en version distribuée (Windows)
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod control;
mod diag;
mod form;
mod logs;
mod probe;
mod wizard;

use std::path::PathBuf;

use apex_config::version;
use eframe::egui;

const USAGE: &str = "\
Apex Supervision — utilitaire d'administration

UTILISATION :
  apex-scada-admin [--config <fichier>] [--wizard]

  --config <fichier>  Fichier de configuration à administrer (défaut : installation détectée)
  --wizard            Ouvre l'assistant de configuration initiale
  --version           Affiche la version
";

/// Icône du produit (PNG 256 px produit par `packaging/icons/generer-icones.ps1`), embarquée
/// dans l'exécutable : la fenêtre a son icône sur toutes les plateformes, sans fichier externe.
const ICONE_PNG: &[u8] = include_bytes!("../../../packaging/icons/apex-supervision-256.png");

/// Icône de la fenêtre, décodée au démarrage (décodeur PNG déjà fourni par eframe).
fn icone_fenetre() -> Option<egui::IconData> {
    eframe::icon_data::from_png_bytes(ICONE_PNG).ok()
}

fn parse_args() -> Result<Option<app::Args>, String> {
    let mut args = app::Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config" | "-c" => {
                args.config = Some(PathBuf::from(
                    it.next().ok_or("l'option --config attend un fichier")?,
                ))
            }
            "--wizard" => args.wizard = true,
            "-V" | "--version" => {
                println!(
                    "{} (administration) {}",
                    version::PRODUCT_NAME,
                    version::full_version()
                );
                return Ok(None);
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(None);
            }
            other => return Err(format!("option inconnue « {other} »")),
        }
    }
    Ok(Some(args))
}

fn main() {
    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => return,
        Err(e) => {
            eprintln!("Erreur : {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(format!("{} — Administration", version::PRODUCT_NAME))
        .with_inner_size([1000.0, 700.0])
        .with_min_inner_size([780.0, 520.0])
        .with_app_id("apex-scada-admin");
    // Icône illisible (impossible en pratique, vérifié par un test) : icône par défaut d'eframe
    if let Some(icone) = icone_fenetre() {
        viewport = viewport.with_icon(icone);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    let result = eframe::run_native(
        "apex-scada-admin",
        options,
        Box::new(move |cc| Ok(Box::new(app::AdminApp::new(cc, args)))),
    );
    if let Err(e) = result {
        // Typiquement : aucun moteur de rendu graphique disponible (session sans affichage,
        // pilote absent). Consigné dans un fichier, la fenêtre n'ayant pas pu s'ouvrir.
        let message = format!(
            "L'interface graphique n'a pas pu démarrer : {e}\n\
             Vérifiez qu'une session graphique est ouverte et que le pilote d'affichage est installé.\n\
             En attendant, le moteur s'administre en ligne de commande : apex-scada config show | config check | service status"
        );
        eprintln!("{message}");
        let _ = std::fs::write(
            std::env::temp_dir().join("apex-scada-admin-erreur.txt"),
            &message,
        );
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn icone_fenetre_decodable() {
        let icone = super::icone_fenetre().expect("PNG de l'icône illisible");
        assert_eq!((icone.width, icone.height), (256, 256));
        assert_eq!(icone.rgba.len(), 256 * 256 * 4);
    }
}
