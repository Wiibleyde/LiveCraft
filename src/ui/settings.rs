//! The Settings window, one tab per subject.

use super::*;
use crate::api::{HTTP_PORT, TCP_PORT};

fn tabs() -> [&'static str; 11] {
    [
        t!("settings.tab.format"),
        t!("settings.tab.options"),
        t!("settings.tab.outputs"),
        t!("settings.tab.recording"),
        t!("settings.tab.streaming"),
        t!("settings.tab.audio"),
        t!("settings.tab.shortcuts"),
        t!("settings.tab.scripts"),
        t!("settings.tab.data"),
        t!("settings.tab.remote"),
        t!("settings.tab.about"),
    ]
}

const FORMATS: [(&str, usize, usize); 6] = [
    ("480p", 854, 480),
    ("720p", 1280, 720),
    ("1080p", 1920, 1080),
    ("UHD", 3840, 2160),
    ("4K DCI", 4096, 2160),
    ("Vertical HD", 1080, 1920),
];

impl App {
    pub(super) fn settings_window(&mut self, ctx: &egui::Context, project: &Project) {
        if !self.settings_open {
            return;
        }
        let mut open = true;
        let mut p = project.clone();
        let window = egui::Window::new(t!("settings.title")).id(egui::Id::unique("Reglages"));
        window.open(&mut open).default_size([680.0, 480.0]).show(ctx, |ui| {
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(110.0);
                    for (n, name) in tabs().into_iter().enumerate() {
                        ui.selectable_value(&mut self.settings_tab, n, name);
                    }
                });
                ui.separator();
                ui.vertical(|ui| {
                    egui::ScrollArea::vertical().max_height(500.0).show(ui, |ui| {
                        ui.set_min_width(500.0);
                        match self.settings_tab {
                            0 => self.tab_format(ui, &p),
                            1 => {
                                self.language_choice(ui);
                                tab_options(ui, &mut p);
                            }
                            2 => self.tab_outputs(ui, &mut p),
                            3 => self.tab_recording(ui, &mut p),
                            4 => self.tab_streaming(ui, &mut p),
                            5 => self.tab_audio_devices(ui, &mut p),
                            6 => tab_shortcuts(ui, &mut p),
                            7 => self.tab_scripts(ui, &mut p),
                            8 => tab_data(ui, &mut p),
                            9 => self.tab_remote(ui),
                            _ => self.tab_about(ui, &p),
                        }
                    });
                });
            });
        });
        self.settings_open = open;
        if p != *project {
            // Only what this window edits: the desk may have moved on since
            // the start of the frame, and that must not be undone.
            self.with(|s| {
                let d = &mut s.project;
                d.quick_play = p.quick_play;
                d.show_clocks = p.show_clocks;
                d.fullscreen_route = p.fullscreen_route;
                d.fullscreen_monitor = p.fullscreen_monitor;
                d.srt = p.srt;
                d.ndi_out = p.ndi_out;
                d.omt_out = p.omt_out;
                d.records = p.records;
                d.streams = p.streams;
                d.master_device = p.master_device;
                d.headphones_device = p.headphones_device;
                d.ftb_audio = p.ftb_audio;
                for (bus, edited) in d.buses.iter_mut().zip(&p.buses) {
                    bus.device.clone_from(&edited.device);
                }
                d.shortcuts = p.shortcuts;
                d.scripts = p.scripts;
                d.data_sources = p.data_sources;
            });
            if p.hardware_decoding != project.hardware_decoding {
                self.engine.set_hardware_decoding(p.hardware_decoding);
            }
        }
    }

    /// Each language under its own name, so that it can be found from any other.
    fn language_choice(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new(t!("settings.language")).strong());
        ui.horizontal(|ui| {
            let current = language();
            for lang in Lang::ALL {
                if ui.selectable_label(lang == current, lang.native_name()).clicked() && lang != current {
                    self.set_language(lang);
                }
            }
        });
        ui.separator();
    }

    fn tab_format(&mut self, ui: &mut egui::Ui, p: &Project) {
        let e = &self.engine;
        let busy =
            e.is_recording() || e.is_streaming() || e.is_multi() || e.is_srt() || e.is_net() || e.is_replay();
        ui.label(RichText::new(t!("settings.output_format")).strong());
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal_wrapped(|ui| {
                for (label, w, h) in FORMATS {
                    let on = (self.format_draft.0, self.format_draft.1) == (w, h);
                    if ui.selectable_label(on, label).clicked() {
                        self.format_draft.0 = w;
                        self.format_draft.1 = h;
                    }
                }
            });
            ui.horizontal(|ui| {
                for fps in [24, 25, 30, 50, 60] {
                    ui.selectable_value(&mut self.format_draft.2, fps, t!("settings.fps", fps = fps));
                }
            });
            if self.format_draft != (p.out_w, p.out_h, p.fps) && ui.button(t!("settings.apply")).clicked() {
                let (w, h, fps) = self.format_draft;
                self.engine.set_format(w, h, fps);
                self.thumbs.clear();
            }
        });
        if busy {
            ui.label(RichText::new(t!("settings.stop_outputs_to_change_format")).small().color(DIM));
        }
        ui.separator();
        ui.label(RichText::new(t!("settings.compositing")).strong());
        let mut on = e.use_gpu.load(Ordering::Relaxed);
        if ui.checkbox(&mut on, t!("settings.compose_on_gpu")).changed() {
            e.use_gpu.store(on, Ordering::Relaxed);
        }
        let line = match e.compositor.lock().unwrap().clone() {
            Compositor::Starting => t!("settings.compositor_starting").to_string(),
            Compositor::Gpu(name) => t!("settings.on_gpu", name = name),
            // Wanted but refused or given up: it can be tried again.
            Compositor::Cpu(why) if on => {
                format!("{}\n{}", t!("settings.on_cpu", reason = why), t!("settings.gpu_try_again"))
            }
            Compositor::Cpu(why) => t!("settings.on_cpu", reason = why),
        };
        ui.label(RichText::new(line).small().color(DIM));
    }

    fn tab_outputs(&mut self, ui: &mut egui::Ui, p: &mut Project) {
        ui.label(RichText::new(t!("settings.fullscreen")).strong());
        for n in 0..2 {
            ui.horizontal(|ui| {
                let label = t!("settings.screen_n", n = n + 1);
                ui.checkbox(&mut self.windows_on[n], label);
                let count = self.screens.len().max(1);
                ui.add(
                    egui::DragValue::new(&mut p.fullscreen_monitor[n])
                        .range(1..=count)
                        .prefix(format!("{} ", t!("settings.monitor"))),
                );
                let route = p.fullscreen_route[n];
                let mut picked = route;
                let shown = route_name(route, p);
                egui::ComboBox::from_id_salt(("fs", n)).selected_text(shown).show_ui(ui, |ui| {
                    self.route_list(ui, &mut picked, p);
                });
                p.fullscreen_route[n] = picked;
            });
        }
        ui.separator();
        ui.label(RichText::new(t!("settings.external_output")).strong());
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.windows_on[2], t!("settings.active"));
            let mut picked = self.external_route;
            let shown = route_name(picked, p);
            egui::ComboBox::from_id_salt("ext").selected_text(shown).show_ui(ui, |ui| {
                self.route_list(ui, &mut picked, p);
            });
            self.external_route = picked;
        });
        ui.separator();
        ui.label(RichText::new(t!("settings.srt_outputs")).strong());
        for (n, o) in p.srt.iter_mut().enumerate() {
            ui.push_id(n, |ui| {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut o.enabled, format!("{}", n + 1));
                    ui.add(egui::TextEdit::singleline(&mut o.url).desired_width(260.0));
                    ui.add(egui::DragValue::new(&mut o.kbps).range(500..=50000).suffix(" kb/s"));
                });
            });
        }
        let running = self.engine.is_srt();
        let label = if running { t!("settings.stop_srt") } else { t!("settings.start_srt") };
        if ui.button(label).clicked() {
            if running && self.with(|s| s.padlock) {
                self.say(t!("settings.locked_unlock_to_stop"));
            } else {
                let name = if running { "StopSRTOutput" } else { "StartSRTOutput" };
                self.call(name, &Args::new());
            }
        }
        ui.separator();
        ui.label(RichText::new(t!("settings.net_title")).strong());
        egui::Grid::new("net_out").show(ui, |ui| {
            for (label, cfg) in [("NDI", &mut p.ndi_out), ("OMT", &mut p.omt_out)] {
                ui.checkbox(&mut cfg.enabled, label);
                ui.label(t!("settings.net_name"));
                ui.add_sized([220.0, 20.0], egui::TextEdit::singleline(&mut cfg.name).id_salt(label));
                ui.end_row();
            }
        });
        let running: Vec<String> = self
            .engine
            .net
            .lock()
            .unwrap()
            .iter()
            .map(|o| {
                t!("settings.net_connections", net = o.net.label(), name = o.name, count = o.connections())
            })
            .collect();
        for line in &running {
            ui.label(RichText::new(line).small().color(DIM));
        }
        let on = !running.is_empty();
        if ui.button(if on { t!("settings.net_stop") } else { t!("settings.net_start") }).clicked() {
            if on && self.with(|s| s.padlock) {
                self.say(t!("settings.locked_unlock_to_stop"));
            } else if on {
                self.engine.stop_net();
            } else if let Err(e) = self.engine.start_net() {
                self.say(e);
            }
        }
        if on {
            ui.label(RichText::new(t!("settings.net_rename_note")).small().color(DIM));
        }
    }

    fn codec_combo(&self, ui: &mut egui::Ui, salt: &str, codec: &mut String) {
        let list = self.encoders.lock().unwrap().clone();
        egui::ComboBox::from_id_salt(salt).selected_text(codec_label(codec)).show_ui(ui, |ui| {
            for e in &list {
                ui.selectable_value(codec, e.clone(), codec_label(e));
            }
        });
    }

    fn tab_recording(&mut self, ui: &mut egui::Ui, p: &mut Project) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(&self.saved.record_dir).small());
            if ui.button(t!("settings.folder")).clicked()
                && let Some(d) = rfd::FileDialog::new().pick_folder()
            {
                self.saved.record_dir = d.to_string_lossy().into_owned();
            }
        });
        for n in 0..2 {
            let r = &mut p.records[n];
            ui.separator();
            ui.push_id(n, |ui| {
                ui.checkbox(&mut r.enabled, t!("settings.recorder_n", n = n + 1));
                ui.horizontal(|ui| {
                    for c in ["mp4", "mkv", "mov", "ts"] {
                        ui.selectable_value(&mut r.container, c.to_string(), c);
                    }
                    self.codec_combo(ui, "codec", &mut r.codec);
                });
                ui.add(egui::Slider::new(&mut r.video_kbps, 1000..=100000).text(t!("settings.video_kbps")));
                ui.add(egui::Slider::new(&mut r.audio_kbps, 64..=320).text(t!("settings.audio_kbps")));
                ui.add(egui::Slider::new(&mut r.split_minutes, 0..=240).text(t!("settings.split_minutes")));
                audio_pick(ui, "audio", &mut r.audio);
            });
        }
    }

    fn tab_streaming(&mut self, ui: &mut egui::Ui, p: &mut Project) {
        ui.label(RichText::new(t!("settings.up_to_five_destinations")).small().color(DIM));
        for n in 0..5 {
            let s = &mut p.streams[n];
            ui.separator();
            ui.push_id(n, |ui| {
                ui.checkbox(&mut s.enabled, t!("settings.destination_n", n = n + 1));
                ui.horizontal(|ui| {
                    ui.label(t!("settings.server"));
                    ui.add(egui::TextEdit::singleline(&mut s.url).desired_width(300.0));
                });
                ui.horizontal(|ui| {
                    ui.label(t!("settings.stream_key"));
                    ui.add(egui::TextEdit::singleline(&mut s.key).password(true).desired_width(300.0));
                });
                if n > 0 {
                    ui.checkbox(&mut s.shared, t!("settings.same_quality"));
                }
                if n == 0 || !s.shared {
                    ui.add(egui::Slider::new(&mut s.kbps, 500..=50000).text(t!("settings.video_kbps")));
                    self.codec_combo(ui, "codec", &mut s.codec);
                }
            });
        }
    }

    fn tab_audio_devices(&mut self, ui: &mut egui::Ui, p: &mut Project) {
        let devices = self.engine.audio_devices.lock().unwrap().clone();
        device_combo(ui, t!("settings.master_device"), &mut p.master_device, &devices);
        device_combo(ui, t!("settings.headphones_device"), &mut p.headphones_device, &devices);
        ui.separator();
        for (n, name) in ["A", "B", "C", "D", "E", "F", "G"].iter().enumerate() {
            device_combo(ui, &t!("settings.bus_n", name = name), &mut p.buses[n].device, &devices);
        }
        ui.separator();
        ui.checkbox(&mut p.ftb_audio, t!("settings.ftb_mutes_audio"));
    }

    fn tab_scripts(&mut self, ui: &mut egui::Ui, p: &mut Project) {
        ui.label(
            RichText::new(
                "API.Function(\"Fade\",Input:=\"1\",Duration:=1000), Sleep(1000), For/Next, Do/Loop",
            )
            .small()
            .color(DIM),
        );
        let mut remove = None;
        let mut run = None;
        for (n, s) in p.scripts.iter_mut().enumerate() {
            ui.push_id(n, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut s.name).desired_width(160.0));
                    if ui.button(t!("settings.run")).clicked() {
                        run = Some(s.code.clone());
                    }
                    if ui.button(t!("settings.remove")).clicked() {
                        remove = Some(n);
                    }
                });
                ui.add(
                    egui::TextEdit::multiline(&mut s.code).code_editor().desired_rows(5).desired_width(480.0),
                );
            });
        }
        if let Some(n) = remove {
            p.scripts.remove(n);
        }
        if let Some(code) = run {
            self.engine.run_script(code);
        }
        ui.horizontal(|ui| {
            if ui.button(t!("settings.add_script")).clicked() {
                let name = t!("settings.script_n", n = p.scripts.len() + 1);
                p.scripts.push(Script { name, code: String::new() });
            }
            if ui.button(t!("settings.stop_all")).clicked() {
                self.call("ScriptStopAll", &Args::new());
            }
        });
    }

    fn tab_remote(&mut self, ui: &mut egui::Ui) {
        ui.label(t!("settings.remote_ports", http = HTTP_PORT, tcp = TCP_PORT));
        ui.label(RichText::new(t!("settings.remote_local", port = HTTP_PORT)).small());
        ui.label(RichText::new(t!("settings.remote_call_links")).small());
        ui.checkbox(&mut self.saved.remote_lan, t!("settings.remote_lan"));
        ui.horizontal(|ui| {
            ui.label(t!("settings.password"));
            ui.add(egui::TextEdit::singleline(&mut self.saved.remote_password).password(true));
        });
        ui.label(RichText::new(t!("settings.restart_needed")).small().color(DIM));
    }
}

