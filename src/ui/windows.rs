//! Floating windows of the desk: overlays, playlist, categories, help and the
//! screens that show the output.

use super::*;

impl App {
    pub(super) fn overlays_window(&mut self, ctx: &egui::Context, project: &Project) {
        if !self.overlays_open {
            return;
        }
        let mut open = true;
        let mut cfgs = project.overlay_cfg;
        let mut stingers = project.stingers;
        let window =
            egui::Window::new(t!("windows.overlays_title")).id(egui::Id::unique("Incrustations et stingers"));
        window.open(&mut open).default_width(420.0).show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                for n in 0..16 {
                    let label = if n < 8 { format!("{}", n + 1) } else { format!("S{}", n - 7) };
                    ui.selectable_value(&mut self.overlay_pick, n, label);
                }
            });
            ui.separator();
            if self.overlay_pick < 8 {
                let n = self.overlay_pick;
                let c = &mut cfgs[n];
                let mut current = project.overlays[n];
                ui.horizontal(|ui| {
                    ui.label(t!("windows.overlay_n", n = n + 1));
                    input_combo(ui, "canal", &mut current, project, 0);
                });
                if current != project.overlays[n] {
                    self.with(|s| s.set_overlay(n, current));
                }
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut c.pip, false, t!("windows.full_frame"));
                    ui.selectable_value(&mut c.pip, true, t!("windows.pip"));
                });
                ui.horizontal(|ui| {
                    ui.label(t!("windows.effect"));
                    egui::ComboBox::from_id_salt("effet").selected_text(c.effect.label()).show_ui(ui, |ui| {
                        for k in [
                            TransKind::Cut,
                            TransKind::Fade,
                            TransKind::Zoom,
                            TransKind::Wipe,
                            TransKind::VerticalWipe,
                            TransKind::Slide,
                            TransKind::VerticalSlide,
                        ] {
                            ui.selectable_value(&mut c.effect, k, k.label());
                        }
                    });
                    ui.add(egui::DragValue::new(&mut c.effect_ms).range(0..=5000).suffix(" ms"));
                });
                ui.horizontal(|ui| {
                    ui.label(t!("windows.closes_after"));
                    ui.add(
                        egui::DragValue::new(&mut c.duration_ms).range(0..=600_000).speed(100).suffix(" ms"),
                    );
                    ui.label(RichText::new(t!("windows.zero_stays")).small().color(DIM));
                });
                if c.pip {
                    placement_editor(ui, &mut c.placement);
                    border_editor(ui, &mut c.border);
                }
            } else {
                let n = self.overlay_pick - 8;
                let st = &mut stingers[n];
                ui.horizontal(|ui| {
                    ui.label(t!("windows.stinger_n", n = n + 1));
                    input_combo(ui, "stinger", &mut st.input, project, 0);
                });
                ui.add(egui::Slider::new(&mut st.duration_ms, 100..=10000).text(t!("windows.duration_ms")));
                let max = st.duration_ms;
                ui.add(egui::Slider::new(&mut st.cut_ms, 0..=max).text(t!("windows.cut_point_ms")));
                ui.checkbox(&mut st.under_overlays, t!("windows.under_overlays"));
            }
        });
        self.overlays_open = open;
        if cfgs != project.overlay_cfg || stingers != project.stingers {
            // Only the slot edited: the rest may have changed from elsewhere.
            self.with(|s| {
                for (n, c) in cfgs.iter().enumerate().filter(|(n, c)| **c != project.overlay_cfg[*n]) {
                    s.project.overlay_cfg[n] = *c;
                }
                for (n, c) in stingers.iter().enumerate().filter(|(n, c)| **c != project.stingers[*n]) {
                    s.project.stingers[n] = *c;
                }
                s.version += 1;
            });
        }
    }

    pub(super) fn playlist_window(&mut self, ctx: &egui::Context, project: &Project) {
        if !self.playlist_open {
            return;
        }
        let mut open = true;
        let mut list = project.playlist.clone();
        let window = egui::Window::new(t!("windows.playlist_title")).id(egui::Id::unique("Playlist"));
        window.open(&mut open).default_width(480.0).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.checkbox(&mut list.looped, t!("windows.loop"));
                ui.checkbox(&mut list.manual, t!("windows.manual"));
                ui.checkbox(&mut list.clear_overlays, t!("windows.clear_overlays"));
            });
            let mut remove = None;
            let mut up = None;
            for (n, it) in list.items.iter_mut().enumerate() {
                ui.push_id(n, |ui| {
                    ui.horizontal(|ui| {
                        let name = project
                            .inputs
                            .iter()
                            .find(|i| i.id == it.input)
                            .map_or(String::new(), |i| i.name.clone());
                        ui.label(RichText::new(name).strong());
                        let mut secs = it.duration_ms as f32 / 1000.0;
                        let d = egui::DragValue::new(&mut secs).range(0.0..=3600.0).suffix(" s");
                        if ui.add(d).changed() {
                            it.duration_ms = (secs * 1000.0) as u64;
                        }
                        egui::ComboBox::from_id_salt("tr").selected_text(it.trans.label()).show_ui(
                            ui,
                            |ui| {
                                for k in TransKind::ALL {
                                    ui.selectable_value(&mut it.trans, k, k.label());
                                }
                            },
                        );
                        ui.add(egui::DragValue::new(&mut it.trans_ms).range(0..=5000).suffix(" ms"));
                        ui.checkbox(&mut it.overlay, t!("windows.overlay"));
                        if ui.small_button(t!("windows.move_up")).clicked() && n > 0 {
                            up = Some(n);
                        }
                        if ui.small_button(t!("windows.remove")).clicked() {
                            remove = Some(n);
                        }
                    });
                });
            }
            if let Some(n) = up {
                list.items.swap(n, n - 1);
            }
            if let Some(n) = remove {
                list.items.remove(n);
            }
            ui.menu_button(t!("windows.add_entry"), |ui| {
                for i in project.inputs.iter().filter(|i| i.kind.has_picture()) {
                    if ui.button(&i.name).clicked() {
                        list.items.push(PlaylistItem {
                            input: i.id,
                            start_ms: None,
                            duration_ms: 10_000,
                            trans: TransKind::Fade,
                            trans_ms: 500,
                            overlay: false,
                        });
                        ui.close();
                    }
                }
            });
            let running = self.with(|s| s.playlist_on);
            ui.horizontal(|ui| {
                let label = if running { t!("windows.stop") } else { t!("windows.start") };
                if ui.button(label).clicked() {
                    let f = if running { "StopPlayList" } else { "StartPlayList" };
                    self.call(f, &Args::new());
                }
                if ui.button(t!("windows.next")).clicked() {
                    self.call("NextPlayListEntry", &Args::new());
                }
            });
        });
        self.playlist_open = open;
        if list != project.playlist {
            self.with(|s| s.project.playlist = list);
        }
    }

    pub(super) fn categories_window(&mut self, ctx: &egui::Context, project: &Project) {
        if !self.categories_open {
            return;
        }
        let mut open = true;
        let mut cats = project.categories.clone();
        let window =
            egui::Window::new(t!("windows.categories_title")).id(egui::Id::unique("Categories d'entrees"));
        window.open(&mut open).show(ctx, |ui| {
            for (n, c) in cats.iter_mut().enumerate() {
                ui.push_id(n, |ui| {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut c.enabled, "");
                        ui.color_edit_button_srgb(&mut c.color);
                        ui.text_edit_singleline(&mut c.name);
                    });
                });
            }
            if cats.len() < 22 && ui.button(t!("windows.add_category")).clicked() {
                cats.push(Category { name: String::new(), color: [120, 120, 130], enabled: true });
            }
        });
        self.categories_open = open;
        if cats != project.categories {
            self.with(|s| s.project.categories = cats);
        }
    }

    /// Says plainly what opening a .vmix kept and what it left behind.
    pub(super) fn import_window(&mut self, ctx: &egui::Context) {
        let Some(r) = self.import_report.clone() else { return };
        if self.shot_path.is_some() {
            return;
        }
        let mut open = true;
        let window = egui::Window::new(t!("windows.import_title")).id(egui::Id::unique("Projet vMix ouvert"));
        window.open(&mut open).default_width(460.0).show(ctx, |ui| {
            ui.label(RichText::new(&r.name).strong());
            ui.label(t!(
                "windows.import_counts",
                inputs = r.inputs,
                layers = r.layers,
                overlays = r.overlays
            ));
            ui.label(RichText::new(t!("windows.import_untouched")).small().color(DIM));
            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                if !r.placeholders.is_empty() {
                    ui.label(RichText::new(t!("windows.import_placeholders")).strong());
                    r.placeholders.iter().for_each(|p| drop(ui.label(p)));
                }
                if !r.missing_files.is_empty() {
                    ui.label(RichText::new(t!("windows.import_missing")).strong());
                    r.missing_files.iter().for_each(|p| drop(ui.label(p)));
                }
                if !r.notes.is_empty() {
                    ui.label(RichText::new(t!("windows.import_notes")).strong());
                    r.notes.iter().for_each(|p| drop(ui.label(p)));
                }
            });
        });
        if !open {
            self.import_report = None;
        }
    }

    pub(super) fn help_window(&mut self, ctx: &egui::Context) {
        let mut open = self.help_open;
        let window = egui::Window::new(t!("windows.help_title")).id(egui::Id::unique("Raccourcis"));
        window.open(&mut open).resizable(false).show(ctx, |ui| {
            for (key, what) in [
                (t!("windows.help.key_digits"), t!("windows.help.preview_input")),
                (t!("windows.help.key_shift_digits"), t!("windows.help.input_on_air")),
                (t!("windows.help.key_space"), t!("windows.help.cut")),
                (t!("windows.help.key_enter"), t!("windows.help.take_on_air")),
                (t!("windows.help.key_escape"), t!("windows.help.close_output_screen")),
                ("F11", t!("windows.help.output_fullscreen")),
                (t!("windows.help.key_right_click_title"), t!("windows.help.collapse_input")),
                (t!("windows.help.key_right_click_tab"), t!("windows.help.manage_categories")),
                (t!("windows.help.key_drag_thumbnail"), t!("windows.help.reorder")),
            ] {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(key).strong());
                    ui.label(RichText::new(what).color(DIM));
                });
            }
        });
        self.help_open = open;
    }

    /// A project dropped on the window replaces the desk, so it asks first.
    fn confirm_open_window(&mut self, ctx: &egui::Context) {
        let Some(path) = self.confirm_open.clone() else { return };
        egui::Window::new(t!("windows.confirm_open_title"))
            .id(egui::Id::unique("Ouvrir ce projet"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .show(ctx, |ui| {
                let name = file_name(std::path::Path::new(&path));
                ui.label(t!("windows.confirm_open_question", name = name));
                ui.horizontal(|ui| {
                    if ui.button(t!("windows.open")).clicked() {
                        self.confirm_open = None;
                        self.open_path(path.clone());
                    }
                    if ui.button(t!("windows.cancel")).clicked() {
                        self.confirm_open = None;
                    }
                });
            });
    }

    pub(super) fn confirm_window(&mut self, ctx: &egui::Context) {
        self.confirm_open_window(ctx);
        if !self.confirm_new {
            return;
        }
        egui::Window::new(t!("windows.new_project_title"))
            .id(egui::Id::unique("Nouveau projet"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(t!("windows.new_project_question"));
                ui.horizontal(|ui| {
                    if ui.button(t!("windows.clear_all")).clicked() {
                        self.engine.load_project(Project::blank());
                        self.after_load(None, None);
                        self.confirm_new = false;
                    }
                    if ui.button(t!("windows.cancel")).clicked() {
                        self.confirm_new = false;
                    }
                });
            });
    }

    /// Fullscreen 1, Fullscreen 2 and the External window.
    pub(super) fn screen_windows(&mut self, ctx: &egui::Context, project: &Project) {
        let mut routes = [None; ROUTES];
        for n in 0..ROUTES {
            if !self.windows_on[n] {
                continue;
            }
            let route = if n < 2 { project.fullscreen_route[n] } else { self.external_route };
            // The program picture is already on the GPU; anything else travels.
            if route != Route::Output {
                routes[n] = Some(route);
            }
            let tex = match route {
                Route::Output => self.program_tex.as_ref().map(|t| t.id()),
                _ => self.route_tex[n].as_ref().map(|t| t.id()),
            };
            let aspect = project.out_w as f32 / project.out_h.max(1) as f32;
            let close = self.show_screen(ctx, n, tex, aspect, project);
            if close {
                self.windows_on[n] = false;
            }
        }
        *self.engine.routes.lock().unwrap() = routes;
    }

    fn show_screen(
        &self,
        ctx: &egui::Context,
        n: usize,
        tex: Option<egui::TextureId>,
        aspect: f32,
        project: &Project,
    ) -> bool {
        let titles = [
            t!("windows.screen.fullscreen_1"),
            t!("windows.screen.fullscreen_2"),
            t!("windows.screen.external"),
        ];
        let ids = ["Plein ecran 1", "Plein ecran 2", "Sortie externe"];
        let mut builder = egui::ViewportBuilder::default()
            .with_title(t!("windows.screen_title", screen = titles[n]))
            .with_inner_size([960.0, 540.0]);
        // Fullscreen goes to the chosen display, by default the second one.
        if n < 2 && self.want_fullscreen[n] {
            let index = project.fullscreen_monitor[n].saturating_sub(1);
            if self.screens.len() > 1
                && let Some((x, y, w, h)) = self.screens.get(index).copied()
            {
                builder = builder
                    .with_position([x as f32, y as f32])
                    .with_inner_size([w as f32, h as f32])
                    .with_decorations(false);
            }
        }
        let mut close = false;
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of(ids[n]), builder, |ui, _class| {
            let vctx = ui.ctx().clone();
            let (esc, f11, closing, full) = vctx.input(|i| {
                (
                    i.key_pressed(egui::Key::Escape),
                    i.key_pressed(egui::Key::F11),
                    i.viewport().close_requested(),
                    i.viewport().fullscreen.unwrap_or(false),
                )
            });
            if f11 {
                vctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!full));
            }
            close = esc || closing;
            let black = egui::Frame::new().fill(Color32::BLACK);
            egui::CentralPanel::default().frame(black).show(ui, |ui| {
                let rect = fit(ui.max_rect(), aspect);
                if let Some(t) = tex {
                    ui.painter().image(t, rect, full_uv(), Color32::WHITE);
                }
            });
        });
        close
    }
}
