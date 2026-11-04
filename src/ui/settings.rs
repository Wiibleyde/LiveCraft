//! The Settings window, one tab per subject.

use super::*;
use crate::api::{HTTP_PORT, TCP_PORT};

const TABS: [&str; 11] = [
    "Format",
    "Options",
    "Sorties",
    "Enregistrement",
    "Diffusion",
    "Audio",
    "Raccourcis",
    "Scripts",
    "Données",
    "Contrôle web",
    "À propos",
];

const FORMATS: [(&str, usize, usize); 6] = [
    ("480p", 854, 480),
    ("720p", 1280, 720),
    ("1080p", 1920, 1080),
    ("UHD", 3840, 2160),
    ("4K DCI", 4096, 2160),
    ("Vertical HD", 1080, 1920),
];

impl App {
    pub(super) fn settings_window(&mut self, ctx: &egui::Context, project: &Project) {
        if !self.settings_open {
            return;
        }
        let mut open = true;
        let mut p = project.clone();
        egui::Window::new("Réglages").open(&mut open).default_size([680.0, 480.0]).show(ctx, |ui| {
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(110.0);
                    for (n, name) in TABS.iter().enumerate() {
                        ui.selectable_value(&mut self.settings_tab, n, *name);
                    }
                });
                ui.separator();
                ui.vertical(|ui| {
                    egui::ScrollArea::vertical().max_height(500.0).show(ui, |ui| {
                        ui.set_min_width(500.0);
                        match self.settings_tab {
                            0 => self.tab_format(ui, &p),
                            1 => tab_options(ui, &mut p),
                            2 => self.tab_outputs(ui, &mut p),
                            3 => self.tab_recording(ui, &mut p),
                            4 => self.tab_streaming(ui, &mut p),
                            5 => self.tab_audio_devices(ui, &mut p),
                            6 => tab_shortcuts(ui, &mut p),
                            7 => self.tab_scripts(ui, &mut p),
                            8 => tab_data(ui, &mut p),
                            9 => self.tab_remote(ui),
                            _ => tab_about(ui),
                        }
                    });
                });
            });
        });
        self.settings_open = open;
        if p != *project {
            // Only what this window edits: the desk may have moved on since
            // the start of the frame, and that must not be undone.
            self.with(|s| {
                let d = &mut s.project;
                d.quick_play = p.quick_play;
                d.show_clocks = p.show_clocks;
                d.fullscreen_route = p.fullscreen_route;
                d.fullscreen_monitor = p.fullscreen_monitor;
                d.srt = p.srt;
                d.records = p.records;
                d.streams = p.streams;
                d.master_device = p.master_device;
                d.headphones_device = p.headphones_device;
                d.ftb_audio = p.ftb_audio;
                for (bus, edited) in d.buses.iter_mut().zip(&p.buses) {
                    bus.device.clone_from(&edited.device);
                }
                d.shortcuts = p.shortcuts;
                d.scripts = p.scripts;
                d.data_sources = p.data_sources;
            });
        }
    }

    fn tab_format(&mut self, ui: &mut egui::Ui, p: &Project) {
        let e = &self.engine;
        let busy = e.is_recording() || e.is_streaming() || e.is_multi() || e.is_srt() || e.is_replay();
        ui.label(RichText::new("Format de sortie").strong());
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal_wrapped(|ui| {
                for (label, w, h) in FORMATS {
                    let on = (self.format_draft.0, self.format_draft.1) == (w, h);
                    if ui.selectable_label(on, label).clicked() {
                        self.format_draft.0 = w;
                        self.format_draft.1 = h;
                    }
                }
            });
            ui.horizontal(|ui| {
                for fps in [24, 25, 30, 50, 60] {
                    ui.selectable_value(&mut self.format_draft.2, fps, format!("{fps} i/s"));
                }
            });
            if self.format_draft != (p.out_w, p.out_h, p.fps) && ui.button("Appliquer").clicked() {
                let (w, h, fps) = self.format_draft;
                self.engine.set_format(w, h, fps);
                self.thumbs.clear();
            }
        });
        if busy {
            ui.label(RichText::new("Arrêtez les sorties pour changer le format").small().color(DIM));
        }
    }

    fn tab_outputs(&mut self, ui: &mut egui::Ui, p: &mut Project) {
        ui.label(RichText::new("Plein écran").strong());
        for n in 0..2 {
            ui.horizontal(|ui| {
                let label = format!("Écran {}", n + 1);
                ui.checkbox(&mut self.windows_on[n], label);
                let count = self.screens.len().max(1);
                ui.add(
                    egui::DragValue::new(&mut p.fullscreen_monitor[n]).range(1..=count).prefix("moniteur "),
                );
                let route = p.fullscreen_route[n];
                let mut picked = route;
                let shown = route_name(route, p);
                egui::ComboBox::from_id_salt(("fs", n)).selected_text(shown).show_ui(ui, |ui| {
                    self.route_list(ui, &mut picked, p);
                });
                p.fullscreen_route[n] = picked;
            });
        }
        ui.separator();
        ui.label(RichText::new("Sortie externe (fenêtre sur un autre écran)").strong());
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.windows_on[2], "Active");
            let mut picked = self.external_route;
            let shown = route_name(picked, p);
            egui::ComboBox::from_id_salt("ext").selected_text(shown).show_ui(ui, |ui| {
                self.route_list(ui, &mut picked, p);
            });
            self.external_route = picked;
        });
        ui.separator();
        ui.label(RichText::new("Sorties SRT").strong());
        for (n, o) in p.srt.iter_mut().enumerate() {
            ui.push_id(n, |ui| {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut o.enabled, format!("{}", n + 1));
                    ui.add(egui::TextEdit::singleline(&mut o.url).desired_width(260.0));
                    ui.add(egui::DragValue::new(&mut o.kbps).range(500..=50000).suffix(" kb/s"));
                });
            });
        }
        let running = self.engine.is_srt();
        if ui.button(if running { "Arrêter le SRT" } else { "Démarrer le SRT" }).clicked() {
            if running && self.with(|s| s.padlock) {
                self.say("Verrouillé : ouvrez le cadenas pour arrêter");
            } else {
                let name = if running { "StopSRTOutput" } else { "StartSRTOutput" };
                self.call(name, &Args::new());
            }
        }
    }

    fn codec_combo(&self, ui: &mut egui::Ui, salt: &str, codec: &mut String) {
        let list = self.encoders.lock().unwrap().clone();
        egui::ComboBox::from_id_salt(salt).selected_text(codec_label(codec)).show_ui(ui, |ui| {
            for e in &list {
                ui.selectable_value(codec, e.clone(), codec_label(e));
            }
        });
    }

    fn tab_recording(&mut self, ui: &mut egui::Ui, p: &mut Project) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(&self.saved.record_dir).small());
            if ui.button("Dossier…").clicked()
                && let Some(d) = rfd::FileDialog::new().pick_folder()
            {
                self.saved.record_dir = d.to_string_lossy().into_owned();
            }
        });
        for n in 0..2 {
            let r = &mut p.records[n];
            ui.separator();
            ui.push_id(n, |ui| {
                ui.checkbox(&mut r.enabled, format!("Enregistreur {}", n + 1));
                ui.horizontal(|ui| {
                    for c in ["mp4", "mkv", "mov", "ts"] {
                        ui.selectable_value(&mut r.container, c.to_string(), c);
                    }
                    self.codec_combo(ui, "codec", &mut r.codec);
                });
                ui.add(egui::Slider::new(&mut r.video_kbps, 1000..=100000).text("Vidéo (kb/s)"));
                ui.add(egui::Slider::new(&mut r.audio_kbps, 64..=320).text("Audio (kb/s)"));
                ui.add(
                    egui::Slider::new(&mut r.split_minutes, 0..=240).text("Nouveau fichier toutes les (min)"),
                );
                audio_pick(ui, "audio", &mut r.audio);
            });
        }
    }

    fn tab_streaming(&mut self, ui: &mut egui::Ui, p: &mut Project) {
        ui.label(RichText::new("Jusqu'à cinq destinations en même temps").small().color(DIM));
        for n in 0..5 {
            let s = &mut p.streams[n];
            ui.separator();
            ui.push_id(n, |ui| {
                ui.checkbox(&mut s.enabled, format!("Destination {}", n + 1));
                ui.horizontal(|ui| {
                    ui.label("Serveur");
                    ui.add(egui::TextEdit::singleline(&mut s.url).desired_width(300.0));
                });
                ui.horizontal(|ui| {
                    ui.label("Clé");
                    ui.add(egui::TextEdit::singleline(&mut s.key).password(true).desired_width(300.0));
                });
                if n > 0 {
                    ui.checkbox(&mut s.shared, "Même qualité que la destination 1 (un seul encodage)");
                }
                if n == 0 || !s.shared {
                    ui.add(egui::Slider::new(&mut s.kbps, 500..=50000).text("Vidéo (kb/s)"));
                    self.codec_combo(ui, "codec", &mut s.codec);
                }
            });
        }
    }

    fn tab_audio_devices(&mut self, ui: &mut egui::Ui, p: &mut Project) {
        let devices = self.engine.audio_devices.lock().unwrap().clone();
        device_combo(ui, "Général", &mut p.master_device, &devices);
        device_combo(ui, "Casque (vide : sortie par défaut)", &mut p.headphones_device, &devices);
        ui.separator();
        for (n, name) in ["A", "B", "C", "D", "E", "F", "G"].iter().enumerate() {
            device_combo(ui, &format!("Bus {name}"), &mut p.buses[n].device, &devices);
        }
        ui.separator();
        ui.checkbox(&mut p.ftb_audio, "Le fondu au noir coupe aussi le son");
    }

    fn tab_scripts(&mut self, ui: &mut egui::Ui, p: &mut Project) {
        ui.label(
            RichText::new(
                "API.Function(\"Fade\",Input:=\"1\",Duration:=1000), Sleep(1000), For/Next, Do/Loop",
            )
            .small()
            .color(DIM),
        );
        let mut remove = None;
        let mut run = None;
        for (n, s) in p.scripts.iter_mut().enumerate() {
            ui.push_id(n, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut s.name).desired_width(160.0));
                    if ui.button("Lancer").clicked() {
                        run = Some(s.code.clone());
                    }
                    if ui.button("Retirer").clicked() {
                        remove = Some(n);
                    }
                });
                ui.add(
                    egui::TextEdit::multiline(&mut s.code).code_editor().desired_rows(5).desired_width(480.0),
                );
            });
        }
        if let Some(n) = remove {
            p.scripts.remove(n);
        }
        if let Some(code) = run {
            self.engine.run_script(code);
        }
        ui.horizontal(|ui| {
            if ui.button("Ajouter un script").clicked() {
                p.scripts
                    .push(Script { name: format!("Script {}", p.scripts.len() + 1), code: String::new() });
            }
            if ui.button("Tout arrêter").clicked() {
                self.call("ScriptStopAll", &Args::new());
            }
        });
    }

    fn tab_remote(&mut self, ui: &mut egui::Ui) {
        ui.label(format!("Contrôle web et API sur le port {HTTP_PORT}, API TCP sur le port {TCP_PORT}."));
        ui.label(RichText::new(format!("Depuis cet ordinateur : http://127.0.0.1:{HTTP_PORT}/")).small());
        ui.checkbox(&mut self.saved.remote_lan, "Accessible depuis le réseau local");
        ui.horizontal(|ui| {
            ui.label("Mot de passe");
            ui.add(egui::TextEdit::singleline(&mut self.saved.remote_password).password(true));
        });
        ui.label(RichText::new("Ces deux réglages prennent effet au prochain démarrage").small().color(DIM));
    }
}

