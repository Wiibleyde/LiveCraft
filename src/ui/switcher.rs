//! The two monitors and the transition column between them.

use super::*;

impl App {
    pub(super) fn monitors(&mut self, ui: &mut egui::Ui, project: &Project) {
        let area = ui.available_rect_before_wrap();
        let half = (area.width() - MID_W) / 2.0;
        let aspect = project.out_w as f32 / project.out_h.max(1) as f32;
        const CONTROLS_H: f32 = 22.0;
        let usable_h = area.height() - HEADER_H - CONTROLS_H;
        let mon_w = (half - 24.0).min(usable_h * aspect).max(120.0);
        let mon_h = mon_w / aspect + HEADER_H;
        let top = area.top() + ((area.height() - mon_h - CONTROLS_H) / 2.0).max(0.0);
        let left = area.left() + (half - mon_w) / 2.0;
        let right = area.right() - half + (half - mon_w) / 2.0;

        let name_of = |id: Option<u64>| {
            id.and_then(|i| project.inputs.iter().find(|x| x.id == i))
                .map_or(String::new(), |i| i.name.clone())
        };
        let l = Rect::from_min_size(pos2(left, top), vec2(mon_w, mon_h));
        let r = Rect::from_min_size(pos2(right, top), vec2(mon_w, mon_h));
        let safe = self.safe_area;
        let tex = self.preview_tex.as_ref();
        let name = name_of(project.preview);
        let clicks = draw_monitor(ui, l, tex, &name, ORANGE, "apercu", true, safe);
        if clicks.gear {
            self.editing = project.preview;
        }
        if clicks.safe {
            self.safe_area = (self.safe_area + 1) % SAFE_AREAS;
        }
        if clicks.scopes {
            self.scope_open = !self.scope_open;
        }
        let tex = self.program_tex.as_ref();
        let name = name_of(project.program);
        if draw_monitor(ui, r, tex, &name, GREEN, "direct", false, 0).gear {
            self.editing = project.program;
        }
        // Inputs with a timeline get position and play controls underneath.
        for (rect, id, salt) in [(l, project.preview, "ctl-apercu"), (r, project.program, "ctl-direct")] {
            let timed =
                id.and_then(|i| project.inputs.iter().find(|x| x.id == i)).filter(|i| i.kind.has_timeline());
            if let Some(rt) = timed.and_then(|i| self.engine.rt(i.id)) {
                let bar = Rect::from_min_size(rect.left_bottom() + vec2(0.0, 2.0), vec2(rect.width(), 20.0));
                let mut c = ui.new_child(egui::UiBuilder::new().id_salt(salt).max_rect(bar));
                video_controls(&mut c, &rt, bar.width());
            }
        }

        let col = Rect::from_min_size(
            pos2(area.center().x - MID_W / 2.0 + 8.0, area.top()),
            vec2(MID_W - 16.0, area.height()),
        );
        let layout = egui::Layout::top_down(egui::Align::Center);
        let builder = egui::UiBuilder::new().id_salt("transitions").max_rect(col).layout(layout);
        let mut mid = ui.new_child(builder);
        self.transition_panel(&mut mid, project);
    }