fn route_name(route: Route, p: &Project) -> String {
    match route {
        Route::Output => t!("settings.route.output").into(),
        Route::Preview => t!("settings.route.preview").into(),
        Route::MultiView => "MultiView".into(),
        Route::MultiView2 => "MultiView 2".into(),
        Route::Input(id) => p.inputs.iter().find(|i| i.id == id).map_or("—".into(), |i| i.name.clone()),
    }
}

pub(super) fn audio_pick(ui: &mut egui::Ui, salt: &str, pick: &mut AudioPick) {
    ui.horizontal(|ui| {
        ui.label(t!("settings.audio"));
        egui::ComboBox::from_id_salt(salt).selected_text(pick.label()).show_ui(ui, |ui| {
            for (name, value) in AudioPick::all() {
                ui.selectable_value(pick, value, name);
            }
        });
    });
}

fn tab_options(ui: &mut egui::Ui, p: &mut Project) {
    ui.label(RichText::new(t!("settings.take_on_air")).strong());
    ui.horizontal(|ui| {
        let t = &mut p.quick_play;
        egui::ComboBox::from_id_salt("rapide").selected_text(t.kind.label()).show_ui(ui, |ui| {
            for k in TransKind::ALL {
                ui.selectable_value(&mut t.kind, k, k.label());
            }
        });
        ui.add(egui::DragValue::new(&mut t.ms).range(50..=5000).suffix(" ms"));
    });
    ui.separator();
    ui.checkbox(&mut p.show_clocks, t!("settings.production_clocks"));
    ui.separator();
    ui.checkbox(&mut p.hardware_decoding, t!("settings.hardware_decoding"))
        .on_hover_text(t!("settings.hardware_decoding_hint"));
}

