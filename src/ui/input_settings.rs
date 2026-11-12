//! The Input Settings window, one tab per subject.

use super::*;
use crate::dsp::EQ_BANDS;

fn tab_names() -> [&'static str; 10] {
    [
        t!("input_settings.tab.general"),
        t!("input_settings.colour"),
        t!("input_settings.tab.key"),
        t!("input_settings.tab.correction"),
        t!("input_settings.tab.effects"),
        t!("input_settings.tab.position"),
        t!("input_settings.tab.layers"),
        t!("input_settings.tab.triggers"),
        t!("input_settings.tab.audio"),
        t!("input_settings.tab.copy"),
    ]
}

impl App {
    pub(super) fn input_settings_window(&mut self, ctx: &egui::Context, project: &Project) {
        let Some(id) = self.editing else { return };
        let Some(before) = project.inputs.iter().find(|i| i.id == id).cloned() else {
            // An input added from a menu earlier in this frame is not in this
            // frame's copy of the project yet; only a closed one ends editing.
            if self.with(|s| s.input(id).is_none()) {
                self.editing = None;
            }
            return;
        };
        let mut input = before.clone();
        let mut open = true;
        let mut copy_from = None;
        egui::Window::new(t!("input_settings.window_title"))
            .id(egui::Id::unique("Reglages de l'entree"))
            .open(&mut open)
            .default_size([640.0, 460.0])
            .resizable(true)
            .show(ctx, |ui| {
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(100.0);
                        for (n, name) in tab_names().into_iter().enumerate() {
                            ui.selectable_value(&mut self.edit_tab, n, name);
                        }
                    });
                    ui.separator();
                    ui.vertical(|ui| {
                        egui::ScrollArea::vertical().max_height(470.0).show(ui, |ui| {
                            ui.set_min_width(460.0);
                            match self.edit_tab {
                                0 => self.tab_general(ui, &mut input, project),
                                1 => tab_colour(ui, &mut input),
                                2 => self.tab_key(ui, &mut input, project),
                                3 => tab_correction(ui, &mut input),
                                4 => tab_effects(ui, &mut input),
                                5 => tab_position(ui, &mut input),
                                6 => self.tab_layers(ui, &mut input, project),
                                7 => tab_triggers(ui, &mut input),
                                8 => tab_audio(ui, &mut input),
                                _ => copy_from = tab_copy(ui, &input, project),
                            }
                        });
                    });
                });
            });
        if !open {
            self.editing = None;
            self.picking_colour = false;
        }
        if let Some(src) = copy_from {
            let from = project.inputs.iter().find(|i| i.id == src);
            if let Some(from) = from {
                input.position = from.position;
                input.colour = from.colour;
                input.key = from.key;
                input.effects = from.effects.clone();
                input.mirror = from.mirror;
            }
        }
        if input != before {
            let source_changed = input.kind != before.kind;
            let live = !matches!(input.kind, Kind::Scene | Kind::Mix { .. } | Kind::VirtualSet { .. });
            let keep = input.kind.same_source(&before.kind);
            self.with(|s| {
                if let Some(slot) = s.input_mut(id) {
                    slot.merge_changes(&before, input);
                }
            });
            if source_changed && live {
                self.queue_restart(id, keep);
            }
        }
    }

    fn tab_general(&mut self, ui: &mut egui::Ui, input: &mut Input, project: &Project) {
        ui.horizontal(|ui| {
            ui.label(t!("input_settings.name"));
            ui.text_edit_singleline(&mut input.name);
        });
        ui.horizontal(|ui| {
            ui.label(t!("input_settings.tab_label"));
            let shown = input
                .category
                .and_then(|n| project.categories.get(n))
                .map_or(t!("input_settings.none_masc").to_string(), category_name);
            egui::ComboBox::from_id_salt("onglet").selected_text(shown).show_ui(ui, |ui| {
                ui.selectable_value(&mut input.category, None, t!("input_settings.none_masc"));
                for (n, c) in project.categories.iter().enumerate().filter(|(_, c)| c.enabled) {
                    ui.selectable_value(&mut input.category, Some(n), category_name(c));
                }
            });
            ui.checkbox(&mut input.compact, t!("input_settings.compact"));
            ui.checkbox(&mut input.mirror, t!("input_settings.mirror"));
        });
        ui.horizontal(|ui| {
            ui.label(t!("input_settings.click_label"));
            let label = match input.click {
                ClickAction::Preview => t!("input_settings.preview").to_string(),
                ClickAction::QuickPlay => t!("input_settings.go_live").to_string(),
                ClickAction::Cut => t!("input_settings.cut").to_string(),
                ClickAction::Overlay(n) => t!("input_settings.overlay_n", n = n + 1),
            };
            egui::ComboBox::from_id_salt("clic").selected_text(label).show_ui(ui, |ui| {
                ui.selectable_value(&mut input.click, ClickAction::Preview, t!("input_settings.preview"));
                ui.selectable_value(&mut input.click, ClickAction::QuickPlay, t!("input_settings.go_live"));
                ui.selectable_value(&mut input.click, ClickAction::Cut, t!("input_settings.cut"));
                for n in 0..8 {
                    ui.selectable_value(
                        &mut input.click,
                        ClickAction::Overlay(n),
                        t!("input_settings.overlay_n", n = n + 1),
                    );
                }
            });
        });
        go_editor(ui, input);
        ui.horizontal(|ui| {
            ui.checkbox(&mut input.auto_play, t!("input_settings.auto_play"));
            ui.checkbox(&mut input.auto_restart, t!("input_settings.auto_restart"));
            ui.checkbox(&mut input.auto_pause, t!("input_settings.auto_pause"));
        });
        ui.separator();
        self.source_editor(ui, input, project);
    }

    fn source_editor(&mut self, ui: &mut egui::Ui, input: &mut Input, project: &Project) {
        if matches!(input.kind, Kind::VirtualSet { .. }) {
            self.set_editor(ui, input, project);
            return;
        }
        let rt = self.engine.rt(input.id);
        let (cams, mics) = self.devices.lock().unwrap().clone();
        let id = input.id;
        let mut change_path = None;
        match &mut input.kind {
            Kind::Solid { color } => {
                ui.horizontal(|ui| {
                    ui.label(t!("input_settings.colour"));
                    ui.color_edit_button_srgb(color);
                });
            }
            Kind::Title {
                text,
                sub,
                style,
                color,
                accent,
                ticker_speed,
                countdown_ms,
                presets,
                data,
                font,
                ..
            } => {
                ui.label(t!("input_settings.text"));
                ui.text_edit_singleline(text);
                if !matches!(*style, TitleStyle::Ticker | TitleStyle::Countdown) {
                    ui.label(t!("input_settings.subtitle"));
                    ui.text_edit_singleline(sub);
                }
                ui.horizontal(|ui| {
                    ui.selectable_value(
                        style,
                        TitleStyle::LowerThird,
                        t!("input_settings.style.lower_third"),
                    );
                    ui.selectable_value(style, TitleStyle::Centered, t!("input_settings.style.centred"));
                    ui.selectable_value(style, TitleStyle::Ticker, t!("input_settings.style.ticker"));
                    ui.selectable_value(style, TitleStyle::Countdown, t!("input_settings.style.countdown"));
                });
                ui.horizontal(|ui| {
                    ui.label(t!("input_settings.text"));
                    ui.color_edit_button_srgb(color);
                    ui.label(t!("input_settings.accent"));
                    ui.color_edit_button_srgb(accent);
                });
                ui.horizontal(|ui| {
                    ui.label(t!("input_settings.font"));
                    ui.add(
                        egui::TextEdit::singleline(font)
                            .hint_text(t!("input_settings.font_hint"))
                            .desired_width(180.0),
                    );
                });
                if *style == TitleStyle::Ticker {
                    ui.add(
                        egui::Slider::new(ticker_speed, 1.0..=20.0)
                            .text(t!("input_settings.pixels_per_frame")),
                    );
                }
                if *style == TitleStyle::Countdown {
                    let mut secs = (*countdown_ms / 1000) as u32;
                    if ui
                        .add(egui::Slider::new(&mut secs, 5..=7200).text(t!("input_settings.seconds")))
                        .changed()
                    {
                        *countdown_ms = secs as u64 * 1000;
                    }
                }
                ui.horizontal(|ui| {
                    if ui.button(t!("input_settings.save_preset")).clicked() {
                        presets.push(TitlePreset {
                            name: text.clone(),
                            text: text.clone(),
                            sub: sub.clone(),
                        });
                    }
                    ui.label(
                        RichText::new(t!("input_settings.preset_count", count = presets.len()))
                            .small()
                            .color(DIM),
                    );
                });
                if !project.data_sources.is_empty() {
                    let mut linked = data.is_some();
                    if ui.checkbox(&mut linked, t!("input_settings.data_linked")).changed() {
                        *data = linked.then_some((0, 0, 1));
                    }
                    if let Some((src, a, b)) = data {
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::DragValue::new(src)
                                    .range(0..=project.data_sources.len() - 1)
                                    .prefix(format!("{} ", t!("input_settings.data_source_prefix"))),
                            );
                            ui.add(
                                egui::DragValue::new(a)
                                    .prefix(format!("{} ", t!("input_settings.title_column_prefix"))),
                            );
                            ui.add(
                                egui::DragValue::new(b)
                                    .prefix(format!("{} ", t!("input_settings.sub_column_prefix"))),
                            );
                        });
                    }
                }
            }
            Kind::Designed { .. } => {
                if ui.button(t!("input_settings.edit_title")).clicked() {
                    self.designer_for = Some(id);
                }
            }
            Kind::Image { path } => {
                path_row(ui, path);
                if ui.button(t!("input_settings.change_image")).clicked() {
                    change_path = rfd::FileDialog::new().pick_file();
                }
                if let Some(p) = change_path.take() {
                    *path = p.to_string_lossy().into_owned();
                }
            }
            Kind::Video { path, looped, mark_in, mark_out, rate } => {
                path_row(ui, path);
                if ui.button(t!("input_settings.change_video")).clicked()
                    && let Some(p) = rfd::FileDialog::new().pick_file()
                {
                    *path = p.to_string_lossy().into_owned();
                }
                ui.checkbox(looped, t!("input_settings.loop_playback"));
                ui.add(egui::Slider::new(rate, 0.1..=4.0).text(t!("input_settings.speed")));
                let pos = rt.as_ref().map_or(0, |r| r.pos_ms.load(Ordering::Relaxed));
                ui.horizontal(|ui| {
                    if ui.button(t!("input_settings.mark_in")).clicked() {
                        *mark_in = pos;
                    }
                    if ui.button(t!("input_settings.mark_out")).clicked() {
                        *mark_out = pos;
                    }
                    if ui.button(t!("input_settings.clear")).clicked() {
                        *mark_in = 0;
                        *mark_out = 0;
                    }
                    ui.label(
                        RichText::new(t!(
                            "input_settings.marks_range",
                            from = clock(*mark_in),
                            to = clock(*mark_out)
                        ))
                        .small()
                        .color(DIM),
                    );
                });
                if let Some(rt) = &rt {
                    video_controls(ui, rt, 440.0);
                }
            }
            Kind::AudioFile { path, looped } => {
                path_row(ui, path);
                ui.checkbox(looped, t!("input_settings.loop_playback"));
                if let Some(rt) = &rt {
                    video_controls(ui, rt, 440.0);
                }
            }
            Kind::List { items, auto_next, auto_first, looped, shuffle } => {
                ui.horizontal(|ui| {
                    ui.checkbox(auto_next, t!("input_settings.auto_next"));
                    ui.checkbox(auto_first, t!("input_settings.always_first"));
                    ui.checkbox(looped, t!("input_settings.looped"));
                    ui.checkbox(shuffle, t!("input_settings.shuffle"));
                });
                let mut remove = None;
                let mut swap = None;
                for (n, it) in items.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut it.enabled, "");
                        if ui.small_button(t!("input_settings.move_up")).clicked() && n > 0 {
                            swap = Some(n);
                        }
                        if ui.small_button(t!("input_settings.remove")).clicked() {
                            remove = Some(n);
                        }
                        let name = std::path::Path::new(&it.path).file_name();
                        ui.label(name.map_or(it.path.clone(), |s| s.to_string_lossy().into_owned()));
                    });
                }
                if let Some(n) = swap {
                    items.swap(n, n - 1);
                }
                if let Some(n) = remove {
                    items.remove(n);
                }
                if ui.button(t!("input_settings.add_files")).clicked() {
                    for p in rfd::FileDialog::new().pick_files().unwrap_or_default() {
                        items.push(ListItem { path: p.to_string_lossy().into_owned(), enabled: true });
                    }
                }
                if let Some(rt) = &rt {
                    video_controls(ui, rt, 440.0);
                }
            }
            Kind::Photos { folder, seconds } => {
                path_row(ui, folder);
                ui.add(egui::Slider::new(seconds, 0.0..=60.0).text(t!("input_settings.seconds_per_photo")));
            }
            Kind::ImageSequence { first, fps, looped } => {
                path_row(ui, first);
                ui.add(egui::Slider::new(fps, 1.0..=120.0).text(t!("input_settings.frames_per_second")));
                ui.checkbox(looped, t!("input_settings.looped"));
            }
            Kind::Network { url, buffer_ms } => {
                ui.label(t!("input_settings.stream_address"));
                ui.text_edit_singleline(url);
                ui.add(egui::Slider::new(buffer_ms, 0..=5000).text(t!("input_settings.buffer_ms")));
            }
            Kind::Ndi { source } => self.net_source_editor(ui, Net::Ndi, source),
            Kind::Omt { source } => self.net_source_editor(ui, Net::Omt, source),
            Kind::Browser { url, alpha } => {
                ui.label(t!("input_settings.page_address"));
                ui.text_edit_singleline(url);
                ui.checkbox(alpha, t!("input_settings.transparent_background"));
            }
            Kind::Camera { device, ptz_host } => {
                device_combo(ui, t!("input_settings.camera"), device, &cams);
                ui.horizontal(|ui| {
                    ui.label(t!("input_settings.ptz_address"));
                    ui.text_edit_singleline(ptz_host);
                    if ui.button(t!("input_settings.ptz_control")).clicked() {
                        self.ptz_for = Some(id);
                    }
                });
            }
            Kind::Microphone { device } => device_combo(ui, t!("input_settings.microphone"), device, &mics),
            Kind::Mix { preview, output, ms } => {
                ui.horizontal(|ui| {
                    ui.label(t!("input_settings.preview"));
                    input_combo(ui, "mix-apercu", preview, project, id);
                    ui.label(t!("input_settings.output"));
                    input_combo(ui, "mix-sortie", output, project, id);
                });
                ui.add(egui::DragValue::new(ms).range(50..=5000).suffix(" ms"));
                ui.horizontal(|ui| {
                    if ui.button(t!("input_settings.cut")).clicked() {
                        self.with(|s| s.mix_switch(id, false));
                    }
                    if ui.button(t!("input_settings.fade")).clicked() {
                        self.with(|s| s.mix_switch(id, true));
                    }
                });
            }
            Kind::Telestrator { background } => {
                ui.horizontal(|ui| {
                    ui.label(t!("input_settings.background_image"));
                    input_combo(ui, "tele-fond", background, project, id);
                });
                if ui.button(t!("input_settings.draw")).clicked() {
                    self.telestrator_for = Some(id);
                }
            }
            other => {
                ui.label(RichText::new(other.label()).color(DIM));
            }
        }
    }

    /// A source picked from those seen on the network, or typed in full for
    /// one that is not on yet.
    fn net_source_editor(&mut self, ui: &mut egui::Ui, net: Net, source: &mut String) {
        self.look_for_net_sources();
        let found = self.net_found(net);
        let list = found.as_ref().and_then(|f| f.as_ref().ok()).cloned().unwrap_or_default();
        device_combo(ui, t!("input_settings.net_source"), source, &list);
        ui.horizontal(|ui| {
            ui.label(t!("input_settings.net_exact_name"));
            ui.add(egui::TextEdit::singleline(source).desired_width(320.0));
        });
        let note = match found {
            None => t!("input_settings.net_searching").to_string(),
            Some(Err(missing)) => missing,
            Some(Ok(list)) if list.is_empty() => {
                t!("input_settings.net_none_yet", name = net.label())
            }
            Some(Ok(_)) => return,
        };
        ui.label(RichText::new(note).small().color(DIM));
    }

    fn tab_key(&mut self, ui: &mut egui::Ui, input: &mut Input, project: &Project) {
        let k = &mut input.key;
        ui.horizontal(|ui| {
            ui.selectable_value(&mut k.mode, KeyMode::Off, t!("input_settings.key.none"));
            ui.selectable_value(&mut k.mode, KeyMode::Colour, t!("input_settings.colour"));
            ui.selectable_value(&mut k.mode, KeyMode::Chroma, t!("input_settings.key.chroma"));
            ui.selectable_value(&mut k.mode, KeyMode::Luma, t!("input_settings.key.luma"));
            ui.selectable_value(&mut k.mode, KeyMode::Fill, t!("input_settings.key.fill"));
        });
        match k.mode {
            KeyMode::Off => {}
            KeyMode::Fill => {
                ui.horizontal(|ui| {
                    ui.label(t!("input_settings.key.fill_input"));
                    input_combo(ui, "cle-entree", &mut k.fill_input, project, input.id);
                });
            }
            _ => {
                if k.mode != KeyMode::Luma {
                    ui.horizontal(|ui| {
                        ui.label(t!("input_settings.key.colour_to_remove"));
                        ui.color_edit_button_srgb(&mut k.color);
                        if ui
                            .selectable_label(self.picking_colour, t!("input_settings.key.eyedropper"))
                            .clicked()
                        {
                            self.picking_colour = !self.picking_colour;
                        }
                    });
                }
                if k.mode == KeyMode::Chroma {
                    ui.horizontal(|ui| {
                        ui.label(t!("input_settings.key.auto"));
                        for (name, color, tol, soft) in [
                            (t!("input_settings.key.green_1"), [0, 255, 0], 0.25, 0.1),
                            (t!("input_settings.key.green_2"), [40, 200, 60], 0.32, 0.15),
                            (t!("input_settings.blue"), [0, 60, 255], 0.27, 0.12),
                        ] {
                            if ui.button(name).clicked() {
                                k.color = color;
                                k.tolerance = tol;
                                k.softness = soft;
                            }
                        }
                    });
                }
                ui.add(
                    egui::Slider::new(&mut k.tolerance, 0.0..=1.0).text(t!("input_settings.key.tolerance")),
                );
                ui.add(egui::Slider::new(&mut k.softness, 0.0..=0.6).text(t!("input_settings.key.softness")));
                if k.mode == KeyMode::Chroma {
                    ui.add(egui::Slider::new(&mut k.spill, 0.0..=1.0).text(t!("input_settings.key.spill")));
                }
            }
        }
        if self.picking_colour
            && let Some((tex, aspect)) = self.thumbs.get(&input.id).map(|(t, _, a)| (t.id(), *a))
        {
            ui.label(RichText::new(t!("input_settings.key.pick_hint")).small().color(DIM));
            let size = vec2(320.0, 320.0 / aspect);
            let (rect, r) = ui.allocate_exact_size(size, Sense::click());
            ui.painter().image(tex, rect, full_uv(), Color32::WHITE);
            if let (true, Some(pos)) = (r.clicked(), r.interact_pointer_pos()) {
                let u = (pos.x - rect.left()) / rect.width();
                let v = (pos.y - rect.top()) / rect.height();
                if let Some(f) = self.engine.rt(input.id).and_then(|rt| rt.latest()) {
                    let x = ((u * f.w as f32) as usize).min(f.w - 1);
                    let y = ((v * f.h as f32) as usize).min(f.h - 1);
                    let i = (y * f.w + x) * 4;
                    input.key.color = [f.data[i], f.data[i + 1], f.data[i + 2]];
                    self.picking_colour = false;
                }
            }
        }
    }

    fn tab_layers(&mut self, ui: &mut egui::Ui, input: &mut Input, project: &Project) {
        let aspect = project.out_w as f32 / project.out_h.max(1) as f32;
        let id = input.id;
        let names: Vec<String> = input
            .layers
            .iter()
            .map(|l| {
                project.inputs.iter().find(|i| i.id == l.input).map_or(String::new(), |i| i.name.clone())
            })
            .collect();
        ui.label(RichText::new(t!("input_settings.layers.hint")).small().color(DIM));
        if let Some(n) = layers_canvas(ui, &mut input.layers, &names, self.selected_layer, aspect) {
            self.selected_layer = n;
        }
        let choices: Vec<&Input> =
            project.inputs.iter().filter(|i| i.id != id && i.kind.has_picture()).collect();
        let mut remove = None;
        let mut up = None;
        for (n, l) in input.layers.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.checkbox(&mut l.visible, "");
                ui.selectable_value(&mut self.selected_layer, n, format!("{}", n + 1));
                let shown = names.get(n).cloned().unwrap_or_default();
                egui::ComboBox::from_id_salt(("couche", n)).selected_text(shown).show_ui(ui, |ui| {
                    for i in &choices {
                        ui.selectable_value(&mut l.input, i.id, &i.name);
                    }
                });
                if ui.small_button(t!("input_settings.move_up")).clicked() && n > 0 {
                    up = Some(n);
                }
                if ui.small_button(t!("input_settings.remove")).clicked() {
                    remove = Some(n);
                }
            });
        }
        if let Some(n) = up {
            input.layers.swap(n, n - 1);
        }
        if let Some(n) = remove {
            input.layers.remove(n);
        }
        ui.horizontal(|ui| {
            let room = input.layers.len() < 10;
            if ui.add_enabled(room, egui::Button::new(t!("input_settings.layers.add"))).clicked()
                && let Some(first) = choices.first()
            {
                input.layers.push(Layer::new(first.id, Placement { x: 0.1, y: 0.1, scale: 0.5 }));
                self.selected_layer = input.layers.len() - 1;
            }
            if ui.button(t!("input_settings.layers.save_template")).clicked() {
                let template = LayerTemplate {
                    name: t!("input_settings.layers.template_name", n = project.layer_templates.len() + 1),
                    layers: input.layers.clone(),
                };
                self.with(|s| s.project.layer_templates.push(template));
            }
            for t in &project.layer_templates {
                if ui.button(&t.name).clicked() {
                    // A template brings the layout, not the pictures in it.
                    for (l, m) in input.layers.iter_mut().zip(&t.layers) {
                        l.placement = m.placement;
                        l.crop = m.crop;
                        l.border = m.border;
                    }
                }
            }
        });
        if let Some(l) = input.layers.get_mut(self.selected_layer) {
            ui.separator();
            ui.label(
                RichText::new(t!("input_settings.layers.layer_n", n = self.selected_layer + 1)).strong(),
            );
            placement_editor(ui, &mut l.placement);
            for (n, name) in crop_names().into_iter().enumerate() {
                ui.add(egui::Slider::new(&mut l.crop[n], 0.0..=0.9).text(name));
            }
            border_editor(ui, &mut l.border);
        }
    }
}

