//! Small drawing helpers and window content that holds no state of its own.

use crate::frame::Frame;
use crate::model::{Border, Input, Layer, Placement, Project};
use crate::sources;
use eframe::egui::{self, Color32, Pos2, Rect, RichText, Sense, Stroke, pos2, vec2};
use std::sync::atomic::Ordering;

pub const BG: Color32 = Color32::from_rgb(19, 19, 20);
pub const PANEL: Color32 = Color32::from_rgb(27, 27, 29);
pub const CARD: Color32 = Color32::from_rgb(24, 25, 28);
pub const BTN: Color32 = Color32::from_rgb(52, 53, 56);
pub const LINE: Color32 = Color32::from_rgb(58, 60, 64);
pub const TEXT: Color32 = Color32::from_rgb(226, 227, 230);
pub const DIM: Color32 = Color32::from_rgb(150, 152, 158);
pub const RED: Color32 = Color32::from_rgb(214, 40, 40);
pub const GREEN: Color32 = Color32::from_rgb(10, 122, 22);
pub const ORANGE: Color32 = Color32::from_rgb(242, 140, 0);
pub const BLUE: Color32 = Color32::from_rgb(23, 57, 138);
pub const BRIGHT_BLUE: Color32 = Color32::from_rgb(34, 110, 214);
pub const HEADER_H: f32 = 26.0;

/// Names a control for screen readers and for the interface tests.
pub fn named(r: egui::Response, label: impl Into<String>) -> egui::Response {
    let label = label.into();
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, label.clone()));
    r
}

pub fn rgb(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

pub fn apply_style(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = PANEL;
    v.window_stroke = Stroke::new(1.0, LINE);
    v.extreme_bg_color = Color32::from_rgb(10, 10, 11);
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = BRIGHT_BLUE;
    v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    v.widgets.inactive.weak_bg_fill = BTN;
    v.widgets.inactive.bg_fill = BTN;
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(70, 72, 78);
    v.widgets.hovered.bg_fill = Color32::from_rgb(70, 72, 78);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(86, 88, 96);
    ctx.set_visuals(v);
}

pub fn upload(ctx: &egui::Context, slot: &mut Option<egui::TextureHandle>, name: &str, f: &Frame) {
    let img = egui::ColorImage::from_rgba_premultiplied([f.w, f.h], &f.data);
    match slot {
        Some(t) => t.set(img, egui::TextureOptions::LINEAR),
        None => *slot = Some(ctx.load_texture(name, img, egui::TextureOptions::LINEAR)),
    }
}

pub fn full_uv() -> Rect {
    Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))
}

/// Largest rectangle of the given shape inside `area`, centred.
pub fn fit(area: Rect, aspect: f32) -> Rect {
    let w = area.width().min(area.height() * aspect);
    Rect::from_center_size(area.center(), vec2(w, w / aspect))
}

#[derive(Default)]
pub struct MonitorClicks {
    pub gear: bool,
    pub scopes: bool,
    pub safe: bool,
}