fn route_name(route: Route, p: &Project) -> String {
    match route {
        Route::Output => "Sortie".into(),
        Route::Preview => "Aperçu".into(),
        Route::MultiView => "MultiView".into(),
        Route::MultiView2 => "MultiView 2".into(),
        Route::Input(id) => p.inputs.iter().find(|i| i.id == id).map_or("—".into(), |i| i.name.clone()),
    }
}

pub(super) fn audio_pick(ui: &mut egui::Ui, salt: &str, pick: &mut AudioPick) {
    ui.horizontal(|ui| {
        ui.label("Son");
        egui::ComboBox::from_id_salt(salt).selected_text(pick.label()).show_ui(ui, |ui| {
            for (name, value) in AudioPick::all() {
                ui.selectable_value(pick, value, name);
            }
        });
    });
}

fn tab_options(ui: &mut egui::Ui, p: &mut Project) {
    ui.label(RichText::new("Passer au direct").strong());
    ui.horizontal(|ui| {
        let t = &mut p.quick_play;
        egui::ComboBox::from_id_salt("rapide").selected_text(t.kind.label()).show_ui(ui, |ui| {
            for k in TransKind::ALL {
                ui.selectable_value(&mut t.kind, k, k.label());
            }
        });
        ui.add(egui::DragValue::new(&mut t.ms).range(50..=5000).suffix(" ms"));
    });
    ui.separator();
    ui.checkbox(&mut p.show_clocks, "Horloges de production");
}