fn path_row(ui: &mut egui::Ui, path: &str) {
    ui.add(egui::Label::new(RichText::new(path).small()).truncate());
}

/// Labels of the four crop sliders: left, right, top, bottom.
fn crop_names() -> [&'static str; 4] {
    [
        t!("input_settings.crop_left"),
        t!("input_settings.crop_right"),
        t!("input_settings.crop_top"),
        t!("input_settings.crop_bottom"),
    ]
}

pub(super) fn category_name(c: &Category) -> String {
    if c.name.is_empty() { t!("input_settings.unnamed").to_string() } else { c.name.clone() }
}

fn go_editor(ui: &mut egui::Ui, input: &mut Input) {
    ui.horizontal(|ui| {
        ui.label(t!("input_settings.go_button"));
        let shown = match &input.go {
            GoAction::QuickPlay => t!("input_settings.go_live").to_string(),
            GoAction::Transition(n) => t!("input_settings.transition_n", n = n + 1),
            GoAction::Shortcut { .. } => t!("input_settings.function").to_string(),
        };
        egui::ComboBox::from_id_salt("go").selected_text(shown).show_ui(ui, |ui| {
            ui.selectable_value(&mut input.go, GoAction::QuickPlay, t!("input_settings.go_live"));
            for n in 0..4 {
                ui.selectable_value(
                    &mut input.go,
                    GoAction::Transition(n),
                    t!("input_settings.transition_n", n = n + 1),
                );
            }
            // Choosing it again keeps the function already typed.
            let custom = matches!(input.go, GoAction::Shortcut { .. });
            if ui.selectable_label(custom, t!("input_settings.function")).clicked() && !custom {
                input.go = GoAction::Shortcut { function: "OverlayInput1".into(), value: String::new() };
            }
        });
        if let GoAction::Shortcut { function, value } = &mut input.go {
            ui.add(
                egui::TextEdit::singleline(function)
                    .desired_width(120.0)
                    .hint_text(t!("input_settings.function")),
            );
            ui.add(
                egui::TextEdit::singleline(value).desired_width(80.0).hint_text(t!("input_settings.value")),
            );
        }
    });
}

