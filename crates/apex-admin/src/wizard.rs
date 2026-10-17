//! Assistant de configuration initiale : lancé par l'installateur (`--wizard`) ou au premier
//! démarrage de l'utilitaire quand aucune configuration n'existe. Chaque étape est validée
//! avant de passer à la suivante ; rien n'est écrit avant la dernière étape.

use std::path::PathBuf;

use apex_config::{AppConfig, Layout, LayoutKind};
use eframe::egui::{self, RichText};

use crate::form;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    Welcome,
    Network,
    Operation,
    Locations,
    Summary,
}

const STEPS: [Step; 5] = [
    Step::Welcome,
    Step::Network,
    Step::Operation,
    Step::Locations,
    Step::Summary,
];

impl Step {
    fn title(self) -> &'static str {
        match self {
            Step::Welcome => "Bienvenue",
            Step::Network => "Instance et réseau",
            Step::Operation => "Exploitation",
            Step::Locations => "Emplacements",
            Step::Summary => "Récapitulatif",
        }
    }

    /// Sections de configuration validées à cette étape.
    fn sections(self) -> &'static [&'static str] {
        match self {
            Step::Welcome | Step::Summary => &[],
            Step::Network => &["instance", "server"],
            Step::Operation => &["history", "security", "safety", "simulation"],
            Step::Locations => &["paths", "database"],
        }
    }

    fn index(self) -> usize {
        STEPS.iter().position(|s| *s == self).unwrap_or(0)
    }
}

/// Ce que l'assistant demande à l'application à la fin.
#[derive(Debug, Clone)]
pub struct Plan {
    pub config: AppConfig,
    /// Démarrage automatique du service (installation système avec service).
    pub autostart: Option<bool>,
    /// Copie de la base d'une version antérieure avant le premier démarrage.
    pub migrate_legacy: Option<PathBuf>,
    pub start_engine: bool,
}

pub enum Outcome {
    Continue,
    Cancel,
    Finish(Box<Plan>),
}

pub struct Wizard {
    step: Step,
    pub cfg: AppConfig,
    autostart: bool,
    /// État du démarrage automatique à l'ouverture : il n'est modifié que si l'utilisateur
    /// change la valeur (chaque modification demande une élévation).
    initial_autostart: Option<bool>,
    start_engine: bool,
    legacy_db: Option<PathBuf>,
    /// Base actuelle déjà présente (moteur déjà démarré une fois) : la reprise la remplacera.
    current_db: Option<PathBuf>,
    migrate_legacy: bool,
    custom_paths: bool,
    port_check: Option<(String, u16, Result<(), String>)>,
    has_service: bool,
    kind: LayoutKind,
}

impl Wizard {
    pub fn new(
        start: AppConfig,
        layout: &Layout,
        has_service: bool,
        legacy_db: Option<PathBuf>,
        current_db: Option<PathBuf>,
    ) -> Self {
        let custom_paths = !start.paths.data_dir.is_empty() || !start.paths.log_dir.is_empty();
        Self {
            step: Step::Welcome,
            cfg: start,
            autostart: true,
            initial_autostart: None,
            start_engine: true,
            migrate_legacy: legacy_db.is_some() && current_db.is_none(),
            legacy_db,
            current_db,
            custom_paths,
            port_check: None,
            has_service,
            kind: layout.kind,
        }
    }

    /// Valeur initiale du démarrage automatique (état actuel du service).
    pub fn set_autostart(&mut self, on: bool) {
        self.autostart = on;
        self.initial_autostart = Some(on);
    }

