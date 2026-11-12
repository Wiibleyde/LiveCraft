//! The plug-in chain of an input, a bus or Master: add, reorder, bypass,
//! remove, set parameters and open the plug-in's own window.

use super::*;
use crate::clap::PluginInfo;
use crate::plugins::{self, Command, Owner};

enum Edit {
    Bypass(usize),
    Up(usize),
    Remove(usize),
    Add(PluginInfo),
}

pub(super) fn owner_name(owner: Owner) -> String {
    match owner {
        Owner::Bus(0) => t!("audio_panel.master").into(),
        Owner::Bus(b) => format!("Bus {}", (b'A' + b - 1) as char),
        Owner::Input(id) => t!("plugin_chain.owner_input", id = id),
    }
}

impl App {
    pub(super) fn plugin_chain(&mut self, ui: &mut egui::Ui, owner: Owner, slots: &[PluginSlot]) {
        ui.label(RichText::new(t!("plugin_chain.heading")).strong());
        let mut edit = None;
        if slots.is_empty() {
            ui.label(RichText::new(t!("plugin_chain.empty")).color(DIM));
        }
        for (n, slot) in slots.iter().enumerate() {
            let view = self.engine.plugins.view(owner, slot.key);
            ui.push_id(slot.key, |ui| {
                ui.horizontal(|ui| {
                    let mut on = !slot.bypass;
                    let check = ui.checkbox(&mut on, RichText::new(&slot.name).strong());
                    if named(check, t!("plugin_chain.active_named", name = slot.name))
                        .on_hover_text(t!("plugin_chain.bypass_tip"))
                        .changed()
                    {
                        edit = Some(Edit::Bypass(n));
                    }
                    if n > 0
                        && named(ui.small_button("▲"), t!("plugin_chain.up_named", name = slot.name))
                            .clicked()
                    {
                        edit = Some(Edit::Up(n));
                    }
                    if n + 1 < slots.len()
                        && named(ui.small_button("▼"), t!("plugin_chain.down_named", name = slot.name))
                            .clicked()
                    {
                        edit = Some(Edit::Up(n + 1));
                    }
                    let has_editor = view.as_ref().is_some_and(|v| v.has_editor);
                    let open = view.as_ref().is_some_and(|v| v.editor_open);
                    let label =
                        if open { t!("plugin_chain.close_window") } else { t!("plugin_chain.window") };
                    let b = ui.add_enabled(has_editor, egui::Button::new(label).small());
                    let b = named(b, t!("plugin_chain.window_named", name = slot.name));
                    if b.on_disabled_hover_text(t!("plugin_chain.no_window_tip")).clicked() {
                        self.engine.plugins.send(Command::ToggleEditor { owner, key: slot.key });
                    }
                    if named(
                        ui.small_button(t!("plugin_chain.remove")),
                        t!("plugin_chain.remove_named", name = slot.name),
                    )
                    .clicked()
                    {
                        edit = Some(Edit::Remove(n));
                    }
                });
                match &view {
                    None => {
                        ui.label(RichText::new(t!("plugin_chain.starting")).color(DIM));
                    }
                    Some(v) => {
                        if let Some(e) = &v.error {
                            ui.label(RichText::new(e).color(RED));
                        }
                        if v.running {
                            let ms = v.latency as f32 * 1000.0 / sources::AUDIO_RATE as f32;
                            let text = if v.latency == 0 {
                                t!("plugin_chain.no_latency").to_string()
                            } else {
                                t!("plugin_chain.latency", samples = v.latency, ms = format!("{ms:.1}"))
                            };
                            ui.label(RichText::new(text).color(DIM));
                            self.plugin_params(ui, owner, slot, v);
                        }
                    }
                }
            });
            ui.add_space(4.0);
        }
        let catalog = self.engine.plugins.catalog();
        let add = egui::Button::new(t!("plugin_chain.add"));
        egui::containers::menu::MenuButton::from_button(add).ui(ui, |ui| {
            ui.set_min_width(220.0);
            if !catalog.scanned && !catalog.scanning {
                self.engine.plugins.scan();
            }
            if catalog.scanning || !catalog.scanned {
                ui.label(RichText::new(t!("plugin_chain.searching")).color(DIM));
            } else if catalog.found.is_empty() {
                ui.label(RichText::new(t!("plugin_chain.none_found")).color(DIM));
            }
            for info in &catalog.found {
                let hover = format!("{} {}\n{}", info.vendor, info.version, info.path);
                if ui.button(&info.name).on_hover_text(hover).clicked() {
                    edit = Some(Edit::Add(info.clone()));
                    ui.close();
                }
            }
            ui.separator();
            if ui.button(t!("plugin_chain.rescan")).clicked() {
                self.engine.plugins.scan();
            }
            if ui.button(t!("plugin_chain.pick_file")).clicked() {
                let picked =
                    rfd::FileDialog::new().add_filter(t!("plugin_chain.file_filter"), &["clap"]).pick_file();
                if let Some(path) = picked {
                    self.engine.plugins.scan_file(path);
                }
            }
        });
        if !catalog.problems.is_empty() {
            let text = t!("plugin_chain.unreadable", count = catalog.problems.len());
            ui.label(RichText::new(text).color(ORANGE)).on_hover_text(catalog.problems.join("\n"));
        }
        if let Some(edit) = edit {
            let key = self.engine.new_id();
            self.with(|s| {
                let Some(list) = plugins::chain_mut(&mut s.project, owner) else { return };
                match edit {
                    Edit::Bypass(n) => list[n].bypass = !list[n].bypass,
                    Edit::Up(n) => list.swap(n - 1, n),
                    Edit::Remove(n) => {
                        list.remove(n);
                    }
                    Edit::Add(info) => list.push(PluginSlot {
                        key,
                        path: info.path,
                        id: info.id,
                        name: info.name,
                        ..PluginSlot::default()
                    }),
                }
            });
        }
    }

    fn plugin_params(
        &mut self,
        ui: &mut egui::Ui,
        owner: Owner,
        slot: &PluginSlot,
        view: &plugins::SlotView,
    ) {
        if view.params.is_empty() {
            return;
        }
        egui::CollapsingHeader::new(t!("plugin_chain.params_of", name = slot.name))
            .id_salt(("plugin-params", slot.key))
            .default_open(view.params.len() <= 8)
            .show(ui, |ui| {
                for p in &view.params {
                    let mut value = p.value;
                    let changed = ui
                        .horizontal(|ui| {
                            let mut slider =
                                egui::Slider::new(&mut value, p.min..=p.max).show_value(false).text(&p.name);
                            if p.stepped {
                                slider = slider.integer();
                            }
                            let r = ui.add_enabled(!p.read_only, slider);
                            ui.label(RichText::new(&p.text).color(DIM));
                            r.changed()
                        })
                        .inner;
                    if changed {
                        self.engine.plugins.send(Command::SetParam { owner, key: slot.key, id: p.id, value });
                    }
                }
            });
    }

    /// The chain of a bus or Master, in its own window.
    pub(super) fn bus_plugins_window(&mut self, ctx: &egui::Context, project: &Project) {
        let Some(owner) = self.plugins_for else { return };
        let Some(slots) = plugins::chain(project, owner) else {
            self.plugins_for = None;
            return;
        };
        let mut open = true;
        egui::Window::new(t!("plugin_chain.window_title", name = owner_name(owner)))
            .open(&mut open)
            .default_size([440.0, 260.0])
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(460.0)
                    .show(ui, |ui| self.plugin_chain(ui, owner, slots));
            });
        if !open {
            self.plugins_for = None;
        }
    }
}