fn tab_colour(ui: &mut egui::Ui, input: &mut Input) {
    let c = &mut input.colour;
    ui.add(egui::Slider::new(&mut c.black, 0.0..=0.5).text(t!("input_settings.blacks")));
    ui.add(egui::Slider::new(&mut c.white, 0.5..=1.0).text(t!("input_settings.whites")));
    ui.add(egui::Slider::new(&mut c.rgb[0], 0.0..=2.0).text(t!("input_settings.red")));
    ui.add(egui::Slider::new(&mut c.rgb[1], 0.0..=2.0).text(t!("input_settings.green")));
    ui.add(egui::Slider::new(&mut c.rgb[2], 0.0..=2.0).text(t!("input_settings.blue")));
    ui.add(egui::Slider::new(&mut c.alpha, 0.0..=1.0).text(t!("input_settings.transparency")));
    ui.add(egui::Slider::new(&mut c.saturation, 0.0..=2.0).text(t!("input_settings.saturation")));
    if ui.button(t!("input_settings.reset")).clicked() {
        let keep = (c.lift, c.gamma, c.gain, c.hue);
        *c = Colour::default();
        (c.lift, c.gamma, c.gain, c.hue) = keep;
    }
}

fn tab_correction(ui: &mut egui::Ui, input: &mut Input) {
    let c = &mut input.colour;
    for (name, values, range) in [
        (t!("input_settings.lift"), &mut c.lift, -0.5..=0.5),
        (t!("input_settings.gamma"), &mut c.gamma, 0.2..=3.0),
        (t!("input_settings.gain"), &mut c.gain, 0.0..=3.0),
    ] {
        ui.label(RichText::new(name).strong());
        for (n, col) in [t!("input_settings.red"), t!("input_settings.green"), t!("input_settings.blue")]
            .into_iter()
            .enumerate()
        {
            ui.add(egui::Slider::new(&mut values[n], range.clone()).text(col));
        }
    }
    ui.add(egui::Slider::new(&mut c.hue, -180.0..=180.0).text(t!("input_settings.hue")));
    ui.add(egui::Slider::new(&mut c.saturation, 0.0..=2.0).text(t!("input_settings.saturation")));
    if ui.button(t!("input_settings.reset")).clicked() {
        let d = Colour::default();
        (c.lift, c.gamma, c.gain, c.hue, c.saturation) = (d.lift, d.gamma, d.gain, d.hue, d.saturation);
    }
}