/// `extras` adds the scope and safe-area icons the preview title carries.
#[allow(clippy::too_many_arguments)]
pub fn draw_monitor(
    ui: &mut egui::Ui,
    rect: Rect,
    tex: Option<&egui::TextureHandle>,
    name: &str,
    color: Color32,
    salt: &str,
    extras: bool,
    safe: u8,
) -> MonitorClicks {
    let head = Rect::from_min_size(rect.min, vec2(rect.width(), HEADER_H));
    let body = Rect::from_min_max(pos2(rect.left(), head.bottom()), rect.max);
    let p = ui.painter();
    p.rect_filled(head, 0.0, color);
    p.text(
        head.left_center() + vec2(8.0, 0.0),
        egui::Align2::LEFT_CENTER,
        name,
        egui::FontId::proportional(13.0),
        Color32::WHITE,
    );
    p.rect_filled(body, 0.0, Color32::BLACK);
    if let Some(t) = tex {
        p.image(t.id(), body, full_uv(), Color32::WHITE);
    }
    draw_safe_area(ui, body, safe);
    let mut clicks = MonitorClicks::default();
    let spot =
        |n: f32| Rect::from_center_size(head.right_center() - vec2(14.0 + n * 22.0, 0.0), vec2(18.0, 18.0));
    let hit = |ui: &egui::Ui, r: Rect, s: &str, tip: &str| {
        ui.interact(r, ui.scope_id().with((salt, s.to_string())), Sense::click()).on_hover_text(tip).clicked()
    };
    clicks.gear = hit(ui, spot(0.0), "gear", t!("widgets.input_settings"));
    draw_gear(ui, spot(0.0).center(), 6.0, Color32::WHITE);
    if extras {
        clicks.safe = hit(ui, spot(1.0), "safe", t!("widgets.safe_areas"));
        let r = spot(1.0).shrink(3.0);
        ui.painter().rect_stroke(r, 1.0, Stroke::new(1.3, Color32::WHITE), egui::StrokeKind::Inside);
        ui.painter().rect_stroke(
            r.shrink(3.0),
            0.0,
            Stroke::new(1.0, Color32::WHITE),
            egui::StrokeKind::Inside,
        );
        clicks.scopes = hit(ui, spot(2.0), "scopes", t!("widgets.scopes"));
        let r = spot(2.0).shrink(3.0);
        for (n, c) in [Color32::RED, Color32::GREEN, Color32::from_rgb(70, 110, 255)].iter().enumerate() {
            let w = r.width() / 3.0;
            let x = r.left() + n as f32 * w;
            ui.painter().rect_filled(Rect::from_min_size(pos2(x, r.top()), vec2(w, r.height())), 0.0, *c);
        }
    }
    clicks
}

/// 0 off, then EBU margins, 80 %, 90 %, a grid, and the 9:16 vertical frame.
pub const SAFE_AREAS: u8 = 6;

fn draw_safe_area(ui: &egui::Ui, r: Rect, mode: u8) {
    let p = ui.painter();
    let line = Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 170));
    let inset = |f: f32| Rect::from_center_size(r.center(), r.size() * f);
    let boxed = |f: f32| {
        p.rect_stroke(inset(f), 0.0, line, egui::StrokeKind::Inside);
    };
    match mode {
        1 => {
            boxed(0.93);
            boxed(0.9);
        }
        2 => boxed(0.8),
        3 => boxed(0.9),
        4 => {
            for n in 1..3 {
                let t = n as f32 / 3.0;
                let x = r.left() + r.width() * t;
                let y = r.top() + r.height() * t;
                p.line_segment([pos2(x, r.top()), pos2(x, r.bottom())], line);
                p.line_segment([pos2(r.left(), y), pos2(r.right(), y)], line);
            }
        }
        5 => {
            let w = r.height() * 9.0 / 16.0;
            let frame = Rect::from_center_size(r.center(), vec2(w, r.height()));
            p.rect_stroke(frame, 0.0, line, egui::StrokeKind::Inside);
        }
        _ => {}
    }
}

pub fn draw_gear(ui: &egui::Ui, c: Pos2, r: f32, color: Color32) {
    let p = ui.painter();
    p.circle_stroke(c, r * 0.62, Stroke::new(r * 0.36, color));
    for n in 0..8 {
        let a = n as f32 * std::f32::consts::TAU / 8.0;
        let d = vec2(a.cos(), a.sin());
        p.line_segment([c + d * (r * 0.7), c + d * (r * 1.12)], Stroke::new(r * 0.32, color));
    }
}

pub fn draw_down_arrow(ui: &egui::Ui, c: Pos2) {
    let points = vec![c + vec2(-3.5, -2.0), c + vec2(3.5, -2.0), c + vec2(0.0, 2.5)];
    ui.painter().add(egui::Shape::convex_polygon(points, TEXT, Stroke::NONE));
}

pub fn gear(ui: &mut egui::Ui, fill: Color32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(17.0, 15.0), Sense::click());
    ui.painter().rect_filled(rect, 1.0, fill);
    draw_gear(ui, rect.center(), 4.2, Color32::WHITE);
    resp
}

/// Small painted glyphs for the card and footer buttons.
#[derive(Clone, Copy)]
pub enum Icon {
    Loop,
    Play,
    Pause,
    Restart,
    Camera,
    Lock(bool),
    Stats,
    Speaker(bool),
    Follow,
    /// Back or on by one: slides.
    Step(bool),
}