fn tab_shortcuts(ui: &mut egui::Ui, p: &mut Project) {
    ui.label(
        RichText::new("Touche, fonction (Cut, Fade, OverlayInput1…), entrée, valeur").small().color(DIM),
    );
    let mut remove = None;
    for (n, s) in p.shortcuts.iter_mut().enumerate() {
        ui.push_id(n, |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut s.key).hint_text("F1").desired_width(60.0));
                ui.add(
                    egui::TextEdit::singleline(&mut s.function).hint_text("Fonction").desired_width(160.0),
                );
                ui.add(egui::TextEdit::singleline(&mut s.input).hint_text("Entrée").desired_width(80.0));
                ui.add(egui::TextEdit::singleline(&mut s.value).hint_text("Valeur").desired_width(80.0));
                if ui.small_button("Retirer").clicked() {
                    remove = Some(n);
                }
            });
        });
    }
    if let Some(n) = remove {
        p.shortcuts.remove(n);
    }
    if ui.button("Ajouter un raccourci").clicked() {
        p.shortcuts.push(Shortcut::default());
    }
}

fn tab_data(ui: &mut egui::Ui, p: &mut Project) {
    ui.label(RichText::new("Fichiers CSV ou texte reliés aux titres").small().color(DIM));
    let mut remove = None;
    for (n, d) in p.data_sources.iter_mut().enumerate() {
        ui.push_id(n, |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(120.0));
                if ui.button("Fichier…").clicked()
                    && let Some(f) = rfd::FileDialog::new().pick_file()
                {
                    d.path = f.to_string_lossy().into_owned();
                }
                ui.add(egui::DragValue::new(&mut d.row).prefix("ligne "));
                ui.add(egui::DragValue::new(&mut d.auto_next_secs).range(0.0..=600.0).suffix(" s"));
                if ui.small_button("Retirer").clicked() {
                    remove = Some(n);
                }
            });
        });
    }
    if let Some(n) = remove {
        p.data_sources.remove(n);
    }
    if ui.button("Ajouter une source").clicked() {
        p.data_sources.push(DataSource {
            name: format!("Source {}", p.data_sources.len() + 1),
            ..DataSource::default()
        });
    }
}

fn tab_about(ui: &mut egui::Ui) {
    ui.label(RichText::new("Régie").strong());
    ui.label("Mélangeur vidéo en direct, reconstruit à partir de la documentation publique.");
    ui.label(
        RichText::new("Les vidéos, caméras et sorties passent par ffmpeg, à installer séparément.")
            .small()
            .color(DIM),
    );
}