fn tab_effects(ui: &mut egui::Ui, input: &mut Input) {
    let mut remove = None;
    let mut up = None;
    for (n, e) in input.effects.iter_mut().enumerate() {
        ui.push_id(n, |ui| {
            ui.horizontal(|ui| {
                ui.checkbox(&mut e.enabled, e.kind.label());
                if ui.small_button(t!("input_settings.move_up")).clicked() && n > 0 {
                    up = Some(n);
                }
                if ui.small_button(t!("input_settings.remove")).clicked() {
                    remove = Some(n);
                }
            });
            if e.kind != EffectKind::AlphaMask {
                ui.add(egui::Slider::new(&mut e.strength, 0.0..=1.0).text(t!("input_settings.strength")));
            }
            if matches!(e.kind, EffectKind::Lut | EffectKind::AlphaMask) {
                ui.horizontal(|ui| {
                    if ui.button(t!("input_settings.file")).clicked()
                        && let Some(p) = rfd::FileDialog::new().pick_file()
                    {
                        e.path = p.to_string_lossy().into_owned();
                    }
                    ui.add(egui::Label::new(RichText::new(&e.path).small()).truncate());
                });
            }
            for (i, name) in [
                t!("input_settings.effects.limit_left"),
                t!("input_settings.effects.limit_right"),
                t!("input_settings.effects.limit_top"),
                t!("input_settings.effects.limit_bottom"),
            ]
            .into_iter()
            .enumerate()
            {
                ui.add(egui::Slider::new(&mut e.crop[i], 0.0..=0.9).text(name));
            }
        });
        ui.separator();
    }
    if let Some(n) = up {
        input.effects.swap(n, n - 1);
    }
    if let Some(n) = remove {
        input.effects.remove(n);
    }
    ui.horizontal(|ui| {
        for kind in EffectKind::ALL {
            if ui.button(format!("+ {}", kind.label())).clicked() {
                input.effects.push(Effect { kind, enabled: true, strength: 0.4, ..Effect::default() });
            }
        }
    });
}