pub fn icon_button(ui: &mut egui::Ui, icon: Icon, on: Option<Color32>) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(18.0, 15.0), Sense::click());
    if let Some(c) = on {
        ui.painter().rect_filled(rect, 1.0, c);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 1.0, BTN);
    }
    draw_icon(ui, rect.center(), icon, TEXT);
    resp
}

pub fn draw_icon(ui: &egui::Ui, c: Pos2, icon: Icon, col: Color32) {
    let p = ui.painter();
    let line = Stroke::new(1.3, col);
    match icon {
        Icon::Loop => {
            p.circle_stroke(c, 4.0, line);
            let tip = c + vec2(4.0, -2.0);
            let arrow = vec![tip + vec2(-2.5, -1.0), tip + vec2(2.5, -1.0), tip + vec2(0.0, 2.5)];
            p.add(egui::Shape::convex_polygon(arrow, col, Stroke::NONE));
        }
        Icon::Play => {
            let t = vec![c + vec2(-3.0, -4.5), c + vec2(4.0, 0.0), c + vec2(-3.0, 4.5)];
            p.add(egui::Shape::convex_polygon(t, col, Stroke::NONE));
        }
        Icon::Pause => {
            for dx in [-3.0, 1.0] {
                p.rect_filled(Rect::from_min_size(c + vec2(dx, -4.5), vec2(2.2, 9.0)), 0.0, col);
            }
        }
        Icon::Restart => {
            p.rect_filled(Rect::from_min_size(c + vec2(-4.5, -4.5), vec2(1.8, 9.0)), 0.0, col);
            let t = vec![c + vec2(4.0, -4.5), c + vec2(4.0, 4.5), c + vec2(-2.0, 0.0)];
            p.add(egui::Shape::convex_polygon(t, col, Stroke::NONE));
        }
        Icon::Camera => {
            let body = Rect::from_center_size(c + vec2(0.0, 1.0), vec2(12.0, 8.0));
            p.rect_stroke(body, 1.0, line, egui::StrokeKind::Inside);
            p.circle_stroke(c + vec2(0.0, 1.0), 2.2, line);
            p.rect_filled(Rect::from_min_size(c + vec2(-2.0, -5.0), vec2(4.0, 2.0)), 0.0, col);
        }
        Icon::Lock(closed) => {
            let color = if closed { ORANGE } else { col };
            p.rect_filled(Rect::from_center_size(c + vec2(0.0, 2.0), vec2(9.0, 7.0)), 1.0, color);
            let top = if closed { c + vec2(0.0, -2.0) } else { c + vec2(3.0, -3.0) };
            p.circle_stroke(top, 3.0, Stroke::new(1.4, color));
        }
        Icon::Stats => {
            for (n, h) in [3.0, 6.0, 9.0].iter().enumerate() {
                let x = c.x - 5.0 + n as f32 * 4.0;
                let r = Rect::from_min_max(pos2(x, c.y + 4.5 - h), pos2(x + 2.6, c.y + 4.5));
                p.rect_filled(r, 0.0, col);
            }
        }
        Icon::Speaker(on) => {
            let t = vec![
                c + vec2(-5.0, -2.0),
                c + vec2(-2.0, -2.0),
                c + vec2(1.5, -5.0),
                c + vec2(1.5, 5.0),
                c + vec2(-2.0, 2.0),
                c + vec2(-5.0, 2.0),
            ];
            p.add(egui::Shape::closed_line(t, line));
            if on {
                p.line_segment([c + vec2(3.5, -2.5), c + vec2(3.5, 2.5)], line);
            } else {
                p.line_segment([c + vec2(3.0, -3.0), c + vec2(7.0, 3.0)], line);
            }
        }
        Icon::Follow => {
            let up = vec![c + vec2(-5.0, 2.0), c + vec2(-1.0, -3.0), c + vec2(3.0, 2.0)];
            p.add(egui::Shape::convex_polygon(up, Color32::from_rgb(70, 190, 80), Stroke::NONE));
            let dn = vec![c + vec2(0.0, 0.0), c + vec2(6.0, 0.0), c + vec2(3.0, 4.5)];
            p.add(egui::Shape::convex_polygon(dn, ORANGE, Stroke::NONE));
        }
        Icon::Step(forward) => {
            let s = if forward { 1.0 } else { -1.0 };
            let mut t = vec![c + vec2(-2.5 * s, -4.5), c + vec2(3.0 * s, 0.0), c + vec2(-2.5 * s, 4.5)];
            // Mirrored, the points must still run the same way round.
            if !forward {
                t.reverse();
            }
            p.add(egui::Shape::convex_polygon(t, col, Stroke::NONE));
            p.rect_filled(Rect::from_center_size(c + vec2(4.2 * s, 0.0), vec2(1.8, 9.0)), 0.0, col);
        }
    }
}