    fn transition_panel(&mut self, ui: &mut egui::Ui, project: &Project) {
        let width = ui.available_width();
        let clocks = project.show_clocks;
        let fixed = if clocks { 150.0 } else { 80.0 };
        let btn_h = ((ui.available_height() - fixed) / 7.0 - 5.0).clamp(20.0, 30.0);
        ui.spacing_mut().item_spacing = vec2(3.0, 5.0);
        ui.add_space(2.0);
        let quick = column_button(ui, "Passer au direct", width, btn_h, false);
        if quick.on_hover_text("Envoie l'aperçu à la sortie avec la transition rapide").clicked() {
            self.with(State::quick_play_preview);
        }
        if column_button(ui, "Coupe", width, btn_h, false).clicked() {
            self.with(State::cut);
        }
        for slot in 0..4 {
            self.transition_button(ui, project, slot, width, btn_h);
        }
        let ftb = self.with(|s| s.ftb);
        if column_button(ui, "Fondu au noir", width, btn_h, ftb).clicked() {
            self.with(|s| s.ftb = !s.ftb);
        }
        // Overlay channels: 1 to 8, on with the input in Preview, or off.
        for row in 0..2 {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                ui.add_space((width - 4.0 * 24.0 - 6.0) / 2.0);
                for slot in row * 4..row * 4 + 4 {
                    self.overlay_button(ui, project, slot);
                }
            });
        }
        let mut t = self.with(|s| s.tbar);
        ui.spacing_mut().slider_width = width - 6.0;
        let bar = egui::Slider::new(&mut t, 0.0..=1.0).show_value(false);
        let r = ui.add(bar).on_hover_text("Transition à la main, avec l'effet du premier bouton");
        if r.drag_stopped() && t >= 0.98 {
            self.with(|s| s.set_tbar(1.0));
        } else if r.changed() {
            self.with(|s| s.set_tbar(t.min(0.99)));
        }
        if clocks {
            self.production_clocks(ui, project, width);
        }
    }

    fn transition_button(&mut self, ui: &mut egui::Ui, project: &Project, slot: usize, width: f32, h: f32) {
        let t = project.trans[slot];
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 1.0;
            if column_button(ui, &t.kind.label(), width - 17.0, h, false).clicked() {
                self.with(|s| s.auto(slot));
            }
            let open = egui::Button::new("").fill(BTN).min_size(vec2(16.0, h));
            let menu = egui::containers::menu::MenuButton::from_button(open);
            let (resp, _) = menu.ui(ui, |ui| {
                ui.set_min_width(170.0);
                let mut ms = t.ms;
                let d = egui::DragValue::new(&mut ms).range(50..=10000).speed(10).suffix(" ms");
                if ui.add(d).changed() {
                    self.with(|s| s.project.trans[slot].ms = ms);
                }
                ui.separator();
                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    for kind in TransKind::ALL {
                        if ui.selectable_label(t.kind == kind, kind.label()).clicked() {
                            self.with(|s| s.project.trans[slot].kind = kind);
                            ui.close();
                        }
                    }
                    ui.separator();
                    for n in 0..8 {
                        let kind = TransKind::Stinger(n);
                        if ui.selectable_label(t.kind == kind, kind.label()).clicked() {
                            self.with(|s| s.project.trans[slot].kind = kind);
                            ui.close();
                        }
                    }
                });
            });
            draw_down_arrow(ui, resp.rect.center());
        });
    }

    fn overlay_button(&mut self, ui: &mut egui::Ui, project: &Project, slot: usize) {
        let on = project.overlays[slot].is_some();
        let fill = if on { GREEN } else { BTN };
        let label = RichText::new(format!("{}", slot + 1)).size(11.0).color(Color32::WHITE);
        let b = egui::Button::new(label).fill(fill).min_size(vec2(24.0, 18.0));
        let name = |id: Option<u64>| {
            id.and_then(|id| project.inputs.iter().find(|i| i.id == id)).map(|i| i.name.clone())
        };
        let shown = name(project.overlays[slot])
            .or_else(|| name(project.overlay_last[slot]).map(|n| format!("Dernière : {n}")))
            .unwrap_or_else(|| "Prendra l'entrée en aperçu".to_string());
        let r = ui.add(b).on_hover_text(shown);
        let r = named(r, format!("Canal {}", slot + 1));
        if r.clicked() {
            self.with(|s| s.overlay_button(slot));
        }
    }

    /// The panel of two clocks below the T-bar.
    fn production_clocks(&mut self, ui: &mut egui::Ui, project: &Project, width: f32) {
        let (rect, resp) = ui.allocate_exact_size(vec2(width - 6.0, 62.0), Sense::click());
        if resp.on_hover_text("Double-clic : réglages des horloges").double_clicked() {
            self.clocks_open = true;
        }
        let half = rect.height() / 2.0;
        for (n, cfg) in project.clocks.iter().enumerate() {
            let (text, tint) = self.clock_text(cfg, project);
            let r = Rect::from_min_size(rect.min + vec2(0.0, n as f32 * half), vec2(rect.width(), half));
            let (bg, fg) =
                if cfg.inverted { (Color32::WHITE, Color32::BLACK) } else { (Color32::BLACK, tint) };
            ui.painter().rect_filled(r, 0.0, bg);
            let size = if n == 0 { 20.0 } else { 15.0 };
            ui.painter().text(
                r.center(),
                egui::Align2::CENTER_CENTER,
                text,
                egui::FontId::proportional(size),
                fg,
            );
        }
        ui.painter().rect_stroke(rect, 0.0, Stroke::new(2.0, Color32::WHITE), egui::StrokeKind::Inside);
    }

    fn clock_text(&self, c: &ClockCfg, project: &Project) -> (String, Color32) {
        let white = Color32::WHITE;
        let span = |secs: f64| format_span(secs, c.precision);
        match c.kind {
            ClockType::Clock => {
                let now = chrono::Local::now() + chrono::Duration::hours(c.hour_offset as i64);
                let f = match (c.precision, c.am_pm) {
                    (0, false) => "%H:%M",
                    (0, true) => "%I:%M %p",
                    (_, false) => "%H:%M:%S",
                    (_, true) => "%I:%M:%S %p",
                };
                (now.format(f).to_string(), white)
            }
            ClockType::Recording => {
                let t = *self.engine.record_started.lock().unwrap();
                (span(t.map_or(0.0, |t| t.elapsed().as_secs_f64())), white)
            }
            ClockType::Streaming => {
                let t = *self.engine.stream_started.lock().unwrap();
                (span(t.map_or(0.0, |t| t.elapsed().as_secs_f64())), white)
            }
            ClockType::TimeRemaining => {
                let video = project
                    .program
                    .filter(|id| project.inputs.iter().any(|i| i.id == *id && i.kind.has_timeline()));
                let left = video.and_then(|id| self.engine.rt(id)).map_or(0.0, |rt| {
                    let dur = rt.dur_ms.load(Ordering::Relaxed) as f64;
                    let pos = rt.pos_ms.load(Ordering::Relaxed) as f64;
                    (dur - pos).max(0.0) / 1000.0
                });
                (format!("{left:04.1}"), white)
            }
            ClockType::Event => event_clock(c),
        }
    }
}

