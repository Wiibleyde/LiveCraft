//! Tool windows: production clocks, statistics, MultiCorder, Instant Replay,
//! PTZ, Telestrator, title designer and picture scopes.

use super::settings::audio_pick;
use super::*;
use crate::extras::{Ptz, ptz_send};
use crate::replay;
use crate::titles::{Stroke as Mark, StrokeShape};

/// Display rectangles, primary first: left, top, width, height.
pub(super) fn list_screens() -> Vec<(i32, i32, i32, i32)> {
    use windows_sys::Win32::Foundation::{LPARAM, RECT};
    use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};
    unsafe extern "system" fn add(_: HMONITOR, _: HDC, r: *mut RECT, data: LPARAM) -> i32 {
        // SAFETY: Windows hands back the list pointer given below, and a valid rectangle.
        unsafe {
            let list = &mut *(data as *mut Vec<(i32, i32, i32, i32)>);
            let r = &*r;
            list.push((r.left, r.top, r.right - r.left, r.bottom - r.top));
        }
        1
    }
    let mut list: Vec<(i32, i32, i32, i32)> = Vec::new();
    // SAFETY: the callback only runs during this call, while `list` is alive.
    unsafe {
        EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(add), &mut list as *mut _ as LPARAM);
    }
    // The primary display sits at the origin; the rest keep reading order.
    list.sort_by_key(|&(x, y, _, _)| ((x, y) != (0, 0), y, x));
    list
}

