//! The window: preview left, output right, transitions between them, coloured
//! tabs and input cards below, audio mixer bottom right. It only reads shared
//! state and sends short commands, and it repaints when the engine has
//! something new instead of running flat out.

mod audio_panel;
mod bars;
mod input_settings;
mod inputs;
mod settings;
mod switcher;
#[cfg(test)]
mod tests;
mod tools;
mod virtual_set;
mod windows;

use crate::api::ApiConfig;
use crate::engine::{Engine, ROUTES, State};
use crate::functions::Args;
use crate::i18n::{Lang, language, set_language};
use crate::model::*;
use crate::output;
use crate::secret;
use crate::sources;
use crate::widgets::*;
use eframe::egui::{self, Color32, Rect, RichText, Sense, Stroke, pos2, vec2};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const CARD_W: f32 = 214.0;
const CARD_H: f32 = 182.0;
const BASIC_CARD_H: f32 = 146.0;
const STRIP_W: f32 = 26.0;
const MID_W: f32 = 124.0;
const TABS_H: f32 = 24.0;
const AUDIO_W: f32 = 450.0;
const SHOT_AFTER_FRAMES: u32 = 120;

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Saved {
    project: Project,
    record_dir: String,
    last_file: Option<String>,
    remote_lan: bool,
    remote_password: String,
    /// Interface language code; empty follows the system's.
    language: String,
}

fn user_dir(sub: &str) -> PathBuf {
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(sub).join("LiveCraft")
}

fn saved_path() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("LiveCraft").join("session.json")
}

/// Where the session lived when the program was called Regie.
fn older_saved_path() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("Regie").join("session.json")
}

/// A new name must not cost anyone their desk: the old session is read until
/// the first save under the new name.
fn session_to_read(current: PathBuf, older: PathBuf) -> PathBuf {
    if !current.exists() && older.exists() { older } else { current }
}

impl Default for Saved {
    fn default() -> Self {
        Self::fresh(Project::blank())
    }
}

impl Saved {
    /// Everything secret in the session. Addresses only count when they hold
    /// a passphrase, or were sealed because they did.
    fn each_secret(&mut self, f: impl Fn(&str) -> String) {
        let sensitive = |u: &str| u.contains("passphrase=") || secret::is_sealed(u);
        self.remote_password = f(&self.remote_password);
        for d in &mut self.project.streams {
            d.key = f(&d.key);
        }
        for o in self.project.srt.iter_mut().filter(|o| sensitive(&o.url)) {
            o.url = f(&o.url);
        }
        for i in &mut self.project.inputs {
            if let Kind::Network { url, .. } = &mut i.kind
                && sensitive(url)
            {
                *url = f(url);
            }
        }
    }

    fn fresh(project: Project) -> Self {
        Self {
            project,
            record_dir: user_dir("Videos").to_string_lossy().into_owned(),
            last_file: None,
            remote_lan: false,
            remote_password: String::new(),
            language: String::new(),
        }
    }

    fn load() -> Self {
        let path = session_to_read(saved_path(), older_saved_path());
        let Ok(bytes) = std::fs::read(&path) else { return Self::default() };
        match serde_json::from_slice::<Self>(&bytes) {
            Ok(mut saved) => {
                saved.each_secret(secret::open);
                saved
            }
            Err(_) => {
                // Set aside, not overwritten: the empty desk that starts now
                // must not erase a session someone could still recover.
                let _ = std::fs::rename(&path, path.with_extension("illisible.json"));
                Self::default()
            }
        }
    }

    fn store(&self) {
        let path = saved_path();
        let mut sealed = self.clone();
        sealed.each_secret(secret::seal);
        if let (Some(dir), Ok(json)) = (path.parent(), serde_json::to_vec_pretty(&sealed)) {
            let _ = std::fs::create_dir_all(dir);
            // Written beside and swapped in: a crash half way through a write
            // must leave the previous session whole.
            let next = path.with_extension("tmp");
            if std::fs::write(&next, json).is_ok() {
                let _ = std::fs::rename(next, path);
            }
        }
    }
}