fn tab_position(ui: &mut egui::Ui, input: &mut Input) {
    let p = &mut input.position;
    ui.add(egui::Slider::new(&mut p.zoom, 0.05..=5.0).text(t!("input_settings.zoom")));
    ui.add(egui::Slider::new(&mut p.pan_x, -2.0..=2.0).text(t!("input_settings.horizontal")));
    ui.add(egui::Slider::new(&mut p.pan_y, -2.0..=2.0).text(t!("input_settings.vertical")));
    ui.add(
        egui::Slider::new(&mut p.rotate, -std::f32::consts::PI..=std::f32::consts::PI)
            .text(t!("input_settings.rotation")),
    );
    for (n, name) in crop_names().into_iter().enumerate() {
        ui.add(egui::Slider::new(&mut p.crop[n], 0.0..=0.9).text(name));
    }
    ui.horizontal(|ui| {
        if ui.button(t!("input_settings.rotate_90")).clicked() {
            p.quarter_turns = (p.quarter_turns + 1) % 4;
        }
        if ui.button(t!("input_settings.reset")).clicked() {
            *p = Position::default();
        }
    });
}

fn tab_triggers(ui: &mut egui::Ui, input: &mut Input) {
    ui.label(RichText::new(t!("input_settings.triggers.hint")).small().color(DIM));
    let mut remove = None;
    for (n, t) in input.triggers.iter_mut().enumerate() {
        ui.push_id(n, |ui| {
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("evenement").selected_text(t.event.label()).show_ui(ui, |ui| {
                    for e in TriggerEvent::ALL {
                        ui.selectable_value(&mut t.event, e, e.label());
                    }
                });
                if ui.small_button(t!("input_settings.remove")).clicked() {
                    remove = Some(n);
                }
            });
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut t.function)
                        .hint_text(t!("input_settings.function"))
                        .desired_width(120.0),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut t.input)
                        .hint_text(t!("input_settings.input"))
                        .desired_width(70.0),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut t.value)
                        .hint_text(t!("input_settings.value"))
                        .desired_width(70.0),
                );
                ui.add(egui::DragValue::new(&mut t.delay_ms).range(0..=30000).suffix(" ms"));
                if t.event.uses_time() {
                    ui.add(
                        egui::DragValue::new(&mut t.time_ms)
                            .speed(100)
                            .suffix(format!(" {}", t!("input_settings.triggers.ms_at"))),
                    );
                }
            });
        });
        ui.separator();
    }
    if let Some(n) = remove {
        input.triggers.remove(n);
    }
    if ui.button(t!("input_settings.triggers.add")).clicked() {
        input.triggers.push(Trigger { function: "OverlayInput1".into(), ..Trigger::default() });
    }
}