fn tab_shortcuts(ui: &mut egui::Ui, p: &mut Project) {
    ui.label(RichText::new(t!("settings.shortcuts_hint")).small().color(DIM));
    let mut remove = None;
    for (n, s) in p.shortcuts.iter_mut().enumerate() {
        ui.push_id(n, |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut s.key).hint_text("F1").desired_width(60.0));
                ui.add(
                    egui::TextEdit::singleline(&mut s.function)
                        .hint_text(t!("settings.function"))
                        .desired_width(160.0),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut s.input)
                        .hint_text(t!("settings.input"))
                        .desired_width(80.0),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut s.value)
                        .hint_text(t!("settings.value"))
                        .desired_width(80.0),
                );
                if ui.small_button(t!("settings.remove")).clicked() {
                    remove = Some(n);
                }
            });
        });
    }
    if let Some(n) = remove {
        p.shortcuts.remove(n);
    }
    if ui.button(t!("settings.add_shortcut")).clicked() {
        p.shortcuts.push(Shortcut::default());
    }
}

fn tab_data(ui: &mut egui::Ui, p: &mut Project) {
    ui.label(RichText::new(t!("settings.data_hint")).small().color(DIM));
    let mut remove = None;
    for (n, d) in p.data_sources.iter_mut().enumerate() {
        ui.push_id(n, |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(120.0));
                if ui.button(t!("settings.file")).clicked()
                    && let Some(f) = rfd::FileDialog::new().pick_file()
                {
                    d.path = f.to_string_lossy().into_owned();
                }
                ui.add(egui::DragValue::new(&mut d.row).prefix(format!("{} ", t!("settings.row"))));
                ui.add(egui::DragValue::new(&mut d.auto_next_secs).range(0.0..=600.0).suffix(" s"));
                if ui.small_button(t!("settings.remove")).clicked() {
                    remove = Some(n);
                }
            });
        });
    }
    if let Some(n) = remove {
        p.data_sources.remove(n);
    }
    if ui.button(t!("settings.add_source")).clicked() {
        p.data_sources.push(DataSource {
            name: t!("settings.source_n", n = p.data_sources.len() + 1),
            ..DataSource::default()
        });
    }
}