fn colored_button(ui: &mut egui::Ui, label: &str, fill: Color32, on: bool) -> egui::Response {
    let text = RichText::new(label).size(12.0).color(if on { Color32::WHITE } else { TEXT });
    ui.add(egui::Button::new(text).fill(fill).min_size(vec2(0.0, 22.0)))
}

pub fn bar_button(ui: &mut egui::Ui, label: &str, on: bool) -> egui::Response {
    colored_button(ui, label, if on { BRIGHT_BLUE } else { BTN }, on)
}

/// A small gear and its button. Returns (gear clicked, button clicked).
pub fn gear_button(ui: &mut egui::Ui, label: &str, on: bool) -> (bool, bool) {
    let g = gear(ui, BTN).clicked();
    let m = colored_button(ui, label, if on { RED } else { BTN }, on).clicked();
    ui.add_space(10.0);
    (g, m)
}

pub fn column_button(ui: &mut egui::Ui, label: &str, width: f32, height: f32, on: bool) -> egui::Response {
    let text = RichText::new(label).size(12.0).color(Color32::WHITE);
    let fill = if on { RED } else { BTN };
    ui.add(egui::Button::new(text).fill(fill).min_size(vec2(width, height)))
}

/// Text-only card button; `on` gives it a coloured background.
pub fn tiny(ui: &mut egui::Ui, label: &str, on: Option<Color32>) -> egui::Response {
    let text = RichText::new(label).size(10.5).color(TEXT);
    let fill = on.unwrap_or(Color32::TRANSPARENT);
    ui.add(egui::Button::new(text).fill(fill).stroke(Stroke::NONE).min_size(vec2(0.0, 15.0)))
}

pub fn vertical_text(ui: &egui::Ui, rect: Rect, name: &str) {
    let font = egui::FontId::proportional(11.0);
    let galley = ui.painter().layout_no_wrap(name.to_string(), font, Color32::WHITE);
    let pos = pos2(rect.center().x - galley.size().y / 2.0, rect.bottom() - 6.0);
    let shape =
        egui::epaint::TextShape::new(pos, galley, Color32::WHITE).with_angle(-std::f32::consts::FRAC_PI_2);
    ui.painter().add(shape);
}

pub fn vertical_tag(ui: &mut egui::Ui, height: f32, label: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(18.0, height), Sense::hover());
    ui.painter().rect_filled(rect, 1.0, BRIGHT_BLUE);
    vertical_text(ui, rect, label);
}

/// Level in five bands, green up to red; blue when the channel is muted.
pub fn draw_meter(ui: &egui::Ui, rect: Rect, peak: f32, muted: bool) {
    let p = ui.painter();
    p.rect_filled(rect, 1.0, Color32::from_rgb(8, 8, 10));
    let db = 20.0 * peak.max(1e-5).log10();
    let level = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    let top = rect.bottom() - rect.height() * level;
    let color = if muted {
        Color32::from_rgb(60, 110, 220)
    } else if db > -3.0 {
        RED
    } else if db > -9.0 {
        ORANGE
    } else if db > -18.0 {
        Color32::from_rgb(210, 200, 40)
    } else {
        Color32::from_rgb(40, 190, 70)
    };
    p.rect_filled(Rect::from_min_max(pos2(rect.left(), top), rect.max), 1.0, color);
}

pub fn placement_editor(ui: &mut egui::Ui, p: &mut Placement) {
    ui.horizontal(|ui| {
        if ui.button(t!("widgets.full_frame")).clicked() {
            *p = Placement::FULL;
        }
        if ui.button(t!("widgets.bottom_right")).clicked() {
            *p = Placement { x: 0.66, y: 0.64, scale: 0.3 };
        }
        if ui.button(t!("widgets.top_right")).clicked() {
            *p = Placement { x: 0.66, y: 0.05, scale: 0.3 };
        }
    });
    ui.add(egui::Slider::new(&mut p.scale, 0.05..=1.0).text(t!("widgets.size")));
    ui.add(egui::Slider::new(&mut p.x, -0.5..=1.0).text(t!("widgets.horizontal")));
    ui.add(egui::Slider::new(&mut p.y, -0.5..=1.0).text(t!("widgets.vertical")));
}