    /// Affiche l'assistant. `can_write` : la configuration peut-elle être enregistrée ?
    pub fn ui(&mut self, ui: &mut egui::Ui, layout: &Layout, can_write: bool) -> Outcome {
        let issues = self.cfg.validate();
        let mut outcome = Outcome::Continue;

        egui::Panel::top("wizard_steps").show(ui, |ui| {
            ui.add_space(6.0);
            ui.heading("Assistant de configuration initiale");
            ui.horizontal_wrapped(|ui| {
                for (n, s) in STEPS.iter().enumerate() {
                    let text = format!("{}. {}", n + 1, s.title());
                    let rich = if *s == self.step {
                        RichText::new(text).strong()
                    } else {
                        RichText::new(text).weak()
                    };
                    ui.label(rich);
                    if n + 1 < STEPS.len() {
                        ui.label(RichText::new("›").weak());
                    }
                }
            });
            ui.add_space(6.0);
        });

        egui::Panel::bottom("wizard_nav").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Annuler").clicked() {
                    outcome = Outcome::Cancel;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let step_ok = !form::has_errors_for(&issues, self.step.sections());
                    if self.step == Step::Summary {
                        let all_ok = !apex_config::has_errors(&issues);
                        let resp = ui.add_enabled(
                            all_ok && can_write,
                            egui::Button::new(RichText::new("Enregistrer et terminer").strong()),
                        );
                        if resp.clicked() {
                            outcome = Outcome::Finish(Box::new(self.plan()));
                        }
                    } else {
                        let resp = ui.add_enabled(step_ok, egui::Button::new("Suivant ›"));
                        if resp.clicked() {
                            self.step = STEPS[self.step.index() + 1];
                        }
                        if !step_ok {
                            ui.label(
                                RichText::new("Corrigez les erreurs pour continuer")
                                    .color(form::error_color(ui)),
                            );
                        }
                    }
                    if self.step != Step::Welcome && ui.button("‹ Précédent").clicked() {
                        self.step = STEPS[self.step.index() - 1];
                    }
                });
            });
            ui.add_space(6.0);
        });

        egui::CentralPanel::default_margins().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.set_max_width(720.0);
                    match self.step {
                        Step::Welcome => self.welcome(ui, layout, can_write),
                        Step::Network => self.network(ui, &issues),
                        Step::Operation => self.operation(ui, &issues),
                        Step::Locations => self.locations(ui, layout, &issues),
                        Step::Summary => self.summary(ui, layout, &issues, can_write),
                    }
                });
        });
        outcome
    }

    fn plan(&self) -> Plan {
        let mut config = self.cfg.clone();
        if !self.custom_paths {
            config.paths.data_dir.clear();
            config.paths.log_dir.clear();
        }
        Plan {
            config,
            autostart: (self.has_service && self.initial_autostart != Some(self.autostart))
                .then_some(self.autostart),
            migrate_legacy: if self.migrate_legacy {
                self.legacy_db.clone()
            } else {
                None
            },
            start_engine: self.start_engine,
        }
    }

    fn welcome(&mut self, ui: &mut egui::Ui, layout: &Layout, can_write: bool) {
        ui.add_space(8.0);
        ui.label(
            "Cet assistant prépare la configuration du moteur Apex Supervision avant son premier démarrage : \
             nom de l'instance, accès réseau, mode d'exploitation et emplacements des données.",
        );
        ui.add_space(4.0);
        ui.label("Vous pourrez modifier chaque paramètre plus tard depuis l'onglet Configuration de cet utilitaire.");
        ui.add_space(12.0);
        egui::Grid::new("wizard_welcome")
            .num_columns(2)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                ui.label("Mode :");
                ui.label(match layout.kind {
                    LayoutKind::System => "installation système (service)",
                    LayoutKind::Portable => "portable (tout dans un dossier, lancement manuel)",
                });
                ui.end_row();
                ui.label("Fichier de configuration :");
                form::wrapped(
                    ui,
                    RichText::new(layout.config_file.display().to_string()).monospace(),
                );
                ui.end_row();
            });
        ui.add_space(12.0);
        if layout.config_file.exists() {
            form::banner(
                ui,
                form::warn_color(ui),
                "Une configuration existe déjà : l'assistant part de ses valeurs, et l'ancienne version sera sauvegardée.",
            );
        }
        if !can_write {
            form::banner(
                ui,
                form::error_color(ui),
                "Vous n'avez pas le droit de modifier cette configuration. Relancez l'utilitaire en administrateur \
                 (bouton en haut de la fenêtre) pour l'enregistrer.",
            );
        }
        if let Some(db) = &self.legacy_db {
            form::banner(
                ui,
                form::info_color(ui),
                &format!(
                    "Une base d'une version antérieure a été trouvée ({}). Vous pourrez choisir d'en reprendre les données à l'étape « Emplacements ».",
                    db.display()
                ),
            );
        }
    }

    fn network(&mut self, ui: &mut egui::Ui, issues: &[apex_config::Issue]) {
        ui.add_space(8.0);
        ui.strong("Nom de l'instance");
        ui.add(egui::TextEdit::singleline(&mut self.cfg.instance.name).desired_width(320.0));
        form::hint(
            ui,
            "Affiché dans l'IHM web, les journaux et cet utilitaire (ex. « Bâtiment A — CTA »).",
        );
        form::show_issues(ui, issues, "instance.name");

        ui.add_space(14.0);
        ui.strong("Accès à l'IHM web");
        form::bind_editor(ui, &mut self.cfg.server.bind);
        form::hint(
            ui,
            "Le serveur parle HTTP non chiffré : pour un accès depuis le réseau, limitez-le par pare-feu ou placez-le derrière un proxy HTTPS.",
        );
        form::show_issues(ui, issues, "server.bind");

        ui.add_space(14.0);
        ui.strong("Port web");
        ui.horizontal(|ui| {
            ui.add(egui::DragValue::new(&mut self.cfg.server.port).range(1..=65535u16));
            if ui.button("Vérifier la disponibilité").clicked() {
                let r = form::port_available(&self.cfg.server.bind, self.cfg.server.port);
                self.port_check = Some((self.cfg.server.bind.clone(), self.cfg.server.port, r));
            }
            if let Some((bind, port, r)) = &self.port_check {
                if *bind == self.cfg.server.bind && *port == self.cfg.server.port {
                    match r {
                        Ok(()) => ui.colored_label(form::ok_color(ui), "Port libre"),
                        Err(e) => ui.colored_label(form::error_color(ui), e),
                    };
                }
            }
        });
        form::hint(
            ui,
            "Par défaut 3000. L'adresse de l'IHM sera http://<poste>:<port>/.",
        );
        form::show_issues(ui, issues, "server.port");
    }

    fn operation(&mut self, ui: &mut egui::Ui, issues: &[apex_config::Issue]) {
        ui.add_space(8.0);
        ui.strong("Lancement du moteur");
        if self.has_service {
            ui.radio_value(
                &mut self.autostart,
                true,
                "Démarrer automatiquement avec le système (service) — recommandé",
            );
            ui.radio_value(
                &mut self.autostart,
                false,
                "Démarrage manuel du service (depuis cet utilitaire)",
            );
            form::hint(
                ui,
                "Le service fonctionne en arrière-plan, sans session ouverte.",
            );
        } else if self.kind == LayoutKind::System {
            form::banner(
                ui,
                form::warn_color(ui),
                "Le service système n'est pas installé : réinstallez le logiciel pour un démarrage automatique.",
            );
        } else {
            ui.label("Mode portable : le moteur est lancé et arrêté depuis cet utilitaire (pas de service système).");
        }

        ui.add_space(14.0);
        ui.strong("Sécurité des équipements");
        ui.checkbox(&mut self.cfg.safety.safe_mode, "Mode sûr (mise en service)");
        form::hint(
            ui,
            "Aucune commande n'est envoyée aux équipements réels : la supervision fonctionne, mais le planificateur \
             et les sorties des flux sont bloqués. Désactivez-le une fois l'installation vérifiée.",
        );
        ui.add_space(6.0);
        ui.checkbox(
            &mut self.cfg.simulation.enabled,
            "Simulation et données de démonstration",
        );
        form::hint(
            ui,
            "Automate Modbus embarqué, simulateur GTB et données de démonstration. \
             À réserver à la démonstration et à la formation : désactivez-la en production.",
        );
        if self.cfg.simulation.enabled {
            ui.horizontal(|ui| {
                ui.label("Port de l'automate embarqué :");
                ui.add(
                    egui::DragValue::new(&mut self.cfg.simulation.modbus_slave_port)
                        .range(1..=65535u16),
                );
            });
            form::show_issues(ui, issues, "simulation.modbus_slave_port");
        }

        ui.add_space(14.0);
        ui.strong("Conservation et sessions");
        egui::Grid::new("wizard_operation")
            .num_columns(2)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                ui.label("Historique des mesures :");
                ui.add(
                    egui::DragValue::new(&mut self.cfg.history.retention_days)
                        .range(0..=3650u32)
                        .suffix(" jours"),
                );
                ui.end_row();
                ui.label("");
                form::hint(
                    ui,
                    "0 = conservation illimitée (la base grossit sans limite).",
                );
                ui.end_row();
                ui.label("Expiration des sessions :");
                ui.add(
                    egui::DragValue::new(&mut self.cfg.security.session_ttl_hours)
                        .range(1..=720u32)
                        .suffix(" h"),
                );
                ui.end_row();
            });
        form::show_issues(ui, issues, "history.retention_days");
        form::show_issues(ui, issues, "security.session_ttl_hours");
        ui.add_space(10.0);
        form::hint(
            ui,
            "Comptes initiaux de l'IHM web : admin / superviseur / visu (mots de passe d'usine). \
             Changez-les à la première connexion : le moteur le rappelle tant que ce n'est pas fait.",
        );
    }

    fn locations(&mut self, ui: &mut egui::Ui, layout: &Layout, issues: &[apex_config::Issue]) {
        ui.add_space(8.0);
        let (data, logs, _) = form::default_locations(layout, &self.cfg);
        egui::Grid::new("wizard_locations")
            .num_columns(2)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                ui.label("Données (base, historique) :");
                form::wrapped(ui, RichText::new(data.display().to_string()).monospace());
                ui.end_row();
                ui.label("Journaux :");
                form::wrapped(ui, RichText::new(logs.display().to_string()).monospace());
                ui.end_row();
            });
        form::hint(
            ui,
            "Ces emplacements sont conservés lors des mises à jour et de la désinstallation.",
        );
        ui.add_space(10.0);
        ui.checkbox(
            &mut self.custom_paths,
            "Utiliser d'autres emplacements (chemins absolus)",
        );
        if self.custom_paths {
            egui::Grid::new("wizard_custom_paths")
                .num_columns(2)
                .spacing([16.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Données :");
                    form::path_field(ui, &mut self.cfg.paths.data_dir, &data);
                    ui.end_row();
                    ui.label("Journaux :");
                    form::path_field(ui, &mut self.cfg.paths.log_dir, &logs);
                    ui.end_row();
                });
            form::show_issues(ui, issues, "paths.data_dir");
            form::show_issues(ui, issues, "paths.log_dir");
            if self.kind == LayoutKind::System {
                form::hint(
                    ui,
                    if cfg!(windows) {
                        "Le compte du service (LocalService) doit pouvoir écrire dans ces dossiers : \
                         accordez-lui le droit Modification si vous choisissez un emplacement hors des \
                         dossiers standard."
                    } else {
                        "Le service systemd ne peut écrire que dans ses dossiers standard : pour un autre \
                         emplacement, donnez-le à l'utilisateur apex-scada ET déclarez-le avec \
                         « systemctl edit apex-scada » → [Service] ReadWritePaths=<dossier> \
                         (voir INSTALLATION.md)."
                    },
                );
            }
        }

        if let Some(db) = &self.legacy_db {
            ui.add_space(14.0);
            ui.strong("Version antérieure");
            ui.checkbox(
                &mut self.migrate_legacy,
                format!("Reprendre les données de « {} »", db.display()),
            );
            form::hint(
                ui,
                "La base est COPIÉE avant le premier démarrage (configuration, historique, comptes) ; \
                 l'original n'est jamais modifié.",
            );
            if let Some(current) = &self.current_db {
                form::banner(
                    ui,
                    form::warn_color(ui),
                    &format!(
                        "Une base existe déjà ({}). Si vous cochez la reprise, elle sera d'abord sauvegardée \
                         en entier (fichier .bak-<date> à côté), puis remplacée. Ne cochez que si elle ne \
                         contient encore aucune donnée utile.",
                        current.display()
                    ),
                );
            }
        }
    }

    fn summary(
        &mut self,
        ui: &mut egui::Ui,
        layout: &Layout,
        issues: &[apex_config::Issue],
        can_write: bool,
    ) {
        ui.add_space(8.0);
        let plan = self.plan();
        let c = &plan.config;
        let (data, logs, _) = form::default_locations(layout, c);
        let yes_no = |b: bool| if b { "oui" } else { "non" };
        egui::Grid::new("wizard_summary")
            .num_columns(2)
            .striped(true)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                let mut row = |k: &str, v: String| {
                    ui.label(k);
                    ui.label(v);
                    ui.end_row();
                };
                row("Instance", c.instance.name.clone());
                row("Adresse de l'IHM", crate::probe::web_url(c));
                row("Écoute", format!("{}:{}", c.server.bind, c.server.port));
                if let Some(a) = plan.autostart {
                    row("Démarrage automatique", yes_no(a).into());
                }
                row("Mode sûr", yes_no(c.safety.safe_mode).into());
                row("Simulation", yes_no(c.simulation.enabled).into());
                row(
                    "Historique",
                    if c.history.retention_days == 0 {
                        "illimité".into()
                    } else {
                        format!("{} jours", c.history.retention_days)
                    },
                );
                row(
                    "Données",
                    if c.paths.data_dir.is_empty() {
                        data.display().to_string()
                    } else {
                        c.paths.data_dir.clone()
                    },
                );
                row(
                    "Journaux",
                    if c.paths.log_dir.is_empty() {
                        logs.display().to_string()
                    } else {
                        c.paths.log_dir.clone()
                    },
                );
                if let Some(db) = &plan.migrate_legacy {
                    row("Reprise des données", db.display().to_string());
                }
            });
        ui.add_space(10.0);
        for i in issues {
            let color = match i.severity {
                apex_config::Severity::Error => form::error_color(ui),
                apex_config::Severity::Warning => form::warn_color(ui),
            };
            ui.colored_label(color, i.to_string());
        }
        ui.add_space(10.0);
        ui.checkbox(
            &mut self.start_engine,
            "Démarrer le moteur à la fin de l'assistant",
        );
        ui.add_space(6.0);
        form::hint(
            ui,
            format!(
                "La configuration sera écrite dans « {} »{}.",
                layout.config_file.display(),
                if layout.config_file.exists() {
                    " (l'ancienne version est sauvegardée)"
                } else {
                    ""
                }
            ),
        );
        if !can_write {
            ui.colored_label(
                form::error_color(ui),
                "Droits insuffisants pour enregistrer : relancez l'utilitaire en administrateur.",
            );
        }
    }
}
