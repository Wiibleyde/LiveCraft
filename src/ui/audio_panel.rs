//! The audio mixer: outputs on the left, one strip per input with sound.

use super::*;

const STRIP: f32 = 92.0;
const BUSES: [&str; 8] = ["M", "A", "B", "C", "D", "E", "F", "G"];

fn head(ui: &mut egui::Ui, name: &str, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(STRIP, 17.0), Sense::hover());
    ui.painter().rect_filled(rect, 1.0, color);
    let shown: String = name.chars().take(13).collect();
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        shown,
        egui::FontId::proportional(10.5),
        Color32::WHITE,
    );
}

impl App {
    pub(super) fn audio_panel(&mut self, ui: &mut egui::Ui, project: &Project) {
        let height = ui.available_height();
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing = vec2(4.0, 3.0);
            vertical_tag(ui, height, "SORTIES");
            self.master_strip(ui, project, height);
            for b in 0..7 {
                let used = project.inputs.iter().any(|i| i.audio.buses & (1 << (b + 1)) != 0);
                if used {
                    self.bus_strip(ui, project, b, height);
                }
            }
            vertical_tag(ui, height, "ENTRÉES");
            egui::ScrollArea::horizontal().auto_shrink([false, false]).show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing = vec2(4.0, 3.0);
                    let list = project.inputs.iter().filter(|i| i.kind.has_audio() && i.audio.visible);
                    for input in list {
                        self.input_strip_audio(ui, project, input, height);
                    }
                });
            });
        });
    }

    fn master_strip(&mut self, ui: &mut egui::Ui, project: &Project, height: f32) {
        let mut level = project.master_level;
        let mut muted = project.master_muted;
        let mut phones = project.headphones_level;
        let peak = f32::from_bits(self.engine.bus_peaks[0].load(Ordering::Relaxed));
        let mut open_settings = false;
        ui.allocate_ui_with_layout(vec2(STRIP, height), egui::Layout::top_down(egui::Align::Center), |ui| {
            ui.set_width(STRIP);
            head(ui, "Général", GREEN);
            let fader_h = (height - 72.0).max(40.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    open_settings |= gear(ui, BTN).on_hover_text("Réglages audio").clicked();
                });
                ui.spacing_mut().slider_width = fader_h;
                ui.add(egui::Slider::new(&mut level, 0.0..=1.2).vertical().show_value(false));
                let (r, _) = ui.allocate_exact_size(vec2(10.0, fader_h), Sense::hover());
                draw_meter(ui, r, peak, muted);
            });
            ui.spacing_mut().slider_width = STRIP - 18.0;
            let knob = ui.add(egui::Slider::new(&mut phones, 0.0..=1.2).show_value(false));
            knob.on_hover_text("Volume du casque");
            let fill = if muted { RED } else { GREEN };
            let m = egui::Button::new(RichText::new("M").size(11.0).color(Color32::WHITE))
                .fill(fill)
                .min_size(vec2(40.0, 17.0));
            if ui.add(m).on_hover_text("Couper ou rétablir la sortie générale").clicked() {
                muted = !muted;
            }
        });
        if open_settings {
            self.open_settings(5);
        }
        if level != project.master_level
            || muted != project.master_muted
            || phones != project.headphones_level
        {
            self.with(|s| {
                let d = &mut s.project;
                if level != project.master_level {
                    d.master_level = level;
                }
                if muted != project.master_muted {
                    d.master_muted = muted;
                }
                if phones != project.headphones_level {
                    d.headphones_level = phones;
                }
            });
        }
    }

    fn bus_strip(&mut self, ui: &mut egui::Ui, project: &Project, b: usize, height: f32) {
        let cfg = &project.buses[b];
        let (mut level, mut muted, mut to_master) = (cfg.level, cfg.muted, cfg.send_to_master);
        let peak = f32::from_bits(self.engine.bus_peaks[b + 1].load(Ordering::Relaxed));
        ui.allocate_ui_with_layout(vec2(STRIP, height), egui::Layout::top_down(egui::Align::Center), |ui| {
            ui.set_width(STRIP);
            head(ui, &format!("Bus {}", BUSES[b + 1]), BLUE);
            let fader_h = (height - 56.0).max(40.0);
            ui.horizontal(|ui| {
                ui.add_space(20.0);
                ui.spacing_mut().slider_width = fader_h;
                ui.add(egui::Slider::new(&mut level, 0.0..=1.2).vertical().show_value(false));
                let (r, _) = ui.allocate_exact_size(vec2(10.0, fader_h), Sense::hover());
                draw_meter(ui, r, peak, muted);
            });
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                ui.add_space(12.0);
                if tiny(ui, "M", to_master.then_some(GREEN)).on_hover_text("Envoyer vers Général").clicked()
                {
                    to_master = !to_master;
                }
                if icon_button(ui, Icon::Speaker(!muted), muted.then_some(RED)).clicked() {
                    muted = !muted;
                }
            });
        });
        if level != cfg.level || muted != cfg.muted || to_master != cfg.send_to_master {
            self.with(|s| {
                let c = &mut s.project.buses[b];
                if level != cfg.level {
                    c.level = level;
                }
                if muted != cfg.muted {
                    c.muted = muted;
                }
                if to_master != cfg.send_to_master {
                    c.send_to_master = to_master;
                }
            });
        }
    }

    fn input_strip_audio(&mut self, ui: &mut egui::Ui, project: &Project, input: &Input, height: f32) {
        let id = input.id;
        let mut a = input.audio.clone();
        let mut solo = self.with(|s| s.solo.contains(&id));
        let was_solo = solo;
        let peak = self.engine.rt(id).map_or(0.0, |r| r.peak());
        let on_air = project.program == Some(id);
        let mut open = false;
        ui.allocate_ui_with_layout(vec2(STRIP, height), egui::Layout::top_down(egui::Align::Center), |ui| {
            ui.set_width(STRIP);
            head(ui, &input.name, if on_air { GREEN } else { BLUE });
            let fader_h = (height - 62.0).max(40.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    open |= gear(ui, BTN).on_hover_text("Réglages audio de l'entrée").clicked();
                    let s = named(tiny(ui, "S", solo.then_some(ORANGE)), format!("Solo {}", input.name));
                    if s.on_hover_text("Écouter au casque").clicked() {
                        solo = !solo;
                    }
                    let follow = icon_button(ui, Icon::Follow, a.follow.then_some(BTN));
                    let follow = named(follow, format!("Suivi {}", input.name));
                    if follow.on_hover_text("Mixage automatique : suit le direct").clicked() {
                        a.follow = !a.follow;
                    }
                    let sp = icon_button(ui, Icon::Speaker(!a.muted), a.muted.then_some(RED));
                    let sp = named(sp, format!("Muet {}", input.name));
                    if sp.on_hover_text("Couper ou rétablir").clicked() {
                        a.muted = !a.muted;
                    }
                });
                ui.spacing_mut().slider_width = fader_h;
                ui.add(egui::Slider::new(&mut a.volume, 0.0..=1.2).vertical().show_value(false));
                let (r, _) = ui.allocate_exact_size(vec2(10.0, fader_h), Sense::hover());
                draw_meter(ui, r, peak, a.muted);
            });
            for row in 0..2 {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 1.0;
                    ui.spacing_mut().button_padding = vec2(4.0, 0.0);
                    ui.add_space(6.0);
                    for b in row * 4..row * 4 + 4 {
                        let on = a.buses & (1 << b) != 0;
                        let r = named(
                            tiny(ui, BUSES[b], on.then_some(GREEN)),
                            format!("Bus {} {}", BUSES[b], input.name),
                        );
                        if r.clicked() {
                            a.buses ^= 1 << b;
                        }
                    }
                });
            }
        });
        if open {
            self.editing = Some(id);
            self.edit_tab = 8;
        }
        if a != input.audio || solo != was_solo {
            let was = &input.audio;
            self.with(|s| {
                if let Some(slot) = s.audio_mut(id) {
                    if a.volume != was.volume {
                        slot.volume = a.volume;
                    }
                    if a.muted != was.muted {
                        slot.muted = a.muted;
                    }
                    if a.follow != was.follow {
                        slot.follow = a.follow;
                    }
                    if a.buses != was.buses {
                        slot.buses = a.buses;
                    }
                }
                if solo != was_solo {
                    if solo {
                        s.solo.insert(id);
                    } else {
                        s.solo.remove(&id);
                    }
                }
            });
        }
    }
}
