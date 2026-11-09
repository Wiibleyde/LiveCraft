//! A virtual set's settings: its decor, its own pictures, the presenters
//! standing in it and its camera presets.

use super::*;
use crate::sets;

impl App {
    pub(super) fn set_editor(&mut self, ui: &mut egui::Ui, input: &mut Input, project: &Project) {
        let id = input.id;
        let choices: Vec<(u64, String)> = project
            .inputs
            .iter()
            .filter(|i| i.id != id && i.kind.has_picture())
            .map(|i| (i.id, i.name.clone()))
            .collect();
        let Kind::VirtualSet { look, background, foreground, cameras, camera, move_ms } = &mut input.kind
        else {
            return;
        };
        ui.horizontal(|ui| {
            ui.label("Décor");
            ui.selectable_value(look, SetLook::Studio, "Studio");
            ui.selectable_value(look, SetLook::Evening, "Soirée");
            ui.selectable_value(look, SetLook::Empty, "Aucun");
        });
        let built_in = if *look == SetLook::Empty { "Aucun" } else { "Décor intégré" };
        ui.horizontal(|ui| {
            ui.label("Fond");
            picture_combo(ui, "plateau-fond", background, &choices, built_in);
            ui.label("Premier plan");
            picture_combo(ui, "plateau-avant", foreground, &choices, built_in);
        });
        ui.label(
            RichText::new("Une image ou une vidéo choisie remplace le décor intégré ; le premier plan doit être transparent.")
                .small()
                .color(DIM),
        );

        ui.separator();
        ui.label(RichText::new("Présentateurs").strong());
        ui.label(
            RichText::new(
                "La clé (fond vert) se règle dans les réglages de chaque présentateur, sa place dans l'onglet Couches.",
            )
            .small()
            .color(DIM),
        );
        let mut remove = None;
        for (n, l) in input.layers.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.checkbox(&mut l.visible, "");
                let shown = choices.iter().find(|c| c.0 == l.input).map_or("", |c| c.1.as_str());
                egui::ComboBox::from_id_salt(("presentateur", n)).selected_text(shown).show_ui(ui, |ui| {
                    for (other, name) in &choices {
                        ui.selectable_value(&mut l.input, *other, name);
                    }
                });
                if ui.small_button("Retirer").clicked() {
                    remove = Some(n);
                }
            });
        }
        if let Some(n) = remove {
            input.layers.remove(n);
        }
        let room = input.layers.len() < 10 && !choices.is_empty();
        if ui.add_enabled(room, egui::Button::new("Ajouter un présentateur")).clicked() {
            input.layers.push(Layer::new(choices[0].0, sets::presenter_spot(input.layers.len())));
        }

        ui.separator();
        ui.label(RichText::new("Caméras").strong());
        ui.add(egui::Slider::new(move_ms, 0..=5000).text("durée d'un mouvement (ms)"));
        let mut go = None;
        let mut remove = None;
        for (n, cam) in cameras.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                let on = (*camera == n).then_some(GREEN);
                let button =
                    named(tiny(ui, &format!("C{}", n + 1), on), format!("Aller à la caméra {}", n + 1));
                if button.on_hover_text("Y aller en douceur").clicked() {
                    go = Some(n);
                }
                ui.add(egui::TextEdit::singleline(&mut cam.name).desired_width(110.0));
                let v = &mut cam.view;
                ui.add(
                    egui::DragValue::new(&mut v.zoom).range(1.0..=sets::MAX_ZOOM).speed(0.01).prefix("zoom "),
                );
                ui.add(egui::DragValue::new(&mut v.x).range(0.0..=1.0).speed(0.005).prefix("x "));
                ui.add(egui::DragValue::new(&mut v.y).range(0.0..=1.0).speed(0.005).prefix("y "));
                if ui.small_button("Retirer").clicked() {
                    remove = Some(n);
                }
            });
        }
        if let Some(n) = remove {
            cameras.remove(n);
            if *camera > n {
                *camera -= 1;
            }
            *camera = (*camera).min(cameras.len().saturating_sub(1));
        }
        if ui.button("Ajouter une caméra (vue actuelle)").clicked() {
            let view = self.with(|s| s.set_view(id, Instant::now())).unwrap_or_default();
            cameras.push(SetCamera { name: format!("Caméra {}", cameras.len() + 1), view });
        }
        if let Some(n) = go {
            self.with(|s| s.set_camera(id, n, None, Instant::now()));
        }
    }
}

/// A picture for the set, or nothing to keep the built-in decor.
fn picture_combo(
    ui: &mut egui::Ui,
    salt: &str,
    value: &mut Option<u64>,
    choices: &[(u64, String)],
    none: &str,
) {
    let shown = value.and_then(|id| choices.iter().find(|c| c.0 == id)).map_or(none, |c| c.1.as_str());
    egui::ComboBox::from_id_salt(salt).selected_text(shown).show_ui(ui, |ui| {
        ui.selectable_value(value, None, none);
        for (id, name) in choices {
            ui.selectable_value(value, Some(*id), name);
        }
    });
}
