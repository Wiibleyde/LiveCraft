//! Top bar, footer bar and status line, with what their buttons start.

use super::*;

impl App {
    pub(super) fn top_bar(&mut self, ui: &mut egui::Ui) {
        let full = ui.available_rect_before_wrap();
        let locked = self.with(|s| s.padlock);
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            let shown = match (&self.current_file, &self.imported) {
                (Some(f), _) => file_name(std::path::Path::new(f)),
                (None, Some(name)) => name.clone(),
                _ => "Préréglage".to_string(),
            };
            ui.label(RichText::new(shown).strong().color(TEXT));
            ui.add_space(8.0);
            if ui.add_enabled_ui(!locked, |ui| bar_button(ui, "Nouveau", false)).inner.clicked() {
                self.confirm_new = true;
            }
            let open = ui.add_enabled_ui(!locked, |ui| {
                let b = egui::Button::new(RichText::new("Ouvrir").size(12.0).color(TEXT))
                    .fill(BTN)
                    .min_size(vec2(0.0, 22.0));
                egui::containers::menu::MenuButton::from_button(b).ui(ui, |ui| {
                    ui.set_min_width(180.0);
                    if ui.button("Ouvrir…").clicked() {
                        self.open_project(false);
                        ui.close();
                    }
                    if ui.button("Ajouter les entrées d'un projet…").clicked() {
                        self.open_project(true);
                        ui.close();
                    }
                    if ui.button("Ouvrir le projet d'exemple").clicked() {
                        self.engine.load_project(Project::sample());
                        self.after_load(None, None);
                        ui.close();
                    }
                });
            });
            drop(open);
            if bar_button(ui, "Enregistrer", false).clicked() {
                self.save_project(false);
            }
            if bar_button(ui, "Enregistrer sous", false).clicked() {
                self.save_project(true);
            }
            if ui.add_enabled_ui(!locked, |ui| bar_button(ui, "Dernier", false)).inner.clicked() {
                self.open_last();
            }
            let undo = self.with(|s| s.undo.as_ref().map(|c| c.input.name.clone()));
            if let Some(name) = undo {
                let b = bar_button(ui, "Annuler", false);
                if b.on_hover_text(format!("Rouvrir « {name} »")).clicked() {
                    self.engine.undo();
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                if bar_button(ui, "?", self.help_open).clicked() {
                    self.help_open = !self.help_open;
                }
                if bar_button(ui, "Réglages", self.settings_open).clicked() {
                    self.settings_open = !self.settings_open;
                }
                let label = if self.basic { "Avancé" } else { "Simple" };
                let tip = "Masque ou montre les boutons des entrées et le mixeur";
                if bar_button(ui, label, false).on_hover_text(tip).clicked() {
                    self.basic = !self.basic;
                }
                let paused = self.with(|s| s.inputs_paused);
                if bar_button(ui, "Pause entrées", paused).clicked() {
                    self.engine.set_inputs_paused(!paused);
                }
            });
        });
        let center = Rect::from_center_size(full.center(), vec2(MID_W, full.height()));
        let builder = egui::UiBuilder::new()
            .id_salt("plein-ecran")
            .max_rect(center)
            .layout(egui::Layout::left_to_right(egui::Align::Center));
        let mut mid = ui.new_child(builder);
        mid.spacing_mut().item_spacing.x = 1.0;
        if bar_button(&mut mid, "Plein écran", self.windows_on[0]).clicked() {
            self.windows_on[0] = !self.windows_on[0];
            self.screens = tools::list_screens();
        }
        let open = egui::Button::new("").fill(BTN).min_size(vec2(16.0, 22.0));
        let menu = egui::containers::menu::MenuButton::from_button(open);
        let project = self.with(|s| s.project.clone());
        let (resp, _) = menu.ui(&mut mid, |ui| {
            ui.set_min_width(170.0);
            let mut route = project.fullscreen_route[0];
            self.route_list(ui, &mut route, &project);
            if route != project.fullscreen_route[0] {
                self.with(|s| s.project.fullscreen_route[0] = route);
            }
        });
        draw_down_arrow(&mid, resp.rect.center());
    }

    pub(super) fn route_list(&self, ui: &mut egui::Ui, route: &mut Route, project: &Project) {
        ui.selectable_value(route, Route::Output, "Sortie");
        ui.selectable_value(route, Route::Preview, "Aperçu");
        ui.selectable_value(route, Route::MultiView, "MultiView");
        ui.selectable_value(route, Route::MultiView2, "MultiView 2 (entrées)");
        ui.separator();
        for i in project.inputs.iter().filter(|i| i.kind.has_picture()) {
            ui.selectable_value(route, Route::Input(i.id), &i.name);
        }
    }

    fn open_project(&mut self, append: bool) {
        let picked =
            rfd::FileDialog::new().add_filter("Projets", &["livecraft", "regie", "vmix"]).pick_file();
        let Some(path) = picked else { return };
        let path = path.to_string_lossy().into_owned();
        if append {
            match read_any(&path) {
                Ok(p) => self.engine.append_project(p),
                Err(e) => self.say(format!("Projet illisible : {e}")),
            }
        } else {
            self.open_path(path);
        }
    }

    pub(super) fn open_path(&mut self, path: String) {
        if path.to_lowercase().ends_with(".vmix") {
            self.open_vmix(path);
            return;
        }
        match read_project(&path) {
            Ok(p) => {
                self.engine.load_project(p);
                self.saved.last_file = Some(path.clone());
                self.after_load(Some(path), None);
            }
            Err(e) => self.say(format!("Projet illisible : {e}")),
        }
    }

    /// Nothing from the previous project may linger on the new one, which
    /// reuses the same input numbers.
    pub(super) fn after_load(&mut self, file: Option<String>, imported: Option<String>) {
        self.current_file = file;
        self.imported = imported;
        self.format_draft = self.with(|s| (s.project.out_w, s.project.out_h, s.project.fps));
        self.thumbs.clear();
        self.editing = None;
        self.ptz_for = None;
        self.telestrator_for = None;
        self.designer_for = None;
        self.pending_restart = None;
        self.tab = None;
        self.selected_layer = 0;
        self.multi_ids.clear();
        self.replay_cams.clear();
        self.external_route = Route::Output;
    }

    /// A .vmix opens as a copy: saving always asks for a new .livecraft file.
    fn open_vmix(&mut self, path: String) {
        match crate::vmix_import::load(&path) {
            Ok((p, report)) => {
                self.engine.load_project(p);
                self.saved.last_file = Some(path);
                self.after_load(None, Some(report.name.clone()));
                self.import_report = Some(report);
            }
            Err(e) => self.say(e),
        }
    }

    fn open_last(&mut self) {
        match self.saved.last_file.clone() {
            Some(p) => self.open_path(p),
            None => self.say("Aucun projet récent"),
        }
    }

    fn save_project(&mut self, ask: bool) {
        let target = match (&self.current_file, ask) {
            (Some(p), false) => Some(p.clone()),
            _ => rfd::FileDialog::new()
                .add_filter("Projet LiveCraft", &["livecraft"])
                .set_file_name("production.livecraft")
                .save_file()
                .map(|p| p.to_string_lossy().into_owned()),
        };
        let Some(path) = target else { return };
        let project = self.with(|s| s.project.clone()).without_secrets();
        let done = serde_json::to_vec_pretty(&project)
            .map_err(|e| e.to_string())
            .and_then(|j| std::fs::write(&path, j).map_err(|e| e.to_string()));
        match done {
            Ok(()) => {
                self.saved.last_file = Some(path.clone());
                self.current_file = Some(path);
                self.say("Projet enregistré");
            }
            Err(e) => self.say(format!("Échec de l'enregistrement : {e}")),
        }
    }

    pub(super) fn output_bar(&mut self, ui: &mut egui::Ui) {
        let locked = self.with(|s| s.padlock);
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            self.add_menu(ui);
            ui.add_space(14.0);
            let rec = *self.engine.record_started.lock().unwrap();
            let stream = *self.engine.stream_started.lock().unwrap();
            let recording = self.engine.is_recording();
            let streaming = self.engine.is_streaming();
            let multi = self.engine.is_multi();
            let timed = |name: &str, since: Option<Instant>| match since {
                Some(t) => {
                    let s = t.elapsed().as_secs();
                    format!("{name}  {:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
                }
                None => name.to_string(),
            };
            let (g, m) = gear_button(ui, &timed("Enregistrer", rec), recording);
            if g {
                self.open_settings(3);
            }
            if m {
                if recording {
                    if locked {
                        self.say("Verrouillé : ouvrez le cadenas pour arrêter");
                    } else {
                        self.engine.stop_recording();
                        self.say("Enregistrement terminé");
                    }
                } else {
                    match self.engine.start_recording(&self.saved.record_dir.clone()) {
                        Ok(p) => self.say(format!("Enregistrement vers {p}")),
                        Err(e) => self.say(e),
                    }
                }
            }
            let (g, m) = gear_button(ui, "Externe", self.windows_on[2]);
            if g {
                self.open_settings(2);
            }
            if m {
                self.windows_on[2] = !self.windows_on[2];
            }
            let (g, m) = gear_button(ui, &timed("Diffuser", stream), streaming);
            if g {
                self.open_settings(4);
            }
            if m {
                if streaming {
                    if locked {
                        self.say("Verrouillé : ouvrez le cadenas pour arrêter");
                    } else {
                        self.engine.stop_streaming();
                    }
                } else if let Err(e) = self.engine.start_streaming() {
                    self.open_settings(4);
                    self.say(e);
                }
            }
            let (g, m) = gear_button(ui, "Multi-enregistrement", multi);
            self.multi_open |= g;
            if m {
                if multi {
                    if locked {
                        self.say("Verrouillé : ouvrez le cadenas pour arrêter");
                    } else {
                        self.engine.stop_multi();
                        self.say("Multi-enregistrement terminé");
                    }
                } else {
                    self.multi_open = true;
                }
            }
            let playlist_on = self.with(|s| s.playlist_on);
            let (g, m) = gear_button(ui, "Playlist", playlist_on);
            self.playlist_open |= g;
            if m {
                let name = if playlist_on { "StopPlayList" } else { "StartPlayList" };
                self.call(name, &Args::new());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                let locked_icon = Icon::Lock(locked);
                let lock = named(icon_button(ui, locked_icon, None), "Cadenas");
                let lock = lock.on_hover_text("Cadenas : protège les boutons sensibles");
                if lock.clicked() {
                    self.with(|s| s.padlock = !s.padlock);
                }
                if icon_button(ui, Icon::Camera, None).on_hover_text("Instantané de la sortie").clicked() {
                    self.take_snapshot(None);
                }
                if named(icon_button(ui, Icon::Stats, None), "Statistiques").clicked() {
                    self.stats_open = !self.stats_open;
                }
                if bar_button(ui, "Incrustations", self.overlays_open).clicked() {
                    self.overlays_open = !self.overlays_open;
                }
            });
        });
    }

    pub(super) fn open_settings(&mut self, tab: usize) {
        self.settings_open = true;
        self.settings_tab = tab;
        self.screens = tools::list_screens();
    }

    pub(super) fn status_line(&mut self, ui: &mut egui::Ui) {
        if let Some(err) = self.engine.output_error.lock().unwrap().take() {
            // Stream keys never reach the screen, even inside an error.
            let keys: Vec<String> =
                self.with(|s| s.project.streams.iter().map(|d| d.key.trim().to_string()).collect());
            let shown = keys.iter().filter(|k| !k.is_empty()).fold(err, |e, k| e.replace(k, "••••"));
            self.message = Some((shown, Instant::now()));
        }
        let (h, w, fps) = self.with(|s| (s.project.out_h, s.project.out_w, s.project.fps));
        ui.horizontal_centered(|ui| {
            let st = &self.engine.stats;
            let ticks = st.ticks_per_sec.load(Ordering::Relaxed);
            let render = st.mix_us.load(Ordering::Relaxed) as f32 / 1000.0;
            let me = st.cpu_self.load(Ordering::Relaxed);
            let total = st.cpu_total.load(Ordering::Relaxed);
            let mode = Color32::from_rgb(90, 200, 90);
            let name = match (w, h) {
                (1080, 1920) => format!("Vertical {fps}"),
                (3840, 2160) => format!("UHD {fps}"),
                (4096, 2160) => format!("4K {fps}"),
                _ => format!("{h}p{fps}"),
            };
            ui.label(RichText::new(name).size(11.0).color(mode));
            let line = if ticks == 0 {
                "  Démarrage…".to_string()
            } else {
                format!(
                    "  i/s : {ticks}   Rendu : {render:.1} ms   Processeur LiveCraft : {me} %   Total : {total} %"
                )
            };
            ui.label(RichText::new(line).size(11.0).color(DIM));
            if st.struggling.load(Ordering::Relaxed) {
                ui.label(RichText::new("  Machine chargée").size(11.0).color(ORANGE));
            }
            if let Some((msg, at)) = &self.message
                && at.elapsed() < Duration::from_secs(12)
            {
                ui.add_space(16.0);
                ui.label(RichText::new(msg).size(11.0).color(ORANGE));
            }
        });
    }

    /// Saves the output, or one input, where the person chooses.
    pub(super) fn take_snapshot(&mut self, input: Option<u64>) {
        let frame = match input {
            None => Some(self.engine.program.lock().unwrap().clone()),
            Some(id) => self.engine.rt(id).and_then(|r| r.latest()),
        };
        let Some(frame) = frame else {
            self.say("Rien à capturer");
            return;
        };
        let dir = user_dir("Pictures");
        let _ = std::fs::create_dir_all(&dir);
        let name = chrono::Local::now().format("instantane-%Y%m%d-%H%M%S.png").to_string();
        let picked = rfd::FileDialog::new()
            .add_filter("Image", &["png", "jpg", "bmp"])
            .set_directory(&dir)
            .set_file_name(name)
            .save_file();
        let Some(path) = picked else { return };
        let path = path.to_string_lossy().into_owned();
        match self.engine.snapshot_to(&path, &frame) {
            Ok(()) => self.say(format!("Image enregistrée : {path}")),
            Err(e) => self.say(format!("Échec de l'instantané : {e}")),
        }
    }
}

fn read_any(path: &str) -> Result<Project, String> {
    if path.to_lowercase().ends_with(".vmix") {
        return crate::vmix_import::load(path).map(|(p, _)| p);
    }
    read_project(path)
}

pub(super) fn read_project(path: &str) -> Result<Project, String> {
    std::fs::read(path)
        .map_err(|e| e.to_string())
        .and_then(|b| serde_json::from_slice::<Project>(&b).map_err(|e| e.to_string()))
}
