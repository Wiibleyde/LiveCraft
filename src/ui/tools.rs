//! Tool windows: production clocks, statistics, MultiCorder, Instant Replay,
//! PTZ, Telestrator, title designer and picture scopes.

use super::settings::audio_pick;
use super::*;
use crate::extras::{Ptz, ptz_send};
use crate::replay;
use crate::titles::{Stroke as Mark, StrokeShape};

impl App {
    pub(super) fn clocks_window(&mut self, ctx: &egui::Context, project: &Project) {
        if !self.clocks_open {
            return;
        }
        let mut open = true;
        let mut clocks = project.clocks.clone();
        let running = self.with(|s| s.clock.running());
        let mut action = None;
        egui::Window::new(t!("tools.clocks_title"))
            .id(egui::Id::unique("Horloges de production"))
            .open(&mut open)
            .show(ctx, |ui| {
                for (n, c) in clocks.iter_mut().enumerate() {
                    ui.push_id(n, |ui| {
                        ui.label(RichText::new(t!("tools.clock_n", n = n + 1)).strong());
                        ui.horizontal(|ui| {
                            for (name, kind) in [
                                (t!("tools.kind_time"), ClockType::Clock),
                                (t!("tools.kind_event"), ClockType::Event),
                                (t!("tools.kind_recording"), ClockType::Recording),
                                (t!("tools.kind_streaming"), ClockType::Streaming),
                                (t!("tools.kind_remaining"), ClockType::TimeRemaining),
                                (t!("tools.kind_show"), ClockType::Show),
                            ] {
                                let was = c.kind;
                                ui.selectable_value(&mut c.kind, kind, name);
                                // At minutes a running show clock would look frozen.
                                if kind == ClockType::Show && was != kind && c.precision == 0 {
                                    c.precision = 1;
                                }
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut c.inverted, t!("tools.inverted"));
                            ui.selectable_value(&mut c.precision, 0, t!("tools.minutes"));
                            ui.selectable_value(&mut c.precision, 1, t!("tools.seconds"));
                            ui.selectable_value(&mut c.precision, 2, t!("tools.tenths"));
                        });
                        match c.kind {
                            ClockType::Clock => {
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::DragValue::new(&mut c.hour_offset)
                                            .range(-23..=23)
                                            .prefix(format!("{} ", t!("tools.offset"))),
                                    );
                                    ui.checkbox(&mut c.am_pm, t!("tools.twelve_hours"));
                                });
                            }
                            ClockType::Event => {
                                ui.horizontal(|ui| {
                                    ui.label(t!("tools.start"));
                                    ui.add(
                                        egui::TextEdit::singleline(&mut c.start)
                                            .hint_text("HH:MM")
                                            .desired_width(60.0),
                                    );
                                    ui.label(t!("tools.end"));
                                    ui.add(
                                        egui::TextEdit::singleline(&mut c.end)
                                            .hint_text("HH:MM")
                                            .desired_width(60.0),
                                    );
                                });
                            }
                            ClockType::Show => {
                                ui.horizontal(|ui| {
                                    let label =
                                        if running { t!("tools.show_pause") } else { t!("tools.show_start") };
                                    if named(ui.button(label), format!("{} {}", t!("tools.kind_show"), label))
                                        .clicked()
                                    {
                                        action = Some("StartPauseShowClock");
                                    }
                                    if named(
                                        ui.button(t!("tools.show_reset")),
                                        format!("{} {}", t!("tools.kind_show"), t!("tools.show_reset")),
                                    )
                                    .clicked()
                                    {
                                        action = Some("ResetShowClock");
                                    }
                                });
                            }
                            _ => {}
                        }
                        ui.separator();
                    });
                }
            });
        self.clocks_open = open;
        if let Some(name) = action {
            self.call(name, &Args::new());
        }
        if clocks != project.clocks {
            self.with(|s| s.project.clocks = clocks);
        }
    }

    pub(super) fn stats_window(&mut self, ctx: &egui::Context, project: &Project) {
        if !self.stats_open {
            return;
        }
        let mut open = true;
        egui::Window::new(t!("tools.stats_title"))
            .id(egui::Id::unique("Statistiques"))
            .open(&mut open)
            .default_width(460.0)
            .show(ctx, |ui| {
                let st = &self.engine.stats;
                ui.label(t!(
                    "tools.stats_summary",
                    fps = st.composed_per_sec.load(Ordering::Relaxed),
                    late = st.late_ticks.load(Ordering::Relaxed),
                ));
                ui.separator();
                ui.label(RichText::new(t!("tools.inputs")).strong());
                egui::Grid::new("stats").striped(true).show(ui, |ui| {
                    ui.label(t!("tools.input"));
                    ui.label(t!("tools.frames_received"));
                    ui.label(t!("tools.state"));
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
                ui.label(RichText::new(t!("tools.encoders")).strong());
                egui::Grid::new("encodeurs").striped(true).show(ui, |ui| {
                    ui.label(t!("tools.output"));
                    ui.label(t!("tools.codec"));
                    ui.label(t!("tools.frames"));
                    ui.label(t!("tools.dropped"));
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
        egui::Window::new(t!("tools.multi_title"))
            .id(egui::Id::unique("Multi-enregistrement"))
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(RichText::new(t!("tools.multi_hint")).small().color(DIM));
                let list: Vec<&Input> = project.inputs.iter().filter(|i| i.kind.is_recordable()).collect();
                if list.is_empty() {
                    ui.label(RichText::new(t!("tools.multi_none")).color(DIM));
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
                if ui.button(if running { t!("tools.stop") } else { t!("tools.start_btn") }).clicked() {
                    if running && self.with(|s| s.padlock) {
                        self.say(t!("tools.locked_stop"));
                    } else if running {
                        self.engine.stop_multi();
                    } else {
                        let dir = self.saved.record_dir.clone();
                        let (ids, audio) = (self.multi_ids.clone(), self.multi_audio);
                        match self.engine.start_multi(&dir, &ids, audio) {
                            Ok(n) => self.say(t!("tools.multi_started", n = n)),
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
        egui::Window::new(t!("tools.replay_title"))
            .id(egui::Id::unique("Ralenti"))
            .open(&mut open)
            .default_width(520.0)
            .show(ctx, |ui| {
                let running = self.engine.replay.lock().unwrap().started.is_some();
                if !running {
                    ui.label(RichText::new(t!("tools.replay_cams")).strong());
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
                    if ui.button(t!("tools.replay_start")).clicked() {
                        let folder = std::env::temp_dir().join("LiveCraft-ralenti");
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
                        RichText::new(t!(
                            "tools.replay_recorded",
                            time = clock((r.recorded_s() * 1000.0) as u64)
                        ))
                        .strong(),
                    );
                    ui.add(egui::Slider::new(&mut r.speed, 0.05..=1.0).text(t!("tools.speed")));
                    ui.checkbox(&mut r.quad, t!("tools.quad"));
                });
                drop(r);
                ui.horizontal(|ui| {
                    if ui.button(t!("tools.mark_in")).clicked() {
                        self.engine.replay_mark_in();
                    }
                    if ui.button(t!("tools.mark_out")).clicked() {
                        self.engine.replay_mark_out();
                    }
                    for secs in [5.0, 10.0, 20.0] {
                        if ui.button(format!("{secs} s")).clicked() {
                            self.engine.replay_mark_last(secs);
                        }
                    }
                    if ui.button(t!("tools.stop")).clicked() {
                        if self.with(|s| s.padlock) {
                            self.say(t!("tools.locked_stop"));
                        } else {
                            self.engine.replay_stop();
                        }
                    }
                });
                ui.add(
                    egui::TextEdit::singleline(&mut self.replay_query).hint_text(t!("tools.replay_search")),
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
                                if ui.button(t!("tools.play")).clicked() {
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
                                        .hint_text(t!("tools.note"))
                                        .desired_width(110.0),
                                );
                                let mut tags = e.tag_text.clone().unwrap_or_else(|| e.tags.join(", "));
                                let edit = egui::TextEdit::singleline(&mut tags)
                                    .hint_text(t!("tools.tags"))
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
        egui::Window::new(t!("tools.ptz_title"))
            .id(egui::Id::unique("Commande PTZ"))
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                if host.is_empty() {
                    ui.label(RichText::new(t!("tools.ptz_no_host")).color(ORANGE));
                }
                let hold = |ui: &mut egui::Ui, label: &str, cmd: Ptz, stop: Ptz, send: &mut Option<Ptz>| {
                    let r = ui.add(
                        egui::Button::new(label).min_size(vec2(44.0, 30.0)).sense(Sense::click_and_drag()),
                    );
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
                    hold(ui, t!("tools.up"), go(0, 1), Ptz::Stop, &mut send);
                    ui.label("");
                    ui.end_row();
                    hold(ui, t!("tools.left"), go(-1, 0), Ptz::Stop, &mut send);
                    if ui.add(egui::Button::new(t!("tools.home")).min_size(vec2(44.0, 30.0))).clicked() {
                        send = Some(Ptz::Home);
                    }
                    hold(ui, t!("tools.right"), go(1, 0), Ptz::Stop, &mut send);
                    ui.end_row();
                    ui.label("");
                    hold(ui, t!("tools.down"), go(0, -1), Ptz::Stop, &mut send);
                    ui.label("");
                    ui.end_row();
                });
                ui.horizontal(|ui| {
                    hold(ui, t!("tools.zoom_in"), Ptz::ZoomIn, Ptz::ZoomStop, &mut send);
                    hold(ui, t!("tools.zoom_out"), Ptz::ZoomOut, Ptz::ZoomStop, &mut send);
                });
                ui.label(t!("tools.ptz_presets"));
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
        egui::Window::new(t!("tools.telestrator_title"))
            .id(egui::Id::unique("Telestrateur"))
            .open(&mut open)
            .default_width(660.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.color_edit_button_srgb(&mut self.tele_color);
                    ui.selectable_value(&mut self.tele_shape, 0, t!("tools.line"));
                    ui.selectable_value(&mut self.tele_shape, 1, t!("tools.square"));
                    ui.selectable_value(&mut self.tele_shape, 2, t!("tools.circle"));
                    ui.add(egui::Slider::new(&mut self.tele_size, 0.003..=0.04).show_value(false));
                    if ui.button(t!("tools.undo")).clicked() {
                        self.engine.telestrator_clear(id, true);
                    }
                    if ui.button(t!("tools.clear")).clicked() {
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
                ui.label(RichText::new(t!("tools.telestrator_hint")).small().color(DIM));
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
        egui::Window::new(t!("tools.designer_title"))
            .id(egui::Id::unique("Titre personnalise"))
            .open(&mut open)
            .default_width(420.0)
            .show(ctx, |ui| {
                let mut remove = None;
                for (n, o) in list.iter_mut().enumerate() {
                    ui.push_id(n, |ui| {
                        match o {
                            TitleObject::Text { text, x, y, size, color, bold } => {
                                ui.horizontal(|ui| {
                                    ui.text_edit_singleline(text);
                                    ui.color_edit_button_srgb(color);
                                    ui.checkbox(bold, t!("tools.bold"));
                                });
                                ui.add(egui::Slider::new(x, 0.0..=1.0).text("X"));
                                ui.add(egui::Slider::new(y, 0.0..=1.0).text("Y"));
                                ui.add(egui::Slider::new(size, 0.01..=0.4).text(t!("tools.size")));
                            }
                            TitleObject::Rect { x, y, w, h, color, alpha } => {
                                ui.horizontal(|ui| {
                                    ui.label(t!("tools.shape"));
                                    ui.color_edit_button_srgb(color);
                                    ui.add(egui::Slider::new(alpha, 0..=255).text(t!("tools.opacity")));
                                });
                                ui.add(egui::Slider::new(x, 0.0..=1.0).text("X"));
                                ui.add(egui::Slider::new(y, 0.0..=1.0).text("Y"));
                                ui.add(egui::Slider::new(w, 0.0..=1.0).text(t!("tools.width")));
                                ui.add(egui::Slider::new(h, 0.0..=1.0).text(t!("tools.height")));
                            }
                            TitleObject::Image { path, x, y, w, h } => {
                                ui.add(egui::Label::new(RichText::new(path.as_str()).small()).truncate());
                                ui.add(egui::Slider::new(x, 0.0..=1.0).text("X"));
                                ui.add(egui::Slider::new(y, 0.0..=1.0).text("Y"));
                                ui.add(egui::Slider::new(w, 0.0..=1.0).text(t!("tools.width")));
                                ui.add(egui::Slider::new(h, 0.0..=1.0).text(t!("tools.height")));
                            }
                        }
                        if ui.small_button(t!("tools.remove")).clicked() {
                            remove = Some(n);
                        }
                        ui.separator();
                    });
                }
                if let Some(n) = remove {
                    list.remove(n);
                }
                ui.horizontal(|ui| {
                    if ui.button(t!("tools.add_text")).clicked() {
                        list.push(TitleObject::Text {
                            text: t!("tools.default_text").into(),
                            x: 0.1,
                            y: 0.75,
                            size: 0.07,
                            color: [255, 255, 255],
                            bold: true,
                        });
                    }
                    if ui.button(t!("tools.add_shape")).clicked() {
                        list.push(TitleObject::Rect {
                            x: 0.08,
                            y: 0.72,
                            w: 0.5,
                            h: 0.14,
                            color: [20, 24, 34],
                            alpha: 220,
                        });
                    }
                    if ui.button(t!("tools.add_image")).clicked()
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
        egui::Window::new(t!("tools.scope_title"))
            .id(egui::Id::unique("Moniteur de forme d'onde"))
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.scope_mode, 0, t!("tools.rgb"));
                    ui.selectable_value(&mut self.scope_mode, 1, t!("tools.luma"));
                    ui.selectable_value(&mut self.scope_mode, 2, t!("tools.parade"));
                    ui.selectable_value(&mut self.scope_mode, 3, t!("tools.vectorscope"));
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