/// Whether the project has an SRT output on or an SRT input.
fn uses_srt(p: &Project) -> bool {
    p.srt.iter().any(|o| o.enabled)
        || p.inputs
            .iter()
            .any(|i| matches!(&i.kind, Kind::Network { url, .. } if url.trim_start().starts_with("srt:")))
}

impl App {
    fn tab_about(&mut self, ui: &mut egui::Ui, p: &Project) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("LiveCraft").strong());
            ui.label(t!("settings.about_version", version = crate::update::CURRENT));
        });
        ui.label(t!("settings.about_tagline"));
        ui.label(RichText::new(t!("settings.about_ffmpeg")).small().color(DIM));
        ui.label(RichText::new(t!("settings.about_ndi")).small().color(DIM));
        ui.separator();
        self.about_updates(ui);
        ui.separator();
        self.ffmpeg_section(ui, p);
    }

    /// ffmpeg is only asked when this is a real run: tests and screenshots
    /// start no ffmpeg of their own to ask about.
    fn ffmpeg_section(&mut self, ui: &mut egui::Ui, p: &Project) {
        use crate::ffmpeg::{self, How, Problem};
        ui.label(RichText::new("FFmpeg").strong());
        let lookup = ffmpeg::lookup();
        let caps = ffmpeg::capabilities_if_known();
        if let Some(f) = ffmpeg::located() {
            let how = match f.how {
                How::Variable => t!("settings.ffmpeg_how_variable", name = ffmpeg::VARIABLE),
                How::Beside => t!("settings.ffmpeg_how_beside").to_string(),
                How::Path => t!("settings.ffmpeg_how_path").to_string(),
            };
            ui.label(t!("settings.ffmpeg_path", path = f.path.display().to_string(), how = how));
            match caps {
                None if !self.asks_ffmpeg => {
                    ui.label(RichText::new(t!("settings.ffmpeg_not_asked")).small().color(DIM));
                }
                None => {
                    ui.label(RichText::new(t!("settings.ffmpeg_asking")).small().color(DIM));
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
                }
                Some(c) if c.answered() => {
                    ui.label(t!("settings.ffmpeg_version", version = c.version.clone().unwrap_or_default()));
                    let answer = if c.srt { t!("settings.ffmpeg_yes") } else { t!("settings.ffmpeg_no") };
                    ui.label(t!("settings.ffmpeg_srt", answer = answer));
                    self.hardware_tests(ui, c);
                }
                Some(_) => {}
            }
        }
        let message = match ffmpeg::problem(lookup, caps, uses_srt(p)) {
            Some(Problem::Missing) => Some(t!("settings.ffmpeg_missing", name = ffmpeg::VARIABLE)),
            Some(Problem::BadVariable(v)) => {
                Some(t!("settings.ffmpeg_bad_variable", name = ffmpeg::VARIABLE, value = v))
            }
            Some(Problem::NoAnswer) => Some(t!("settings.ffmpeg_no_answer", name = ffmpeg::VARIABLE)),
            Some(Problem::NoSrt) => Some(t!("settings.ffmpeg_no_srt", name = ffmpeg::VARIABLE)),
            None => None,
        };
        if let Some(m) = message {
            ui.colored_label(egui::Color32::LIGHT_RED, m);
        }
    }

    /// The hardware encoders ffmpeg lists, each with what happened when it was
    /// really tried. Trying is heavy, so it starts when this is first looked
    /// at, and again on request.
    fn hardware_tests(&self, ui: &mut egui::Ui, caps: &crate::ffmpeg::Capabilities) {
        use crate::ffmpeg::{self, Test};
        let listed = caps.hardware();
        if listed.is_empty() {
            ui.label(t!("settings.ffmpeg_hw_none"));
            return;
        }
        if self.asks_ffmpeg && !ffmpeg::test_view().started {
            ffmpeg::start_tests();
        }
        let view = ffmpeg::test_view();
        ui.label(t!("settings.ffmpeg_hw_title"));
        for name in &listed {
            let text = match view.results.iter().find(|(n, _)| n == name) {
                Some((_, Test::Works)) => t!("settings.ffmpeg_hw_works", name = name),
                Some((_, Test::Failed(why))) if why.is_empty() => {
                    t!("settings.ffmpeg_hw_failed", name = name, why = t!("settings.ffmpeg_hw_no_detail"))
                }
                Some((_, Test::Failed(why))) => t!("settings.ffmpeg_hw_failed", name = name, why = why),
                None if view.running => t!("settings.ffmpeg_hw_trying", name = name),
                None => t!("settings.ffmpeg_hw_unknown", name = name),
            };
            ui.label(format!("  {text}"));
        }
        ui.label(RichText::new(t!("settings.ffmpeg_hw_note")).small().color(DIM));
        if view.running {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(300));
        } else if self.asks_ffmpeg && ui.button(t!("settings.ffmpeg_hw_recheck")).clicked() {
            ffmpeg::start_tests();
        }
    }
}
