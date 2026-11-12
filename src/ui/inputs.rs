//! Category tabs, input cards, folded strips and the add-input menu.

use super::*;

impl App {
    pub(super) fn tabs_row(&mut self, ui: &mut egui::Ui, project: &Project) {
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if !self.basic {
                let all = RichText::new(t!("inputs.tab_all")).size(11.0).color(Color32::WHITE);
                let mut b =
                    egui::Button::new(all).fill(Color32::from_rgb(90, 92, 98)).min_size(vec2(36.0, 19.0));
                if self.tab.is_none() {
                    b = b.stroke(Stroke::new(1.5, Color32::WHITE));
                }
                if ui.add(b).clicked() {
                    self.tab = None;
                }
                for (n, c) in project.categories.iter().enumerate().filter(|(_, c)| c.enabled) {
                    let on = self.tab == Some(n);
                    let text = RichText::new(&c.name).size(11.0).color(Color32::WHITE);
                    let width = if c.name.is_empty() { 26.0 } else { 0.0 };
                    let mut b = egui::Button::new(text).fill(rgb(c.color)).min_size(vec2(width, 19.0));
                    if on {
                        b = b.stroke(Stroke::new(1.5, Color32::WHITE));
                    }
                    let r = ui.add(b);
                    if r.clicked() {
                        self.tab = if on { None } else { Some(n) };
                    }
                    if r.secondary_clicked() {
                        self.categories_open = true;
                    }
                    // Dropping a card on a tab files it there.
                    if let Some(id) = r.dnd_release_payload::<u64>() {
                        let id = *id;
                        self.with(|s| {
                            if let Some(i) = s.input_mut(id) {
                                i.category = Some(n);
                            }
                        });
                    }
                }
                ui.add_space(6.0);
            }
            let search = egui::TextEdit::singleline(&mut self.search).hint_text(t!("inputs.search"));
            ui.add(search.desired_width(110.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !self.basic {
                    let label = RichText::new(t!("inputs.audio_mixer")).size(11.0).color(Color32::WHITE);
                    let fill = if self.audio_open { GREEN } else { BTN };
                    let b = egui::Button::new(label).fill(fill).min_size(vec2(0.0, 19.0));
                    if ui.add(b).clicked() {
                        self.audio_open = !self.audio_open;
                    }
                    let label = RichText::new(t!("inputs.replay")).size(11.0).color(Color32::WHITE);
                    let fill = if self.replay_open { GREEN } else { BTN };
                    let b = egui::Button::new(label).fill(fill).min_size(vec2(0.0, 19.0));
                    if ui.add(b).clicked() {
                        self.replay_open = !self.replay_open;
                    }
                }
            });
        });
        if let Some(n) = self.tab {
            let color = project.categories.get(n).map_or(LINE, |c| rgb(c.color));
            let r = ui.max_rect();
            let line = Rect::from_min_max(pos2(r.left(), r.bottom() - 2.0), r.max);
            ui.painter().rect_filled(line, 0.0, color);
        }
    }

    pub(super) fn inputs_panel(&mut self, ui: &mut egui::Ui, project: &Project) {
        let ctx = ui.ctx().clone();
        let query = self.search.trim().to_lowercase();
        if project.inputs.is_empty() {
            ui.centered_and_justified(|ui| {
                let hint = t!("inputs.empty_hint");
                ui.label(RichText::new(hint).color(DIM));
            });
            return;
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let wrap = egui::Layout::left_to_right(egui::Align::TOP).with_main_wrap(true);
            ui.with_layout(wrap, |ui| {
                ui.spacing_mut().item_spacing = vec2(4.0, 5.0);
                for (n, input) in project.inputs.iter().enumerate() {
                    // Simple mode has no tabs: a tab chosen before must not
                    // hide inputs with nothing to bring them back.
                    if !self.basic && self.tab.is_some_and(|t| input.category != Some(t)) {
                        continue;
                    }
                    if !query.is_empty() && !input.name.to_lowercase().contains(&query) {
                        continue;
                    }
                    if input.compact {
                        self.input_strip(ui, n, input, project);
                    } else {
                        self.input_card(ui, &ctx, n, input, project);
                    }
                }
            });
        });
    }

    fn header_color(project: &Project, id: u64) -> Color32 {
        if project.program == Some(id) {
            GREEN
        } else if project.preview == Some(id) {
            ORANGE
        } else {
            BLUE
        }
    }

    fn click_input(&mut self, input: &Input) {
        let id = input.id;
        match input.click {
            ClickAction::Preview => self.with(|s| s.set_preview(id)),
            ClickAction::QuickPlay => self.with(|s| s.quick_play(id)),
            ClickAction::Cut => self.with(|s| s.cut_to(id)),
            ClickAction::Overlay(n) => self.with(|s| s.toggle_overlay(n, id)),
        }
    }

    pub(super) fn move_input(&mut self, moved: u64, before: u64) {
        self.with(|s| {
            let list = &mut s.project.inputs;
            let (Some(from), Some(to)) =
                (list.iter().position(|i| i.id == moved), list.iter().position(|i| i.id == before))
            else {
                return;
            };
            let item = list.remove(from);
            // Taking it out moved everything after it one place back.
            list.insert(to - usize::from(from < to), item);
            s.version += 1;
        });
    }

    /// A folded input: a narrow bar with its name written upwards.
    fn input_strip(&mut self, ui: &mut egui::Ui, n: usize, input: &Input, project: &Project) {
        let h = self.card_height();
        let (rect, resp) = ui.allocate_exact_size(vec2(STRIP_W, h), Sense::click());
        ui.painter().rect_filled(rect, 1.0, Self::header_color(project, input.id));
        ui.painter().text(
            rect.center_top() + vec2(0.0, 11.0),
            egui::Align2::CENTER_CENTER,
            format!("{}", n + 1),
            egui::FontId::proportional(11.0),
            Color32::WHITE,
        );
        vertical_text(ui, rect, &input.name);
        let id = input.id;
        let resp = named(resp, t!("inputs.input_named", name = input.name));
        let resp = resp.on_hover_text(t!("inputs.strip_tip"));
        if resp.secondary_clicked() {
            self.with(|s| {
                if let Some(i) = s.input_mut(id) {
                    i.compact = false;
                }
            });
        } else if resp.double_clicked() && matches!(input.click, ClickAction::Preview) {
            self.editing = Some(id);
        } else if resp.clicked() {
            self.click_input(input);
        }
    }

    fn input_card(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        n: usize,
        input: &Input,
        project: &Project,
    ) {
        let id = input.id;
        let tex = self.thumb(ctx, id);
        let rt = self.engine.rt(id);
        let card_h = self.card_height();
        let (rect, resp) = ui.allocate_exact_size(vec2(CARD_W, card_h), Sense::click_and_drag());
        let head = Rect::from_min_size(rect.min, vec2(CARD_W, 19.0));
        let pic_h = card_h - 19.0 - if self.basic { 0.0 } else { 36.0 };
        let pic = Rect::from_min_size(pos2(rect.left(), head.bottom()), vec2(CARD_W, pic_h));
        let p = ui.painter();
        let small = egui::FontId::proportional(11.0);
        p.rect_filled(rect, 1.0, CARD);
        p.rect_filled(head, 1.0, Self::header_color(project, id));
        let number = format!("{}", n + 1);
        let left = egui::Align2::LEFT_CENTER;
        p.text(head.left_center() + vec2(5.0, 0.0), left, number, small.clone(), Color32::WHITE);
        p.text(head.left_center() + vec2(24.0, 0.0), left, &input.name, small, Color32::WHITE);
        p.rect_filled(pic, 0.0, Color32::BLACK);
        match tex {
            Some((t, aspect)) => {
                p.image(t, fit(pic, aspect), full_uv(), Color32::WHITE);
            }
            None => {
                p.text(
                    pic.center(),
                    egui::Align2::CENTER_CENTER,
                    input.kind.label(),
                    egui::FontId::proportional(13.0),
                    DIM,
                );
            }
        }
        if let Some(msg) = rt.as_ref().and_then(|r| r.status()) {
            let shown: String = msg.chars().take(38).collect();
            let at = pic.left_bottom() + vec2(4.0, -8.0);
            p.text(at, left, shown, egui::FontId::proportional(10.0), ORANGE);
        }
        if let (true, Some(rt)) = (input.kind.has_audio(), &rt) {
            let meter =
                Rect::from_min_max(pic.right_top() + vec2(-6.0, 2.0), pic.right_bottom() - vec2(1.0, 2.0));
            draw_meter(ui, meter, rt.peak(), input.audio.muted);
        }
        let resp = named(resp, t!("inputs.input_named", name = input.name));
        let resp = resp.on_hover_text(t!("inputs.card_tip"));
        // Header's X closes; right-clicking the title folds the card.
        let on_title =
            |r: &egui::Response| r.ctx.input(|i| i.pointer.interact_pos()).is_some_and(|p| head.contains(p));
        if resp.secondary_clicked() && on_title(&resp) {
            self.with(|s| {
                if let Some(i) = s.input_mut(id) {
                    i.compact = true;
                }
            });
        } else if resp.double_clicked() && (on_title(&resp) || matches!(input.click, ClickAction::Preview)) {
            // A click that acts on air stays a click, however fast it comes;
            // the title bar still opens the settings.
            self.editing = Some(id);
        } else if resp.clicked() {
            self.click_input(input);
        }
        resp.dnd_set_drag_payload(id);
        if let Some(moved) = resp.dnd_release_payload::<u64>()
            && *moved != id
        {
            self.move_input(*moved, id);
        }
        resp.context_menu(|ui| self.input_menu(ui, input));

        let close = Rect::from_center_size(head.right_center() - vec2(10.0, 0.0), vec2(16.0, 16.0));
        let x = ui.interact(close, ui.scope_id().with(("fermer", id)), Sense::click());
        ui.painter().text(
            close.center(),
            egui::Align2::CENTER_CENTER,
            "×",
            egui::FontId::proportional(14.0),
            Color32::WHITE,
        );
        let x = named(x, t!("inputs.close_named", name = input.name));
        if x.on_hover_text(t!("inputs.close_tip")).clicked() {
            if self.with(|s| s.padlock) {
                self.say(t!("inputs.locked_close"));
            } else {
                self.engine.close_input(id);
                self.thumbs.remove(&id);
                if self.editing == Some(id) {
                    self.editing = None;
                }
            }
        }
        if self.basic {
            return;
        }
        self.card_buttons(ui, rect, pic, input, project);
    }

    fn card_buttons(&mut self, ui: &mut egui::Ui, rect: Rect, pic: Rect, input: &Input, project: &Project) {
        let id = input.id;
        let rt = self.engine.rt(id);
        let row1 = Rect::from_min_size(pos2(rect.left() + 2.0, pic.bottom() + 1.0), vec2(CARD_W - 4.0, 17.0));
        let row2 = row1.translate(vec2(0.0, 18.0));
        let layout = egui::Layout::left_to_right(egui::Align::Center);
        let mut top =
            ui.new_child(egui::UiBuilder::new().id_salt((id, "haut")).max_rect(row1).layout(layout));
        top.spacing_mut().item_spacing.x = 1.0;
        top.spacing_mut().button_padding = vec2(5.0, 0.0);
        let go_tip = match &input.go {
            GoAction::QuickPlay => t!("inputs.go_quick_play").to_string(),
            GoAction::Transition(n) => t!("inputs.go_transition", n = n + 1),
            GoAction::Shortcut { function, .. } => t!("inputs.go_function", function = function),
        };
        let go = named(tiny(&mut top, "GO", None), format!("GO {}", input.name));
        if go.on_hover_text(go_tip).clicked() {
            self.run_go(input);
        }
        let cut = named(tiny(&mut top, t!("inputs.cut"), None), t!("inputs.cut_named", name = input.name));
        if cut.clicked() {
            self.with(|s| s.cut_to(id));
        }
        if let Kind::Video { looped, .. } = input.kind {
            let on = looped.then_some(GREEN);
            if icon_button(&mut top, Icon::Loop, on).on_hover_text(t!("inputs.loop")).clicked() {
                self.call("Loop", &Args::new().input(&(self.input_number(project, id))));
            }
        }
        if let Kind::VirtualSet { cameras, camera, .. } = &input.kind {
            for (n, cam) in cameras.iter().enumerate().take(6) {
                let on = (*camera == n).then_some(GREEN);
                let r = named(
                    tiny(&mut top, &format!("C{}", n + 1), on),
                    t!("inputs.set_camera_named", n = n + 1, name = input.name),
                );
                if r.on_hover_text(crate::sets::camera_label(&cam.name)).clicked() {
                    self.with(|s| s.set_camera(id, n, None, Instant::now()));
                }
            }
        }
        if let (true, Some(rt)) = (input.kind.has_timeline(), &rt) {
            let playing = rt.is_playing();
            let icon = if playing { Icon::Pause } else { Icon::Play };
            if icon_button(&mut top, icon, None).clicked() {
                rt.playing.store(!playing, Ordering::Relaxed);
            }
            if icon_button(&mut top, Icon::Restart, None).clicked() {
                rt.seek_ms.store(0, Ordering::Relaxed);
            }
        }
        let mut bottom =
            ui.new_child(egui::UiBuilder::new().id_salt((id, "bas")).max_rect(row2).layout(layout));
        bottom.spacing_mut().item_spacing.x = 0.0;
        bottom.spacing_mut().button_padding = vec2(4.0, 0.0);
        for slot in 0..OVERLAY_CHANNELS {
            let on = (project.overlays[slot] == Some(id)).then_some(GREEN);
            let r = tiny(&mut bottom, &format!("{}", slot + 1), on);
            let r = named(r, t!("inputs.overlay_named", n = slot + 1, name = input.name));
            if r.on_hover_text(t!("inputs.overlay_tip", n = slot + 1)).clicked() {
                self.with(|s| s.toggle_overlay(slot, id));
            }
        }
        bottom.add_space(4.0);
        let has_audio = input.kind.has_audio();
        let on = (has_audio && !input.audio.muted).then_some(GREEN);
        let audio = bottom.add_enabled_ui(has_audio, |ui| tiny(ui, t!("inputs.audio"), on)).inner;
        let audio = named(audio, t!("inputs.audio_named", name = input.name));
        if audio.clicked() {
            self.with(|s| {
                if let Some(a) = s.audio_mut(id) {
                    a.muted = !a.muted;
                }
            });
        }
        bottom.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let category =
                input.category.and_then(|n| project.categories.get(n)).map_or(BTN, |c| rgb(c.color));
            let g = named(gear(ui, category), t!("inputs.settings_named", name = input.name));
            let g = g.on_hover_text(t!("inputs.settings_tip"));
            if g.secondary_clicked() {
                self.take_snapshot(Some(id));
            } else if g.clicked() {
                self.editing = Some(id);
            }
        });
    }

    fn input_number(&self, project: &Project, id: u64) -> String {
        let at = project.inputs.iter().position(|i| i.id == id);
        at.map_or(String::new(), |n| (n + 1).to_string())
    }

    pub(super) fn run_go(&mut self, input: &Input) {
        let id = input.id;
        match &input.go {
            GoAction::QuickPlay => self.with(|s| s.quick_play(id)),
            GoAction::Transition(n) => {
                let n = *n;
                self.with(|s| {
                    s.set_preview(id);
                    s.auto(n);
                });
            }
            GoAction::Shortcut { function, value } => {
                let target = id.to_string();
                let number = self.with(|s| {
                    s.project.inputs.iter().position(|i| i.id == id).map_or(target, |n| (n + 1).to_string())
                });
                self.call(function, &Args::new().input(&number).value(value));
            }
        }
    }

    fn input_menu(&mut self, ui: &mut egui::Ui, input: &Input) {
        let id = input.id;
        ui.set_min_width(180.0);
        if ui.button(t!("inputs.menu_settings")).clicked() {
            self.editing = Some(id);
            ui.close();
        }
        if ui.button(t!("inputs.menu_snapshot")).clicked() {
            self.take_snapshot(Some(id));
            ui.close();
        }
        if ui.button(t!("inputs.menu_duplicate")).clicked() {
            let mut copy = input.clone();
            copy.id = self.engine.new_id();
            copy.name = t!("inputs.virtual_copy", name = input.name);
            self.engine.add_input(copy);
            ui.close();
        }
        match &input.kind {
            Kind::Camera { .. } => {
                if ui.button(t!("inputs.menu_ptz")).clicked() {
                    self.ptz_for = Some(id);
                    ui.close();
                }
            }
            Kind::Telestrator { .. } => {
                if ui.button(t!("inputs.menu_draw")).clicked() {
                    self.telestrator_for = Some(id);
                    ui.close();
                }
            }
            Kind::Designed { .. } => {
                if ui.button(t!("inputs.menu_edit_title")).clicked() {
                    self.designer_for = Some(id);
                    ui.close();
                }
            }
            Kind::VirtualSet { cameras, .. } => {
                for (n, cam) in cameras.iter().enumerate() {
                    if ui
                        .button(t!(
                            "inputs.set_camera_menu",
                            n = n + 1,
                            name = crate::sets::camera_label(&cam.name)
                        ))
                        .clicked()
                    {
                        self.with(|s| s.set_camera(id, n, None, Instant::now()));
                        ui.close();
                    }
                }
            }
            Kind::Title { presets, style, .. } => {
                if *style == TitleStyle::Countdown {
                    for (label, f) in [
                        (t!("inputs.countdown_start"), "StartCountdown"),
                        (t!("inputs.countdown_pause"), "PauseCountdown"),
                        (t!("inputs.countdown_reset"), "StopCountdown"),
                    ] {
                        if ui.button(label).clicked() {
                            if let Some(rt) = self.engine.rt(id) {
                                match f {
                                    "StartCountdown" => rt.playing.store(true, Ordering::Relaxed),
                                    "PauseCountdown" => rt.playing.store(false, Ordering::Relaxed),
                                    _ => {
                                        rt.playing.store(false, Ordering::Relaxed);
                                        rt.seek_ms.store(0, Ordering::Relaxed);
                                    }
                                }
                            }
                            ui.close();
                        }
                    }
                }
                for p in presets {
                    if ui.button(&p.name).clicked() {
                        let (text, sub) = (p.text.clone(), p.sub.clone());
                        self.with(|s| {
                            if let Some(Kind::Title { text: t, sub: u, .. }) =
                                s.input_mut(id).map(|i| &mut i.kind)
                            {
                                *t = text;
                                *u = sub;
                            }
                        });
                        self.engine.restart_input(id, true);
                        ui.close();
                    }
                }
            }
            Kind::Photos { .. } | Kind::List { .. } => {
                let count = self.engine.rt(id).map_or(0, |r| r.items.load(Ordering::Relaxed));
                for n in 0..count.min(30) {
                    if ui.button(t!("inputs.menu_item", n = n + 1)).clicked() {
                        if let Some(rt) = self.engine.rt(id) {
                            rt.select.store(n as i64, Ordering::Relaxed);
                        }
                        ui.close();
                    }
                }
            }
            _ => {}
        }
    }

    fn add_input(&mut self, name: &str, kind: Kind) -> u64 {
        let id = self.engine.new_id();
        let category = self.tab;
        let mut input = Input::new(id, name, kind);
        input.category = category;
        self.engine.add_input(input);
        id
    }

    /// Makes the right kind of input from a file.
    pub(super) fn add_file(&mut self, p: &std::path::Path) {
        let name = file_name(p);
        let path = p.to_string_lossy().into_owned();
        let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        match ext.as_str() {
            "png" | "jpg" | "jpeg" | "webp" | "bmp" => {
                self.add_input(&name, Kind::Image { path });
            }
            "mp3" | "wav" | "m4a" | "flac" | "ogg" | "aac" => {
                self.add_input(&name, Kind::AudioFile { path, looped: false });
            }
            "livecraft" | "regie" | "vmix" => {
                if self.with(|s| s.padlock) {
                    self.say(t!("inputs.locked_open_project"));
                } else if self.with(|s| s.project.inputs.is_empty()) {
                    self.open_path(path);
                } else {
                    self.confirm_open = Some(path);
                }
            }
            _ if p.is_dir() => {
                self.add_input(&name, Kind::Photos { folder: path, seconds: 0.0 });
            }
            _ => {
                let kind = Kind::Video { path, looped: false, mark_in: 0, mark_out: 0, rate: 1.0 };
                self.add_input(&name, kind);
            }
        }
    }

    pub(super) fn add_menu(&mut self, ui: &mut egui::Ui) {
        let button = egui::Button::new(RichText::new(t!("inputs.add_input")).size(12.0).color(TEXT))
            .fill(BTN)
            .min_size(vec2(130.0, 22.0));
        let (cams, mics) = self.devices.lock().unwrap().clone();
        let (opened, _) = egui::containers::menu::MenuButton::from_button(button).ui(ui, |ui| {
            ui.set_min_width(230.0);
            if ui.button(t!("inputs.add_video")).clicked() {
                let picked = rfd::FileDialog::new()
                    .add_filter(
                        t!("inputs.filter_video"),
                        &["mp4", "mov", "mkv", "avi", "webm", "m4v", "ts", "mxf", "wmv", "mpg"],
                    )
                    .pick_files();
                for p in picked.unwrap_or_default() {
                    self.add_file(&p);
                }
                ui.close();
            }
            if ui.button(t!("inputs.add_video_list")).clicked() {
                let picked = rfd::FileDialog::new().pick_files().unwrap_or_default();
                let items: Vec<ListItem> = picked
                    .iter()
                    .map(|p| ListItem { path: p.to_string_lossy().into_owned(), enabled: true })
                    .collect();
                if !items.is_empty() {
                    let kind = Kind::List {
                        items,
                        auto_next: true,
                        auto_first: false,
                        looped: false,
                        shuffle: false,
                    };
                    self.add_input(t!("inputs.list_name"), kind);
                }
                ui.close();
            }
            ui.menu_button(t!("inputs.add_camera"), |ui| {
                if cams.is_empty() {
                    ui.label(RichText::new(t!("inputs.no_camera")).color(DIM));
                }
                for c in &cams {
                    if ui.button(c).clicked() {
                        self.add_input(c, Kind::Camera { device: c.clone(), ptz_host: String::new() });
                        ui.close();
                    }
                }
            });
            if ui.button(t!("inputs.add_network")).clicked() {
                let kind = Kind::Network { url: "srt://".into(), buffer_ms: 0 };
                let id = self.add_input(t!("inputs.network_name"), kind);
                self.editing = Some(id);
                ui.close();
            }
            if ui.button(t!("inputs.web_page")).clicked() {
                let kind = Kind::Browser { url: "https://".into(), alpha: false };
                let id = self.add_input(t!("inputs.web_page"), kind);
                self.editing = Some(id);
                ui.close();
            }
            if ui.button(t!("inputs.add_video_call")).on_hover_text(t!("inputs.add_video_call_tip")).clicked()
            {
                let id = self.add_input(t!("inputs.video_call_name"), crate::call::new_call());
                self.editing = Some(id);
                self.edit_tab = 0;
                ui.close();
            }
            if ui.button(t!("inputs.add_screen")).clicked() {
                self.add_input(t!("inputs.screen_name"), Kind::Screen);
                ui.close();
            }
            ui.separator();
            if ui.button(t!("inputs.add_image")).clicked() {
                let picked = rfd::FileDialog::new()
                    .add_filter(t!("inputs.filter_image"), &["png", "jpg", "jpeg", "webp", "bmp"])
                    .pick_files();
                for p in picked.unwrap_or_default() {
                    self.add_file(&p);
                }
                ui.close();
            }
            if ui.button(t!("inputs.add_photos")).clicked() {
                if let Some(p) = rfd::FileDialog::new().pick_folder() {
                    self.add_file(&p);
                }
                ui.close();
            }
            if ui.button(t!("inputs.add_image_sequence")).clicked() {
                if let Some(p) = rfd::FileDialog::new().pick_file() {
                    let first = p.to_string_lossy().into_owned();
                    self.add_input(&file_name(&p), Kind::ImageSequence { first, fps: 30.0, looped: true });
                }
                ui.close();
            }
            ui.separator();
            ui.menu_button(t!("inputs.add_audio_input"), |ui| {
                if mics.is_empty() {
                    ui.label(RichText::new(t!("inputs.no_microphone")).color(DIM));
                }
                for m in &mics {
                    if ui.button(m).clicked() {
                        self.add_input(m, Kind::Microphone { device: m.clone() });
                        ui.close();
                    }
                }
                ui.separator();
                let system =
                    ui.button(t!("inputs.system_audio")).on_hover_text(t!("inputs.system_audio_tip"));
                if system.clicked() {
                    let id = self.add_input(t!("inputs.system_audio"), Kind::SystemAudio);
                    self.with(|s| {
                        if let Some(i) = s.input_mut(id) {
                            i.audio.muted = true;
                        }
                    });
                    ui.close();
                }
            });
            if ui.button(t!("inputs.add_audio_file")).clicked() {
                let picked = rfd::FileDialog::new()
                    .add_filter(t!("inputs.filter_audio"), &["mp3", "wav", "m4a", "flac", "ogg", "aac"])
                    .pick_files();
                for p in picked.unwrap_or_default() {
                    self.add_file(&p);
                }
                ui.close();
            }
            ui.separator();
            ui.menu_button(t!("inputs.add_title"), |ui| {
                let title = |style| Kind::Title {
                    text: t!("inputs.title_default_text").into(),
                    sub: t!("inputs.title_default_sub").into(),
                    style,
                    color: [255, 255, 255],
                    accent: [240, 120, 40],
                    ticker_speed: 4.0,
                    countdown_ms: 300_000,
                    font: String::new(),
                    size: 0.0,
                    presets: Vec::new(),
                    data: None,
                };
                for (label, style) in [
                    (t!("inputs.title_lower_third"), TitleStyle::LowerThird),
                    (t!("inputs.title_centred"), TitleStyle::Centered),
                    (t!("inputs.title_ticker"), TitleStyle::Ticker),
                    (t!("inputs.title_countdown"), TitleStyle::Countdown),
                ] {
                    if ui.button(label).clicked() {
                        self.add_input(label, title(style));
                        ui.close();
                    }
                }
                if ui.button(t!("inputs.title_custom")).clicked() {
                    let id = self.add_input(
                        t!("inputs.title_custom_name"),
                        Kind::Designed { objects: Vec::new(), anim_ms: 500 },
                    );
                    self.designer_for = Some(id);
                    ui.close();
                }
            });
            if ui.button(t!("inputs.add_scene")).clicked() {
                let id = self.add_input(t!("inputs.scene_name"), Kind::Scene);
                self.editing = Some(id);
                self.edit_tab = 6;
                ui.close();
            }
            if ui.button(t!("inputs.add_mix")).clicked() {
                let kind = Kind::Mix { preview: None, output: None, ms: 1000 };
                let id = self.add_input(t!("inputs.mix_name"), kind);
                self.editing = Some(id);
                ui.close();
            }
            if ui.button(t!("inputs.telestrator")).clicked() {
                let id = self.add_input(t!("inputs.telestrator"), Kind::Telestrator { background: None });
                self.telestrator_for = Some(id);
                ui.close();
            }
            if ui.button(t!("inputs.add_set")).clicked() {
                let id = self.add_input(t!("inputs.set_name"), crate::sets::new_set(SetLook::Studio));
                self.editing = Some(id);
                self.edit_tab = 0;
                ui.close();
            }
            ui.separator();
            ui.menu_button(t!("inputs.add_colour_bars"), |ui| {
                if ui.button(t!("inputs.colour")).clicked() {
                    self.add_input(t!("inputs.colour"), Kind::Solid { color: [24, 62, 120] });
                    ui.close();
                }
                if ui.button(t!("inputs.bars")).clicked() {
                    self.add_input(t!("inputs.bars"), Kind::Bars);
                    ui.close();
                }
                if ui.button(t!("inputs.motion_bars")).clicked() {
                    self.add_input(t!("inputs.motion_bars"), Kind::Motion);
                    ui.close();
                }
                if ui.button(t!("inputs.clock")).clicked() {
                    self.add_input(t!("inputs.clock"), Kind::Clock);
                    ui.close();
                }
                if ui.button(t!("inputs.test_tone")).clicked() {
                    let id = self.add_input(t!("inputs.test_tone"), Kind::Tone);
                    self.with(|s| {
                        if let Some(i) = s.input_mut(id) {
                            i.audio.muted = true;
                        }
                    });
                    ui.close();
                }
            });
        });
        if opened.clicked() {
            self.refresh_devices();
        }
    }
}

const OVERLAY_CHANNELS: usize = crate::engine::OVERLAYS;