pub struct App {
    engine: Arc<Engine>,
    saved: Saved,
    program_tex: Option<egui::TextureHandle>,
    preview_tex: Option<egui::TextureHandle>,
    route_tex: [Option<egui::TextureHandle>; ROUTES],
    program_seen: u64,
    preview_seen: u64,
    route_seen: u64,
    thumbs: HashMap<u64, (egui::TextureHandle, u64, f32)>,
    editing: Option<u64>,
    edit_tab: usize,
    selected_layer: usize,
    picking_colour: bool,
    settings_open: bool,
    settings_tab: usize,
    playlist_open: bool,
    overlays_open: bool,
    overlay_pick: usize,
    categories_open: bool,
    clocks_open: bool,
    stats_open: bool,
    help_open: bool,
    multi_open: bool,
    multi_ids: Vec<u64>,
    multi_audio: AudioPick,
    replay_open: bool,
    replay_cams: Vec<u64>,
    replay_query: String,
    ptz_for: Option<u64>,
    ptz_seq: u32,
    telestrator_for: Option<u64>,
    tele_shape: u8,
    tele_color: [u8; 3],
    tele_size: f32,
    tele_points: Vec<(f32, f32)>,
    designer_for: Option<u64>,
    scope_open: bool,
    scope_mode: u8,
    scope_tex: Option<egui::TextureHandle>,
    scope_at: Instant,
    safe_area: u8,
    windows_on: [bool; ROUTES],
    external_route: Route,
    want_fullscreen: [bool; ROUTES],
    screens: Vec<(i32, i32, i32, i32)>,
    confirm_new: bool,
    confirm_open: Option<String>,
    /// Input to start again once its settings stop moving, and whether it
    /// carries on from where it stood.
    pending_restart: Option<(u64, bool, Instant)>,
    devices_at: Instant,
    basic: bool,
    audio_open: bool,
    tab: Option<usize>,
    search: String,
    current_file: Option<String>,
    imported: Option<String>,
    import_report: Option<crate::vmix_import::Report>,
    last_autosave: Instant,
    format_draft: (usize, usize, u32),
    devices: Arc<Mutex<(Vec<String>, Vec<String>)>>,
    encoders: Arc<Mutex<Vec<String>>>,
    message: Option<(String, Instant)>,
    shot_path: Option<String>,
    frames: u32,
    shot_sent: bool,
    shot_not_before: Instant,
    shot_done: Arc<AtomicBool>,
}

