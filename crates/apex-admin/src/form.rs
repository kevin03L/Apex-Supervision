//! Éléments de formulaire partagés par l'éditeur de configuration et l'assistant initial.

use apex_config::{AppConfig, Issue, Layout, Severity};
use eframe::egui::{self, Color32, RichText};

pub fn ok_color(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(110, 200, 130)
    } else {
        Color32::from_rgb(25, 125, 55)
    }
}

pub fn warn_color(ui: &egui::Ui) -> Color32 {
    ui.visuals().warn_fg_color
}

pub fn error_color(ui: &egui::Ui) -> Color32 {
    ui.visuals().error_fg_color
}

pub fn info_color(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(120, 170, 230)
    } else {
        Color32::from_rgb(30, 90, 170)
    }
}

/// Pastille de couleur (état), dessinée plutôt qu'écrite pour ne dépendre d'aucune police.
pub fn dot(ui: &mut egui::Ui, color: Color32) {
    let size = ui.text_style_height(&egui::TextStyle::Body);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), size * 0.32, color);
}

/// Texte d'aide discret sous un champ (retour à la ligne automatique).
pub fn hint(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.add(egui::Label::new(RichText::new(text.into()).weak()).wrap());
}

/// Texte long qui passe à la ligne au lieu d'élargir la fenêtre.
pub fn wrapped(ui: &mut egui::Ui, text: impl Into<RichText>) {
    ui.add(egui::Label::new(text.into()).wrap());
}

/// Texte sur une ligne, tronqué si nécessaire (texte complet au survol).
pub fn truncated(ui: &mut egui::Ui, text: impl Into<RichText>) {
    let text: RichText = text.into();
    let full = text.text().to_string();
    ui.add(egui::Label::new(text).truncate())
        .on_hover_text(full);
}

/// Problèmes de validation dont le champ commence par `prefix`.
pub fn issues_for<'a>(
    issues: &'a [Issue],
    prefix: &'a str,
) -> impl Iterator<Item = &'a Issue> + 'a {
    issues
        .iter()
        .filter(move |i| i.field == prefix || i.field.starts_with(&format!("{prefix}.")))
}

pub fn has_errors_for(issues: &[Issue], prefixes: &[&str]) -> bool {
    prefixes
        .iter()
        .any(|p| issues_for(issues, p).any(|i| i.severity == Severity::Error))
}

/// Affiche les erreurs et avertissements d'un champ.
pub fn show_issues(ui: &mut egui::Ui, issues: &[Issue], field: &str) {
    for i in issues.iter().filter(|i| i.field == field) {
        let (color, prefix) = match i.severity {
            Severity::Error => (error_color(ui), "Erreur : "),
            Severity::Warning => (warn_color(ui), "Attention : "),
        };
        wrapped(
            ui,
            RichText::new(format!("{prefix}{}", i.message)).color(color),
        );
    }
}

/// Bandeau de message (succès, information, avertissement, erreur).
pub fn banner(ui: &mut egui::Ui, color: Color32, text: &str) {
    egui::Frame::group(ui.style())
        .stroke(egui::Stroke::new(1.0, color))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                dot(ui, color);
                wrapped(ui, text);
            });
        });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindMode {
    Local,
    All,
    Specific,
}

pub fn bind_mode_of(bind: &str) -> BindMode {
    match bind.trim() {
        "127.0.0.1" | "::1" => BindMode::Local,
        "0.0.0.0" | "::" => BindMode::All,
        _ => BindMode::Specific,
    }
}

/// Choix de l'adresse d'écoute, expliqué en termes d'exposition réseau.
pub fn bind_editor(ui: &mut egui::Ui, bind: &mut String) {
    let mode = bind_mode_of(bind);
    ui.vertical(|ui| {
        if ui
            .radio(
                mode == BindMode::Local,
                "Ce poste uniquement (127.0.0.1) — recommandé",
            )
            .clicked()
        {
            *bind = "127.0.0.1".into();
        }
        if ui
            .radio(mode == BindMode::All, "Tout le réseau (0.0.0.0)")
            .clicked()
        {
            *bind = "0.0.0.0".into();
        }
        ui.horizontal(|ui| {
            if ui
                .radio(mode == BindMode::Specific, "Adresse précise :")
                .clicked()
                && mode != BindMode::Specific
            {
                bind.clear();
            }
            ui.add_enabled(
                mode == BindMode::Specific,
                egui::TextEdit::singleline(bind)
                    .hint_text("ex. 192.168.1.10")
                    .desired_width(160.0),
            );
        });
    });
}

/// Vérifie que le port peut être ouvert sur l'adresse donnée.
pub fn port_available(bind: &str, port: u16) -> Result<(), String> {
    let ip: std::net::IpAddr = bind
        .trim()
        .parse()
        .map_err(|_| "adresse d'écoute invalide".to_string())?;
    std::net::TcpListener::bind((ip, port))
        .map(|_| ())
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AddrInUse => format!("le port {port} est déjà utilisé sur {ip}"),
            std::io::ErrorKind::AddrNotAvailable => {
                format!("l'adresse {ip} n'appartient pas à ce poste")
            }
            std::io::ErrorKind::PermissionDenied => {
                format!("ouverture du port {port} refusée par le système")
            }
            _ => format!("port {port} indisponible sur {ip} : {e}"),
        })
}

/// Champ texte de chemin facultatif (vide = emplacement standard, affiché en indication).
pub fn path_field(ui: &mut egui::Ui, value: &mut String, default: &std::path::Path) {
    ui.add(
        egui::TextEdit::singleline(value)
            .hint_text(default.display().to_string())
            .desired_width(f32::INFINITY),
    );
}

/// Emplacements par défaut (sans les personnalisations) pour les indications.
pub fn default_locations(
    layout: &Layout,
    cfg: &AppConfig,
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let mut plain = cfg.clone();
    plain.paths = Default::default();
    (
        layout.data_dir.clone(),
        layout.log_dir.clone(),
        layout.web_dir(&plain),
    )
}

/// Libellé du niveau de journalisation.
pub fn level_label(level: &str) -> &'static str {
    match level {
        "error" => "erreurs uniquement",
        "warn" => "avertissements et erreurs",
        "info" => "informations (recommandé)",
        "debug" => "débogage (volumineux)",
        "trace" => "trace complète (très volumineux)",
        _ => "inconnu",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_modes() {
        assert_eq!(bind_mode_of("127.0.0.1"), BindMode::Local);
        assert_eq!(bind_mode_of(" 0.0.0.0 "), BindMode::All);
        assert_eq!(bind_mode_of("10.0.0.5"), BindMode::Specific);
        assert_eq!(bind_mode_of(""), BindMode::Specific);
    }

    #[test]
    fn port_check_detects_busy_port() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(port_available("127.0.0.1", port)
            .unwrap_err()
            .contains("déjà utilisé"));
        drop(l);
        assert!(port_available("127.0.0.1", port).is_ok());
        assert!(port_available("pas-une-ip", port).is_err());
    }

    #[test]
    fn issue_filtering_by_section() {
        let mut cfg = AppConfig::default();
        cfg.server.port = 0;
        let issues = cfg.validate();
        assert!(has_errors_for(&issues, &["server"]));
        assert!(!has_errors_for(&issues, &["logging", "paths"]));
    }
}