pub fn border_editor(ui: &mut egui::Ui, b: &mut Border) {
    ui.horizontal(|ui| {
        ui.label(t!("widgets.border"));
        ui.color_edit_button_srgb(&mut b.color);
        ui.add(egui::DragValue::new(&mut b.thickness).range(0.0..=40.0).suffix(" px"));
        ui.label(t!("widgets.rounding"));
        ui.add(egui::DragValue::new(&mut b.radius).range(0.0..=200.0).suffix(" px"));
    });
}

/// Layout box: drag a layer to move it, with Shift to resize it.
pub fn layers_canvas(
    ui: &mut egui::Ui,
    layers: &mut [Layer],
    names: &[String],
    selected: usize,
    aspect: f32,
) -> Option<usize> {
    let width = 340.0;
    let (area, _) = ui.allocate_exact_size(vec2(width, width / aspect), Sense::hover());
    ui.painter().rect_filled(area, 0.0, Color32::BLACK);
    let mut clicked = None;
    for (n, l) in layers.iter_mut().enumerate() {
        if !l.visible {
            continue;
        }
        let r = Rect::from_min_size(
            area.min + vec2(l.placement.x * area.width(), l.placement.y * area.height()),
            area.size() * l.placement.scale,
        );
        let id = ui.scope_id().with(("couche", n));
        let resp = ui.interact(r.intersect(area), id, Sense::click_and_drag());
        if resp.clicked() || resp.drag_started() {
            clicked = Some(n);
        }
        if resp.dragged() {
            let d = resp.drag_delta();
            if ui.input(|i| i.modifiers.shift) {
                l.placement.scale = (l.placement.scale + d.x / area.width()).clamp(0.05, 1.0);
            } else {
                l.placement.x += d.x / area.width();
                l.placement.y += d.y / area.height();
            }
        }
        let color = if n == selected { ORANGE } else { BRIGHT_BLUE };
        let fill = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 50);
        ui.painter().rect_filled(r, 0.0, fill);
        ui.painter().rect_stroke(r, 0.0, Stroke::new(1.0, color), egui::StrokeKind::Inside);
        ui.painter().text(
            r.left_top() + vec2(3.0, 2.0),
            egui::Align2::LEFT_TOP,
            format!("{} {}", n + 1, names.get(n).map_or("", |s| s.as_str())),
            egui::FontId::proportional(10.0),
            Color32::WHITE,
        );
    }
    clicked
}

pub fn video_controls(ui: &mut egui::Ui, rt: &sources::InputRt, width: f32) {
    let dur = rt.dur_ms.load(Ordering::Relaxed);
    let pos = rt.pos_ms.load(Ordering::Relaxed);
    let playing = rt.is_playing();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        if icon_button(ui, Icon::Restart, None).on_hover_text(t!("widgets.back_to_start")).clicked() {
            rt.seek_ms.store(0, Ordering::Relaxed);
        }
        let icon = if playing { Icon::Pause } else { Icon::Play };
        if icon_button(ui, icon, None).clicked() {
            rt.playing.store(!playing, Ordering::Relaxed);
        }
        ui.label(RichText::new(format!("{} / {}", clock(pos), clock(dur))).size(10.5).color(DIM));
        if dur > 0 {
            let mut at = pos.min(dur) as f32 / dur as f32;
            ui.spacing_mut().slider_width = (width - 150.0).max(40.0);
            let r = ui.add(egui::Slider::new(&mut at, 0.0..=1.0).show_value(false));
            if r.drag_stopped() {
                rt.seek_ms.store((at * dur as f32) as i64, Ordering::Relaxed);
            }
        }
    });
}

pub fn clock(ms: u64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}", s / 60, s % 60)
}