impl App {
    /// `open` starts with a window up, for checking how they look.
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        shot_path: Option<String>,
        open: Option<String>,
        project_file: Option<String>,
        lang: Option<Lang>,
    ) -> Self {
        let open = open.as_deref().unwrap_or("");
        apply_style(&cc.egui_ctx);
        // The check picture opens the worked example, as the reference picture
        // shows a project already open. A normal first start is an empty desk.
        let mut imported = None;
        let mut import_report = None;
        let file = project_file.as_deref();
        let own =
            file.filter(|f| !f.to_lowercase().ends_with(".vmix")).and_then(|f| bars::read_project(f).ok());
        let opened = file.filter(|_| own.is_none()).and_then(|f| crate::vmix_import::load(f).ok());
        let saved = if let Some(p) = own {
            Saved::fresh(p)
        } else if let Some((p, r)) = opened {
            imported = Some(r.name.clone());
            import_report = Some(r);
            Saved::fresh(p)
        } else if shot_path.is_some() {
            Saved::fresh(if open == "vide" { Project::blank() } else { Project::sample() })
        } else {
            Saved::load()
        };
        let mut saved = saved;
        if let Some(lang) = lang {
            saved.language = lang.code().into();
        }
        let remote = shot_path.is_none();
        Self::assemble(cc, saved, shot_path, open, imported, import_report, remote)
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble(
        cc: &eframe::CreationContext<'_>,
        saved: Saved,
        shot_path: Option<String>,
        open: &str,
        imported: Option<String>,
        import_report: Option<crate::vmix_import::Report>,
        remote: bool,
    ) -> Self {
        set_language(Lang::from_code(&saved.language).unwrap_or_else(Lang::system));
        let engine = Engine::start(saved.project.clone());
        engine.set_repaint(cc.egui_ctx.clone());
        let mut message = None;
        if remote {
            let cfg = ApiConfig { lan: saved.remote_lan, password: saved.remote_password.clone() };
            message = engine.start_api(cfg).map(|e| (e, Instant::now()));
        }

        let devices = Arc::new(Mutex::new((Vec::new(), Vec::new())));
        let encoders = Arc::new(Mutex::new(vec!["libx264".to_string()]));
        let (d, e) = (devices.clone(), encoders.clone());
        std::thread::spawn(move || {
            *d.lock().unwrap() = sources::list_devices();
            *e.lock().unwrap() = output::list_encoders();
        });

        // Real media needs a moment to open before a check picture means much.
        let shot_delay = if imported.is_some() { 10 } else { 0 };
        let p = &saved.project;
        let format_draft = (p.out_w, p.out_h, p.fps);
        let edit_tab =
            ["general", "couleur", "cle", "correction", "effets", "position", "couches", "declencheurs"]
                .iter()
                .position(|t| *t == open)
                .unwrap_or(0);
        let editing =
            (open.starts_with('c') && open != "clocks" || edit_tab > 0 || open == "general").then_some(9);
        Self {
            engine,
            saved,
            program_tex: None,
            preview_tex: None,
            route_tex: [None, None, None],
            program_seen: 0,
            preview_seen: 0,
            route_seen: 0,
            thumbs: HashMap::new(),
            editing,
            edit_tab,
            selected_layer: 0,
            picking_colour: false,
            settings_open: open == "reglages" || open == "options",
            settings_tab: usize::from(open == "options"),
            playlist_open: open == "playlist",
            overlays_open: open == "incrustations",
            overlay_pick: 0,
            categories_open: false,
            clocks_open: open == "clocks",
            stats_open: open == "stats",
            help_open: false,
            multi_open: false,
            multi_ids: Vec::new(),
            multi_audio: AudioPick::MASTER,
            replay_open: open == "ralenti",
            replay_cams: Vec::new(),
            replay_query: String::new(),
            ptz_for: None,
            ptz_seq: 0,
            telestrator_for: None,
            tele_shape: 0,
            tele_color: [255, 32, 32],
            tele_size: 0.012,
            tele_points: Vec::new(),
            designer_for: None,
            scope_open: open == "scope",
            scope_mode: 0,
            scope_tex: None,
            scope_at: Instant::now(),
            safe_area: 0,
            windows_on: [false; ROUTES],
            external_route: Route::Output,
            want_fullscreen: [true, true, false],
            screens: tools::list_screens(),
            confirm_new: false,
            confirm_open: None,
            pending_restart: None,
            devices_at: Instant::now(),
            basic: false,
            audio_open: true,
            tab: None,
            search: String::new(),
            current_file: None,
            imported,
            import_report,
            last_autosave: Instant::now(),
            format_draft,
            devices,
            encoders,
            message,
            shot_path,
            frames: 0,
            shot_sent: false,
            shot_not_before: Instant::now() + Duration::from_secs(shot_delay),
            shot_done: Arc::new(AtomicBool::new(false)),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.engine.state.lock().unwrap())
    }

    /// Takes effect on the next frame: every text is looked up as it is drawn.
    fn set_language(&mut self, lang: Lang) {
        self.saved.language = lang.code().into();
        set_language(lang);
    }

    fn say(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), Instant::now()));
    }

    fn call(&mut self, name: &str, a: &Args) {
        if let Err(e) = self.engine.call(name, a) {
            self.say(e);
        }
    }

    /// Dragging a slider or typing changes a source many times a second;
    /// starting it again each time would freeze it. It waits for a pause.
    fn queue_restart(&mut self, id: u64, keep: bool) {
        let mut keep = keep;
        if let Some((other, other_keep, _)) = self.pending_restart {
            if other == id {
                keep &= other_keep;
            } else {
                self.engine.restart_input(other, other_keep);
            }
        }
        self.pending_restart = Some((id, keep, Instant::now()));
    }

    fn run_pending_restart(&mut self) {
        if let Some((id, keep, at)) = self.pending_restart
            && at.elapsed() >= Duration::from_millis(250)
        {
            self.pending_restart = None;
            self.engine.restart_input(id, keep);
        }
    }

    /// Plugged in since the last look: cameras and screens come and go.
    fn refresh_devices(&mut self) {
        if self.devices_at.elapsed() < Duration::from_secs(5) {
            return;
        }
        self.devices_at = Instant::now();
        self.screens = tools::list_screens();
        let devices = self.devices.clone();
        std::thread::spawn(move || *devices.lock().unwrap() = sources::list_devices());
    }

    fn sync_textures(&mut self, ctx: &egui::Context) {
        let seq = self.engine.program_seq.load(Ordering::Acquire);
        if seq != self.program_seen {
            self.program_seen = seq;
            let f = self.engine.program.lock().unwrap().clone();
            upload(ctx, &mut self.program_tex, "programme", &f);
        }
        let seq = self.engine.preview_seq.load(Ordering::Acquire);
        if seq != self.preview_seen {
            self.preview_seen = seq;
            let f = self.engine.preview.lock().unwrap().clone();
            match f {
                Some(f) => upload(ctx, &mut self.preview_tex, "apercu", &f),
                None => self.preview_tex = None,
            }
        }
        let seq = self.engine.route_seq.load(Ordering::Acquire);
        if seq != self.route_seen {
            self.route_seen = seq;
            let frames = self.engine.route_frames.lock().unwrap().clone();
            for (n, f) in frames.iter().enumerate() {
                if let Some(f) = f {
                    upload(ctx, &mut self.route_tex[n], &format!("sortie{n}"), f);
                }
            }
        }
    }

    /// The card picture and its shape, refreshed when the engine made a new one.
    fn thumb(&mut self, ctx: &egui::Context, id: u64) -> Option<(egui::TextureId, f32)> {
        let latest = self.engine.thumbs.lock().unwrap().get(&id).cloned();
        if let Some((stamp, f)) = latest {
            let stale = self.thumbs.get(&id).is_none_or(|(_, s, _)| *s != stamp);
            if stale {
                let img = egui::ColorImage::from_rgba_premultiplied([f.w, f.h], &f.data);
                let aspect = f.w as f32 / f.h.max(1) as f32;
                match self.thumbs.get_mut(&id) {
                    Some((tex, s, a)) => {
                        tex.set(img, egui::TextureOptions::LINEAR);
                        *s = stamp;
                        *a = aspect;
                    }
                    None => {
                        let opts = egui::TextureOptions::LINEAR;
                        let tex = ctx.load_texture(format!("vignette{id}"), img, opts);
                        self.thumbs.insert(id, (tex, stamp, aspect));
                    }
                }
            }
        }
        self.thumbs.get(&id).map(|(t, _, a)| (t.id(), *a))
    }

    fn keyboard(&mut self, ctx: &egui::Context, project: &Project) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        // Shortcuts set by the person come first.
        let mut to_run: Vec<Shortcut> = Vec::new();
        ctx.input(|i| {
            for s in &project.shortcuts {
                if egui::Key::from_name(&s.key).is_some_and(|k| i.key_pressed(k)) {
                    to_run.push(s.clone());
                }
            }
        });
        let custom = !to_run.is_empty();
        for s in to_run {
            let a = Args::new().input(&s.input).value(&s.value);
            self.call(&s.function, &a);
        }
        if custom {
            return;
        }
        const DIGITS: [egui::Key; 9] = [
            egui::Key::Num1,
            egui::Key::Num2,
            egui::Key::Num3,
            egui::Key::Num4,
            egui::Key::Num5,
            egui::Key::Num6,
            egui::Key::Num7,
            egui::Key::Num8,
            egui::Key::Num9,
        ];
        let (digit, shift, space, enter) = ctx.input(|i| {
            let d = DIGITS.iter().position(|k| i.key_pressed(*k));
            (d, i.modifiers.shift, i.key_pressed(egui::Key::Space), i.key_pressed(egui::Key::Enter))
        });
        if let Some(input) = digit.and_then(|n| project.inputs.get(n)) {
            let id = input.id;
            self.with(|s| if shift { s.quick_play(id) } else { s.set_preview(id) });
        }
        if space {
            self.with(State::cut);
        }
        if enter {
            self.with(State::quick_play_preview);
        }
    }

    /// Files dropped on the window become inputs.
    fn dropped_files(&mut self, ctx: &egui::Context) {
        let files: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        });
        for p in files {
            self.add_file(&p);
        }
    }

    /// Keeps the session safe from a crash without touching the live threads.
    fn autosave(&mut self, project: &Project) {
        if self.shot_path.is_some() || self.last_autosave.elapsed() < Duration::from_secs(30) {
            return;
        }
        self.last_autosave = Instant::now();
        self.saved.project = project.clone();
        let snapshot = self.saved.clone();
        std::thread::spawn(move || snapshot.store());
    }

    fn handle_screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = self.shot_path.clone() else { return };
        self.frames += 1;
        ctx.request_repaint();
        if self.frames >= SHOT_AFTER_FRAMES && !self.shot_sent && Instant::now() >= self.shot_not_before {
            self.shot_sent = true;
            let done = self.shot_done.clone();
            let waker = ctx.clone();
            ctx.request_screenshot(move |img| {
                if let Some(dir) = std::path::Path::new(&path).parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
                let (w, h) = (img.size[0] as u32, img.size[1] as u32);
                if let Err(e) = image::save_buffer(&path, &rgba, w, h, image::ColorType::Rgba8) {
                    eprintln!("{}", t!("app.capture_failed", error = e));
                }
                done.store(true, Ordering::Relaxed);
                waker.request_repaint();
            });
        }
        if self.shot_done.load(Ordering::Relaxed) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn card_height(&self) -> f32 {
        if self.basic { BASIC_CARD_H } else { CARD_H }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.sync_textures(&ctx);
        let project = self.with(|s| s.project.clone());
        self.keyboard(&ctx, &project);
        self.dropped_files(&ctx);

        let panel = |fill| egui::Frame::new().fill(fill).inner_margin(egui::Margin::symmetric(6, 3));
        let top_fill = Color32::from_rgb(12, 12, 13);
        egui::Panel::top("haut").exact_size(30.0).frame(panel(top_fill)).show(ui, |ui| {
            self.top_bar(ui);
        });
        egui::Panel::bottom("etat").exact_size(18.0).frame(panel(Color32::BLACK)).show(ui, |ui| {
            self.status_line(ui);
        });
        egui::Panel::bottom("sorties").exact_size(30.0).frame(panel(PANEL)).show(ui, |ui| {
            self.output_bar(ui);
        });

        // Two rows of inputs are kept on a tall window; the monitors get the rest.
        let width = ui.available_width();
        let aspect = project.out_w as f32 / project.out_h.max(1) as f32;
        let tall = ui.available_height() >= 900.0;
        let rows = if tall { 2.0 } else { 1.0 };
        let lower_min = TABS_H + rows * (self.card_height() + 5.0) + 12.0;
        let room = ((width - MID_W) / 2.0 - 24.0) / aspect + HEADER_H + 34.0;
        let monitors_h = room.min(ui.available_height() - lower_min).max(200.0);
        let lower_h = ui.available_height() - monitors_h;
        let show_audio = self.audio_open && !self.basic;

        egui::Panel::bottom("sources").exact_size(lower_h).frame(panel(BG)).show(ui, |ui| {
            let full = ui.available_rect_before_wrap();
            let tabs = Rect::from_min_size(full.min, vec2(full.width(), TABS_H));
            let mut tabs_ui = ui.new_child(egui::UiBuilder::new().id_salt("onglets").max_rect(tabs));
            self.tabs_row(&mut tabs_ui, &project);
            let below = Rect::from_min_max(pos2(full.left(), tabs.bottom() + 3.0), full.max);
            let split = if show_audio { below.right() - AUDIO_W } else { below.right() + 10.0 };
            let cards = Rect::from_min_max(below.min, pos2(split - 10.0, below.bottom()));
            let mut left = ui.new_child(egui::UiBuilder::new().id_salt("cartes").max_rect(cards));
            self.inputs_panel(&mut left, &project);
            if show_audio {
                let mixer = Rect::from_min_max(pos2(split, below.top()), below.max);
                let mut right = ui.new_child(egui::UiBuilder::new().id_salt("mixeur").max_rect(mixer));
                self.audio_panel(&mut right, &project);
            }
            ui.advance_cursor_after_rect(full);
        });
        egui::CentralPanel::default().frame(panel(BG)).show(ui, |ui| {
            self.monitors(ui, &project);
        });

        self.input_settings_window(&ctx, &project);
        self.settings_window(&ctx, &project);
        self.playlist_window(&ctx, &project);
        self.overlays_window(&ctx, &project);
        self.categories_window(&ctx, &project);
        self.clocks_window(&ctx, &project);
        self.stats_window(&ctx, &project);
        self.help_window(&ctx);
        self.import_window(&ctx);
        self.multi_window(&ctx, &project);
        self.replay_window(&ctx, &project);
        self.ptz_window(&ctx, &project);
        self.telestrator_window(&ctx, &project);
        self.designer_window(&ctx, &project);
        self.scope_window(&ctx);
        self.confirm_window(&ctx);
        self.screen_windows(&ctx, &project);
        self.run_pending_restart();
        self.autosave(&project);
        self.handle_screenshot(&ctx);
        // Clock and meters tick over even when the picture is still.
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.shot_path.is_none() {
            self.saved.project = self.with(|s| s.project.clone());
            self.saved.store();
        }
        self.engine.shutdown();
    }
}