fn tab_audio(ui: &mut egui::Ui, input: &mut Input) {
    if !input.kind.has_audio() {
        ui.label(RichText::new(t!("input_settings.audio.no_audio")).color(DIM));
        return;
    }
    let a = &mut input.audio;
    ui.add(egui::Slider::new(&mut a.gain_db, -20.0..=30.0).text(t!("input_settings.audio.gain_db")));
    ui.add(egui::Slider::new(&mut a.balance, -1.0..=1.0).text(t!("input_settings.audio.balance")));
    ui.add(egui::Slider::new(&mut a.delay_ms, 0.0..=1000.0).text(t!("input_settings.audio.delay_ms")));
    ui.add(egui::Slider::new(&mut a.headphones, 0.0..=1.0).text(t!("input_settings.audio.headphones")));
    ui.horizontal(|ui| {
        ui.selectable_value(&mut a.channel, 0, t!("input_settings.audio.stereo"));
        ui.selectable_value(&mut a.channel, 1, t!("input_settings.audio.left_only"));
        ui.selectable_value(&mut a.channel, 2, t!("input_settings.audio.right_only"));
    });
    ui.horizontal(|ui| {
        ui.checkbox(&mut a.visible, t!("input_settings.audio.visible_in_mixer"));
        ui.checkbox(&mut a.pfl, t!("input_settings.audio.pfl"));
    });
    ui.separator();
    ui.label(RichText::new(t!("input_settings.audio.compressor")).strong());
    ui.add(egui::Slider::new(&mut a.comp_ratio, 1.0..=20.0).text(t!("input_settings.audio.ratio")));
    ui.add(
        egui::Slider::new(&mut a.comp_threshold, -60.0..=0.0).text(t!("input_settings.audio.threshold_db")),
    );
    ui.label(RichText::new(t!("input_settings.audio.gate")).strong());
    ui.add(
        egui::Slider::new(&mut a.gate_threshold, -90.0..=0.0).text(t!("input_settings.audio.threshold_db")),
    );
    ui.separator();
    ui.label(RichText::new(t!("input_settings.audio.equaliser")).strong());
    ui.horizontal(|ui| {
        ui.spacing_mut().slider_width = 90.0;
        for (n, band) in a.eq.iter_mut().enumerate() {
            ui.vertical(|ui| {
                ui.add(egui::Slider::new(band, -12.0..=12.0).vertical().show_value(false));
                let f = EQ_BANDS[n];
                let label = if f >= 1000.0 { format!("{}k", f / 1000.0) } else { format!("{f}") };
                ui.label(RichText::new(label).size(9.0).color(DIM));
            });
        }
    });
    if ui.button(t!("input_settings.audio.flatten")).clicked() {
        a.eq = [0.0; 10];
    }
    ui.separator();
    ui.label(RichText::new(t!("input_settings.audio.bus_levels")).strong());
    for (n, name) in
        [t!("input_settings.audio.bus_master"), "A", "B", "C", "D", "E", "F", "G"].into_iter().enumerate()
    {
        ui.add(egui::Slider::new(&mut a.bus_levels[n], 0.0..=1.2).text(name));
    }
}

fn tab_copy(ui: &mut egui::Ui, input: &Input, project: &Project) -> Option<u64> {
    ui.label(t!("input_settings.copy_from"));
    let mut chosen = None;
    for other in project.inputs.iter().filter(|i| i.id != input.id && i.kind.has_picture()) {
        if ui.button(&other.name).clicked() {
            chosen = Some(other.id);
        }
    }
    chosen
}