pub fn device_combo(ui: &mut egui::Ui, label: &str, device: &mut String, list: &[String]) {
    ui.horizontal(|ui| {
        ui.label(label);
        let shown = if device.is_empty() { t!("widgets.none") } else { crate::audio::device_label(device) };
        // Positional, not the label: the label changes with the interface language.
        egui::ComboBox::from_id_salt(ui.unique_id()).selected_text(shown).show_ui(ui, |ui| {
            ui.selectable_value(device, String::new(), t!("widgets.none"));
            for d in list {
                ui.selectable_value(device, d.clone(), crate::audio::device_label(d));
            }
        });
    });
}

pub fn input_combo(ui: &mut egui::Ui, salt: &str, value: &mut Option<u64>, project: &Project, skip: u64) {
    let shown = value
        .and_then(|id| project.inputs.iter().find(|i| i.id == id))
        .map_or(t!("widgets.none").to_string(), |i| i.name.clone());
    egui::ComboBox::from_id_salt(salt).selected_text(shown).show_ui(ui, |ui| {
        ui.selectable_value(value, None, t!("widgets.none"));
        // `skip` shows the choice: nothing that would let a delay show itself.
        let list = project.inputs.iter().filter(|i: &&Input| {
            i.id != skip && i.kind.has_picture() && !crate::desk::link_loops(&project.inputs, skip, i.id)
        });
        for i in list {
            ui.selectable_value(value, Some(i.id), &i.name);
        }
    });
}

pub fn codec_label(name: &str) -> String {
    match name {
        "libx264" => t!("widgets.codec_h264_cpu").into(),
        "libx265" => t!("widgets.codec_hevc_cpu").into(),
        "libsvtav1" => t!("widgets.codec_av1_cpu").into(),
        "h264_nvenc" => t!("widgets.codec_h264_nvidia").into(),
        "hevc_nvenc" => t!("widgets.codec_hevc_nvidia").into(),
        "av1_nvenc" => t!("widgets.codec_av1_nvidia").into(),
        "h264_qsv" => "H.264 Intel".into(),
        "hevc_qsv" => "HEVC Intel".into(),
        "h264_amf" => "H.264 AMD".into(),
        "hevc_amf" => "HEVC AMD".into(),
        other => other.into(),
    }
}

pub fn file_name(p: &std::path::Path) -> String {
    p.file_stem().map_or("Source".into(), |s| s.to_string_lossy().into_owned())
}

/// Computes the scope picture for a frame, sampling every few pixels.
pub fn scope_image(f: &Frame, mode: u8) -> egui::ColorImage {
    let (w, h) = (256usize, 256usize);
    let mut px = vec![Color32::BLACK; w * h];
    let mut plot = |x: usize, y: usize, c: [u8; 3]| {
        let i = y.min(h - 1) * w + x.min(w - 1);
        let old = px[i].to_array();
        px[i] = Color32::from_rgb(
            old[0].saturating_add(c[0] / 6),
            old[1].saturating_add(c[1] / 6),
            old[2].saturating_add(c[2] / 6),
        );
    };
    let step = (f.w * f.h / 60_000).max(1);
    for (n, p) in f.data.as_chunks::<4>().0.iter().enumerate().step_by(step) {
        let col = (n % f.w) * w / f.w;
        let (r, g, b) = (p[0] as usize, p[1] as usize, p[2] as usize);
        match mode {
            0 => {
                plot(col, 255 - r, [255, 60, 60]);
                plot(col, 255 - g, [60, 255, 60]);
                plot(col, 255 - b, [70, 110, 255]);
            }
            1 => {
                let y = (r * 299 + g * 587 + b * 114) / 1000;
                plot(col, 255 - y, [220, 240, 220]);
            }
            2 => {
                let third = col / 3;
                plot(third, 255 - r, [255, 60, 60]);
                plot(85 + third, 255 - g, [60, 255, 60]);
                plot(170 + third, 255 - b, [70, 110, 255]);
            }
            _ => {
                let (fr, fg, fb) = (r as f32, g as f32, b as f32);
                let cb = -0.169 * fr - 0.331 * fg + 0.5 * fb;
                let cr = 0.5 * fr - 0.419 * fg - 0.081 * fb;
                plot((128.0 + cb) as usize, (128.0 - cr) as usize, [210, 240, 210]);
            }
        }
    }
    egui::ColorImage { size: [w, h], source_size: vec2(w as f32, h as f32), pixels: px }
}