pub(super) fn format_span(secs: f64, precision: u8) -> String {
    let s = secs as u64;
    match precision {
        0 => format!("{:02}:{:02}", s / 3600, s / 60 % 60),
        1 => format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60),
        _ => format!("{:02}:{:04.1}", s / 60, secs % 60.0),
    }
}

/// An event from 22:00 to 01:00 ends the next day: the end moves a day on,
/// and the small hours count as part of the evening before.
fn across_midnight(now: i64, start: Option<i64>, end: Option<i64>) -> (i64, Option<i64>) {
    const DAY: i64 = 86_400;
    let (Some(st), Some(en)) = (start, end) else { return (now, end) };
    if en > st {
        return (now, end);
    }
    let en = en + DAY;
    (if now < st && now + DAY < en { now + DAY } else { now }, Some(en))
}

/// Amber before the start, green during, red once it has run over.
fn event_clock(c: &ClockCfg) -> (String, Color32) {
    use chrono::Timelike;
    let now = chrono::Local::now();
    let now_s = now.num_seconds_from_midnight() as i64;
    let parse = |t: &str| {
        let (h, m) = t.trim().split_once(':')?;
        Some(h.parse::<i64>().ok()? * 3600 + m.parse::<i64>().ok()? * 60)
    };
    let (start, end) = (parse(&c.start), parse(&c.end));
    let span = |v: i64| format_span(v.max(0) as f64, c.precision.min(1));
    let (now_s, end) = across_midnight(now_s, start, end);
    match (start, end) {
        (Some(st), _) if now_s < st => (span(st - now_s), Color32::from_rgb(255, 190, 40)),
        (Some(_), Some(en)) if now_s < en => (span(en - now_s), Color32::from_rgb(70, 220, 90)),
        (Some(_), Some(en)) => (span(now_s - en), Color32::from_rgb(240, 60, 60)),
        (Some(st), None) => (span(now_s - st), Color32::from_rgb(70, 220, 90)),
        _ => ("--:--".to_string(), Color32::GRAY),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_can_run_past_midnight() {
        const H: i64 = 3600;
        let (start, end) = (Some(22 * H), Some(H));
        assert_eq!(across_midnight(21 * H, start, end), (21 * H, Some(25 * H)), "before it starts");
        assert_eq!(across_midnight(23 * H, start, end), (23 * H, Some(25 * H)), "evening");
        assert_eq!(across_midnight(H / 2, start, end), (24 * H + H / 2, Some(25 * H)), "small hours");
        assert_eq!(across_midnight(10 * H, Some(9 * H), Some(11 * H)), (10 * H, Some(11 * H)), "same day");
    }
}