impl App {
    pub(super) fn clocks_window(&mut self, ctx: &egui::Context, project: &Project) {
        if !self.clocks_open {
            return;
        }
        let mut open = true;
        let mut clocks = project.clocks.clone();
        egui::Window::new("Horloges de production").open(&mut open).show(ctx, |ui| {
            for (n, c) in clocks.iter_mut().enumerate() {
                ui.push_id(n, |ui| {
                    ui.label(RichText::new(format!("Horloge {}", n + 1)).strong());
                    ui.horizontal(|ui| {
                        for (name, kind) in [
                            ("Heure", ClockType::Clock),
                            ("Événement", ClockType::Event),
                            ("Enregistrement", ClockType::Recording),
                            ("Diffusion", ClockType::Streaming),
                            ("Temps restant", ClockType::TimeRemaining),
                        ] {
                            ui.selectable_value(&mut c.kind, kind, name);
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut c.inverted, "Inversé");
                        ui.selectable_value(&mut c.precision, 0, "Minutes");
                        ui.selectable_value(&mut c.precision, 1, "Secondes");
                        ui.selectable_value(&mut c.precision, 2, "Dixièmes");
                    });
                    match c.kind {
                        ClockType::Clock => {
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::DragValue::new(&mut c.hour_offset)
                                        .range(-23..=23)
                                        .prefix("décalage "),
                                );
                                ui.checkbox(&mut c.am_pm, "12 heures");
                            });
                        }
                        ClockType::Event => {
                            ui.horizontal(|ui| {
                                ui.label("Début");
                                ui.add(
                                    egui::TextEdit::singleline(&mut c.start)
                                        .hint_text("HH:MM")
                                        .desired_width(60.0),
                                );
                                ui.label("Fin");
                                ui.add(
                                    egui::TextEdit::singleline(&mut c.end)
                                        .hint_text("HH:MM")
                                        .desired_width(60.0),
                                );
                            });
                        }
                        _ => {}
                    }
                    ui.separator();
                });
            }
        });
        self.clocks_open = open;
        if clocks != project.clocks {
            self.with(|s| s.project.clocks = clocks);
        }
    }

    pub(super) fn stats_window(&mut self, ctx: &egui::Context, project: &Project) {
        if !self.stats_open {
            return;
        }
        let mut open = true;
        egui::Window::new("Statistiques").open(&mut open).default_width(460.0).show(ctx, |ui| {
            let st = &self.engine.stats;
            ui.label(format!(
                "Images composées : {}/s · Retards du moteur : {}",
                st.composed_per_sec.load(Ordering::Relaxed),
                st.late_ticks.load(Ordering::Relaxed),
            ));
            ui.separator();
            ui.label(RichText::new("Entrées").strong());
            egui::Grid::new("stats").striped(true).show(ui, |ui| {
                ui.label("Entrée");
                ui.label("Images reçues");
                ui.label("État");
                ui.end_row();
                for i in &project.inputs {
                    let rt = self.engine.rt(i.id);
                    ui.label(&i.name);
                    ui.label(rt.as_ref().map_or(0, |r| r.seq()).to_string());
                    ui.label(rt.as_ref().and_then(|r| r.status()).unwrap_or_else(|| "OK".into()));
                    ui.end_row();
                }
            });
            ui.separator();
            ui.label(RichText::new("Encodeurs").strong());
            egui::Grid::new("encodeurs").striped(true).show(ui, |ui| {
                ui.label("Sortie");
                ui.label("Codec");
                ui.label("Images");
                ui.label("Perdues");
                ui.end_row();
                let mut row = |e: &crate::output::Encoder| {
                    ui.label(&e.label);
                    ui.label(codec_label(&e.codec));
                    ui.label(e.sent.load(Ordering::Relaxed).to_string());
                    ui.label(e.dropped.load(Ordering::Relaxed).to_string());
                    ui.end_row();
                };
                self.engine.recorders.lock().unwrap().iter().for_each(&mut row);
                self.engine.streamers.lock().unwrap().iter().for_each(&mut row);
                self.engine.multi.lock().unwrap().iter().for_each(|(_, e)| row(e));
                self.engine.srt.lock().unwrap().iter().for_each(|(_, e)| row(e));
            });
        });
        self.stats_open = open;
    }

    pub(super) fn multi_window(&mut self, ctx: &egui::Context, project: &Project) {
        if !self.multi_open {
            return;
        }
        let mut open = true;
        egui::Window::new("Multi-enregistrement").open(&mut open).show(ctx, |ui| {
            ui.label(
                RichText::new("Une piste séparée par source, à côté de l'enregistrement principal")
                    .small()
                    .color(DIM),
            );
            let list: Vec<&Input> = project.inputs.iter().filter(|i| i.kind.is_recordable()).collect();
            if list.is_empty() {
                ui.label(RichText::new("Aucune caméra, vidéo ou capture").color(DIM));
            }
            for i in list {
                let mut on = self.multi_ids.contains(&i.id);
                if ui.checkbox(&mut on, &i.name).changed() {
                    self.multi_ids.retain(|x| *x != i.id);
                    if on {
                        self.multi_ids.push(i.id);
                    }
                }
            }
            audio_pick(ui, "multi-son", &mut self.multi_audio);
            let running = self.engine.is_multi();
            if ui.button(if running { "Arrêter" } else { "Démarrer" }).clicked() {
                if running && self.with(|s| s.padlock) {
                    self.say("Verrouillé : ouvrez le cadenas pour arrêter");
                } else if running {
                    self.engine.stop_multi();
                } else {
                    let dir = self.saved.record_dir.clone();
                    let (ids, audio) = (self.multi_ids.clone(), self.multi_audio);
                    match self.engine.start_multi(&dir, &ids, audio) {
                        Ok(n) => self.say(format!("{n} source(s) enregistrée(s) séparément")),
                        Err(e) => self.say(e),
                    }
                }
            }
        });
        self.multi_open = open;
    }

    pub(super) fn replay_window(&mut self, ctx: &egui::Context, project: &Project) {
        if !self.replay_open {
            return;
        }
        let mut open = true;
        egui::Window::new("Ralenti").open(&mut open).default_width(520.0).show(ctx, |ui| {
            let running = self.engine.replay.lock().unwrap().started.is_some();
            if !running {
                ui.label(RichText::new("Caméras à enregistrer (jusqu'à 8)").strong());
                ui.horizontal_wrapped(|ui| {
                    for i in project.inputs.iter().filter(|i| i.kind.has_picture()) {
                        let mut on = self.replay_cams.contains(&i.id);
                        let full = self.replay_cams.len() >= replay::MAX_CAMERAS;
                        if ui.add_enabled(on || !full, egui::Checkbox::new(&mut on, &i.name)).changed() {
                            self.replay_cams.retain(|x| *x != i.id);
                            if on {
                                self.replay_cams.push(i.id);
                            }
                        }
                    }
                });
                if ui.button("Démarrer l'enregistrement").clicked() {
                    let folder = std::env::temp_dir().join("Regie-ralenti");
                    let ids = self.replay_cams.clone();
                    if let Err(e) = self.engine.replay_start(&ids, folder) {
                        self.say(e);
                    }
                }
                return;
            }
            let mut r = self.engine.replay.lock().unwrap();
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("Enregistré : {}", clock((r.recorded_s() * 1000.0) as u64)))
                        .strong(),
                );
                ui.add(egui::Slider::new(&mut r.speed, 0.05..=1.0).text("vitesse"));
                ui.checkbox(&mut r.quad, "Quatre angles");
            });
            drop(r);
            ui.horizontal(|ui| {
                if ui.button("Marquer l'entrée").clicked() {
                    self.engine.replay_mark_in();
                }
                if ui.button("Marquer la sortie").clicked() {
                    self.engine.replay_mark_out();
                }
                for secs in [5.0, 10.0, 20.0] {
                    if ui.button(format!("{secs} s")).clicked() {
                        self.engine.replay_mark_last(secs);
                    }
                }
                if ui.button("Arrêter").clicked() {
                    if self.with(|s| s.padlock) {
                        self.say("Verrouillé : ouvrez le cadenas pour arrêter");
                    } else {
                        self.engine.replay_stop();
                    }
                }
            });
            ui.add(
                egui::TextEdit::singleline(&mut self.replay_query)
                    .hint_text("Rechercher un moment ou une étiquette"),
            );
            let mut play = None;
            let mut r = self.engine.replay.lock().unwrap();
            let names: Vec<String> = r.cams.iter().map(|c| c.name.clone()).collect();
            let query = self.replay_query.clone();
            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                for (n, e) in r.events.iter_mut().enumerate() {
                    if !replay::matches(e, &query) {
                        continue;
                    }
                    ui.push_id(n, |ui| {
                        ui.horizontal(|ui| {
                            if ui.button("Lire").clicked() {
                                play = Some(n);
                            }
                            let span = e.end_s - e.start_s;
                            ui.label(format!("{} ({span:.1} s)", clock((e.start_s * 1000.0) as u64)));
                            let shown = names.get(e.camera).cloned().unwrap_or_default();
                            egui::ComboBox::from_id_salt("cam").selected_text(shown).show_ui(ui, |ui| {
                                for (c, name) in names.iter().enumerate() {
                                    ui.selectable_value(&mut e.camera, c, name);
                                }
                            });
                            ui.add(
                                egui::TextEdit::singleline(&mut e.text)
                                    .hint_text("Note")
                                    .desired_width(110.0),
                            );
                            let mut tags = e.tag_text.clone().unwrap_or_else(|| e.tags.join(", "));
                            let edit = egui::TextEdit::singleline(&mut tags)
                                .hint_text("Étiquettes")
                                .desired_width(100.0);
                            if ui.add(edit).changed() {
                                e.tags = tags
                                    .split(',')
                                    .map(|t| t.trim().to_string())
                                    .filter(|t| !t.is_empty())
                                    .collect();
                                e.tag_text = Some(tags);
                            }
                        });
                    });
                }
            });
            drop(r);
            if let Some(n) = play
                && let Some(id) = self.engine.replay_play(n)
            {
                self.with(|s| s.set_preview(id));
            }
        });
        self.replay_open = open;
    }

    pub(super) fn ptz_window(&mut self, ctx: &egui::Context, project: &Project) {
        let Some(id) = self.ptz_for else { return };
        let host = project.inputs.iter().find(|i| i.id == id).and_then(|i| match &i.kind {
            Kind::Camera { ptz_host, .. } => Some(ptz_host.clone()),
            _ => None,
        });
        let Some(host) = host else {
            self.ptz_for = None;
            return;
        };
        let mut open = true;
        let mut send = None;
        egui::Window::new("Commande PTZ").open(&mut open).resizable(false).show(ctx, |ui| {
            if host.is_empty() {
                ui.label(RichText::new("Indiquez l'adresse dans les réglages de la caméra").color(ORANGE));
            }
            let hold = |ui: &mut egui::Ui, label: &str, cmd: Ptz, stop: Ptz, send: &mut Option<Ptz>| {
                let r = ui
                    .add(egui::Button::new(label).min_size(vec2(44.0, 30.0)).sense(Sense::click_and_drag()));
                if r.drag_started()
                    || r.is_pointer_button_down_on() && r.ctx.input(|i| i.pointer.any_pressed())
                {
                    *send = Some(cmd);
                }
                if r.drag_stopped() || r.clicked() {
                    *send = Some(stop);
                }
            };
            let go = |pan, tilt| Ptz::Move { pan, tilt };
            egui::Grid::new("ptz").show(ui, |ui| {
                ui.label("");
                hold(ui, "Haut", go(0, 1), Ptz::Stop, &mut send);
                ui.label("");
                ui.end_row();
                hold(ui, "Gauche", go(-1, 0), Ptz::Stop, &mut send);
                if ui.add(egui::Button::new("Accueil").min_size(vec2(44.0, 30.0))).clicked() {
                    send = Some(Ptz::Home);
                }
                hold(ui, "Droite", go(1, 0), Ptz::Stop, &mut send);
                ui.end_row();
                ui.label("");
                hold(ui, "Bas", go(0, -1), Ptz::Stop, &mut send);
                ui.label("");
                ui.end_row();
            });
            ui.horizontal(|ui| {
                hold(ui, "Zoom +", Ptz::ZoomIn, Ptz::ZoomStop, &mut send);
                hold(ui, "Zoom -", Ptz::ZoomOut, Ptz::ZoomStop, &mut send);
            });
            ui.label("Positions enregistrées (clic : rappeler, clic droit : mémoriser)");
            ui.horizontal(|ui| {
                for n in 0..6u8 {
                    let r = ui.button(format!("{}", n + 1));
                    if r.clicked() {
                        send = Some(Ptz::Recall(n));
                    }
                    if r.secondary_clicked() {
                        send = Some(Ptz::Store(n));
                    }
                }
            });
        });
        if !open {
            self.ptz_for = None;
        }
        if let Some(cmd) = send {
            self.ptz_seq = self.ptz_seq.wrapping_add(1);
            if let Err(e) = ptz_send(&host, self.ptz_seq, cmd) {
                self.say(e);
            }
        }
    }

    pub(super) fn telestrator_window(&mut self, ctx: &egui::Context, project: &Project) {
        let Some(id) = self.telestrator_for else { return };
        let Some(rt) = self.engine.rt(id) else {
            self.telestrator_for = None;
            return;
        };
        let aspect = project.out_w as f32 / project.out_h.max(1) as f32;
        let mut open = true;
        // Drawn over its chosen picture, or over the output when it has none.
        let chosen = project.inputs.iter().find(|i| i.id == id).and_then(|i| match i.kind {
            Kind::Telestrator { background } => background,
            _ => None,
        });
        let background = match chosen {
            Some(b) => self.thumb(ctx, b).map(|(t, _)| t),
            None => self.program_tex.as_ref().map(|t| t.id()),
        };
        egui::Window::new("Télestrateur").open(&mut open).default_width(660.0).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.color_edit_button_srgb(&mut self.tele_color);
                ui.selectable_value(&mut self.tele_shape, 0, "Ligne");
                ui.selectable_value(&mut self.tele_shape, 1, "Carré");
                ui.selectable_value(&mut self.tele_shape, 2, "Cercle");
                ui.add(egui::Slider::new(&mut self.tele_size, 0.003..=0.04).show_value(false));
                if ui.button("Annuler").clicked() {
                    self.engine.telestrator_clear(id, true);
                }
                if ui.button("Effacer").clicked() {
                    self.engine.telestrator_clear(id, false);
                }
            });
            let (rect, r) = ui.allocate_exact_size(vec2(640.0, 640.0 / aspect), Sense::drag());
            ui.painter().rect_filled(rect, 0.0, Color32::BLACK);
            if let Some(t) = background {
                ui.painter().image(t, rect, full_uv(), Color32::from_gray(150));
            }
            if let Some(pos) = r.interact_pointer_pos() {
                let u = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
                let v = ((pos.y - rect.top()) / rect.height()).clamp(0.0, 1.0);
                if r.dragged() {
                    self.tele_points.push((u, v));
                }
            }
            let col = rgb(self.tele_color);
            let line: Vec<_> = self
                .tele_points
                .iter()
                .map(|(u, v)| rect.min + vec2(u * rect.width(), v * rect.height()))
                .collect();
            ui.painter().add(egui::Shape::line(line, Stroke::new(2.0, col)));
            if r.drag_stopped() && !self.tele_points.is_empty() {
                let points = std::mem::take(&mut self.tele_points);
                let shape = match self.tele_shape {
                    1 => StrokeShape::Rect,
                    2 => StrokeShape::Circle,
                    _ => StrokeShape::Line,
                };
                let mark = Mark { shape, points, color: self.tele_color, size: self.tele_size };
                rt.strokes.lock().unwrap().push(mark);
                rt.strokes_gen.fetch_add(1, Ordering::Release);
            }
            ui.label(
                RichText::new("Aussi depuis une tablette : page /telestrator du contrôle web")
                    .small()
                    .color(DIM),
            );
        });
        if !open {
            self.telestrator_for = None;
        }
    }

    pub(super) fn designer_window(&mut self, ctx: &egui::Context, project: &Project) {
        let Some(id) = self.designer_for else { return };
        let Some(Kind::Designed { objects, anim_ms }) =
            project.inputs.iter().find(|i| i.id == id).map(|i| i.kind.clone())
        else {
            self.designer_for = None;
            return;
        };
        let mut list = objects.clone();
        let mut open = true;
        egui::Window::new("Titre personnalisé").open(&mut open).default_width(420.0).show(ctx, |ui| {
            let mut remove = None;
            for (n, o) in list.iter_mut().enumerate() {
                ui.push_id(n, |ui| {
                    match o {
                        TitleObject::Text { text, x, y, size, color, bold } => {
                            ui.horizontal(|ui| {
                                ui.text_edit_singleline(text);
                                ui.color_edit_button_srgb(color);
                                ui.checkbox(bold, "Gras");
                            });
                            ui.add(egui::Slider::new(x, 0.0..=1.0).text("X"));
                            ui.add(egui::Slider::new(y, 0.0..=1.0).text("Y"));
                            ui.add(egui::Slider::new(size, 0.01..=0.4).text("Taille"));
                        }
                        TitleObject::Rect { x, y, w, h, color, alpha } => {
                            ui.horizontal(|ui| {
                                ui.label("Forme");
                                ui.color_edit_button_srgb(color);
                                ui.add(egui::Slider::new(alpha, 0..=255).text("Opacité"));
                            });
                            ui.add(egui::Slider::new(x, 0.0..=1.0).text("X"));
                            ui.add(egui::Slider::new(y, 0.0..=1.0).text("Y"));
                            ui.add(egui::Slider::new(w, 0.0..=1.0).text("Largeur"));
                            ui.add(egui::Slider::new(h, 0.0..=1.0).text("Hauteur"));
                        }
                        TitleObject::Image { path, x, y, w, h } => {
                            ui.add(egui::Label::new(RichText::new(path.as_str()).small()).truncate());
                            ui.add(egui::Slider::new(x, 0.0..=1.0).text("X"));
                            ui.add(egui::Slider::new(y, 0.0..=1.0).text("Y"));
                            ui.add(egui::Slider::new(w, 0.0..=1.0).text("Largeur"));
                            ui.add(egui::Slider::new(h, 0.0..=1.0).text("Hauteur"));
                        }
                    }
                    if ui.small_button("Retirer").clicked() {
                        remove = Some(n);
                    }
                    ui.separator();
                });
            }
            if let Some(n) = remove {
                list.remove(n);
            }
            ui.horizontal(|ui| {
                if ui.button("+ Texte").clicked() {
                    list.push(TitleObject::Text {
                        text: "Texte".into(),
                        x: 0.1,
                        y: 0.75,
                        size: 0.07,
                        color: [255, 255, 255],
                        bold: true,
                    });
                }
                if ui.button("+ Forme").clicked() {
                    list.push(TitleObject::Rect {
                        x: 0.08,
                        y: 0.72,
                        w: 0.5,
                        h: 0.14,
                        color: [20, 24, 34],
                        alpha: 220,
                    });
                }
                if ui.button("+ Image…").clicked()
                    && let Some(p) = rfd::FileDialog::new().pick_file()
                {
                    let path = p.to_string_lossy().into_owned();
                    list.push(TitleObject::Image { path, x: 0.8, y: 0.05, w: 0.15, h: 0.15 });
                }
            });
        });
        if !open {
            self.designer_for = None;
        }
        if list != objects {
            self.with(|s| {
                if let Some(i) = s.input_mut(id) {
                    i.kind = Kind::Designed { objects: list, anim_ms };
                }
            });
            self.queue_restart(id, true);
        }
    }

    pub(super) fn scope_window(&mut self, ctx: &egui::Context) {
        if !self.scope_open {
            return;
        }
        if self.scope_at.elapsed() > Duration::from_millis(200) {
            self.scope_at = Instant::now();
            if let Some(f) = self.engine.preview.lock().unwrap().clone() {
                let img = scope_image(&f, self.scope_mode);
                match &mut self.scope_tex {
                    Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                    None => {
                        self.scope_tex = Some(ctx.load_texture("scope", img, egui::TextureOptions::LINEAR))
                    }
                }
            }
        }
        let mut open = true;
        egui::Window::new("Moniteur de forme d'onde").open(&mut open).resizable(false).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.scope_mode, 0, "RVB");
                ui.selectable_value(&mut self.scope_mode, 1, "Luminance");
                ui.selectable_value(&mut self.scope_mode, 2, "Parade");
                ui.selectable_value(&mut self.scope_mode, 3, "Vectorscope");
            });
            let (rect, _) = ui.allocate_exact_size(vec2(384.0, 384.0), Sense::hover());
            ui.painter().rect_filled(rect, 0.0, Color32::BLACK);
            if let Some(t) = &self.scope_tex {
                ui.painter().image(t.id(), rect, full_uv(), Color32::WHITE);
            }
        });
        self.scope_open = open;
    }
}
