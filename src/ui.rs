//! The window: preview left, output right, transitions between them, coloured
//! tabs and input cards below, audio mixer bottom right. It only reads shared
//! state and sends short commands, and it repaints when the engine has
//! something new instead of running flat out.

mod audio_panel;
mod bars;
mod input_settings;
mod inputs;
mod plugin_chain;
mod settings;
mod switcher;
#[cfg(test)]
mod tests;
mod tools;
mod updates;
mod virtual_set;
mod windows;

use crate::api::ApiConfig;
use crate::engine::{Compositor, Engine, ROUTES, State};
use crate::functions::Args;
use crate::i18n::{Lang, language, set_language};
use crate::instance;
use crate::model::*;
use crate::netvideo::Net;
use crate::output;
use crate::secret;
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
    updates: crate::update::Updates,
}

fn user_dir(base: PathBuf) -> PathBuf {
    base.join("LiveCraft")
}

/// Copies the session file to `session.prev.json` beside it, replacing the
/// previous copy. True when there was a session to keep.
fn back_up_session(session: &std::path::Path) -> bool {
    session.is_file() && std::fs::copy(session, session.with_file_name("session.prev.json")).is_ok()
}

fn saved_path() -> PathBuf {
    crate::platform::config_dir().join("LiveCraft").join("session.json")
}

/// Where the session lived when the program was called Regie.
fn older_saved_path() -> PathBuf {
    crate::platform::config_dir().join("Regie").join("session.json")
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
            match &mut i.kind {
                Kind::Network { url, .. } if sensitive(url) => *url = f(url),
                Kind::VideoCall { token, turn_password, .. } => {
                    *token = f(token);
                    *turn_password = f(turn_password);
                }
                _ => {}
            }
        }
    }

    fn fresh(project: Project) -> Self {
        Self {
            project,
            record_dir: user_dir(crate::platform::videos_dir()).to_string_lossy().into_owned(),
            last_file: None,
            remote_lan: false,
            remote_password: String::new(),
            language: String::new(),
            updates: crate::update::Updates::default(),
        }
    }

    fn load() -> Self {
        Self::load_from(&session_to_read(saved_path(), older_saved_path()))
    }

    fn load_from(path: &std::path::Path) -> Self {
        let Ok(bytes) = std::fs::read(path) else { return Self::default() };
        match serde_json::from_slice::<Self>(&bytes) {
            Ok(mut saved) => {
                saved.each_secret(secret::open);
                saved
            }
            Err(_) => {
                // Set aside, not overwritten: the empty desk that starts now
                // must not erase a session someone could still recover.
                let _ = std::fs::rename(path, path.with_extension("illisible.json"));
                Self::default()
            }
        }
    }

    fn store(&self, path: &std::path::Path) {
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
    /// The bus or Master whose plug-ins are shown in their own window.
    plugins_for: Option<crate::plugins::Owner>,
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
    /// NDI and OMT sources from the last look, by protocol; none until one ends.
    net_found: Arc<Mutex<[Option<Result<Vec<String>, String>>; 2]>>,
    net_looking: Arc<AtomicBool>,
    net_at: Option<Instant>,
    message: Option<(String, Instant)>,
    shot_path: Option<String>,
    frames: u32,
    shot_sent: bool,
    shot_not_before: Instant,
    shot_done: Arc<AtomicBool>,
    /// Whether the running remote control reaches the network, which can
    /// differ from the setting until the next start.
    api_lan: bool,
    /// Where the desk is kept between runs. None for interface tests and
    /// screenshots: they run the real window and must never write over the
    /// person's own session.
    session_file: Option<PathBuf>,
    /// When to look for a newer version; only a real run ever does.
    update_at: Option<Instant>,
    /// What the check running away from the window answered.
    update_answer: Arc<Mutex<Option<Result<Option<crate::update::Release>, String>>>>,
    update_checking: updates::Checking,
    /// The person's browser; the tests look at what it was asked instead.
    open_in_browser: fn(&str),
    /// Asks for the newest release; the tests never reach the network.
    update_fetch: fn(&str) -> Result<Option<crate::update::Release>, String>,
    /// Whether ffmpeg is asked what it can do: a real run, or a look at the About tab.
    asks_ffmpeg: bool,
    /// Requests from later starts, while this is the first window open.
    first: Option<instance::First>,
    /// While another window holds the session.
    second: Option<SessionHeld>,
    /// This window does not save the session: another one holds it, or what
    /// that one saved last could not be set aside.
    not_saving: bool,
}

/// A window that found the session held by another one.
struct SessionHeld {
    second: instance::Second,
    /// Where this window will keep the session once the other one is gone.
    session: Option<PathBuf>,
    /// The desk as this window started, to tell whether it was worked on.
    desk_at_start: Project,
}

/// The system's file dialog. While one is up the window cannot take anything
/// else: later starts are told to wait, not taken for a frozen window.
struct FileDialog(rfd::FileDialog);

impl FileDialog {
    fn new() -> Self {
        Self(rfd::FileDialog::new())
    }

    fn add_filter(self, name: impl Into<String>, extensions: &[impl ToString]) -> Self {
        Self(self.0.add_filter(name, extensions))
    }

    fn set_directory(self, path: impl AsRef<std::path::Path>) -> Self {
        Self(self.0.set_directory(path))
    }

    fn set_file_name(self, name: impl Into<String>) -> Self {
        Self(self.0.set_file_name(name))
    }

    fn pick_file(self) -> Option<PathBuf> {
        instance::while_busy(|| self.0.pick_file())
    }

    fn pick_files(self) -> Option<Vec<PathBuf>> {
        instance::while_busy(|| self.0.pick_files())
    }

    fn pick_folder(self) -> Option<PathBuf> {
        instance::while_busy(|| self.0.pick_folder())
    }

    fn save_file(self) -> Option<PathBuf> {
        instance::while_busy(|| self.0.save_file())
    }
}

impl App {
    /// `open` starts with a window up, for checking how they look.
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        shot_path: Option<String>,
        open: Option<String>,
        project_file: Option<String>,
        lang: Option<Lang>,
        role: instance::Role,
    ) -> Self {
        let open = open.as_deref().unwrap_or("");
        apply_style(&cc.egui_ctx);
        // The check picture opens the worked example, as the reference picture
        // shows a project already open. A normal first start is an empty desk.
        let saved = if shot_path.is_some() {
            Saved::fresh(if open == "vide" { Project::blank() } else { Project::sample() })
        } else {
            Saved::load()
        };
        let mut saved = saved;
        if let Some(lang) = lang {
            saved.language = lang.code().into();
        }
        if shot_path.is_some() && open == "apropos" {
            updates::as_if_found(&mut saved.updates);
        }
        let remote = shot_path.is_none();
        let mut app = Self::start(cc, saved, shot_path, open, project_file, remote);
        app.take_role(&cc.egui_ctx, role);
        app
    }

    /// Where this window stands among the LiveCraft windows of this account.
    fn take_role(&mut self, ctx: &egui::Context, role: instance::Role) {
        match role {
            instance::Role::First(first) => self.become_first(ctx, first),
            instance::Role::Second(second) => {
                let desk_at_start = self.with(|s| s.project.clone());
                self.second = Some(SessionHeld { second, session: self.session_file.take(), desk_at_start });
                self.not_saving = true;
                self.say(t!("app.second_window"));
            }
            instance::Role::Alone { foreign: true } => self.say(t!("app.channel_foreign")),
            instance::Role::Alone { foreign: false } => {}
        }
    }

    fn become_first(&mut self, ctx: &egui::Context, first: instance::First) {
        let ctx = ctx.clone();
        first.wake_with(move || ctx.request_repaint());
        self.first = Some(first);
    }

    /// What later starts handed over: each brings this window to the front,
    /// and a project opens as one dropped on the window would.
    fn take_handed_over(&mut self, ctx: &egui::Context) {
        if let Some(held) = &mut self.second
            && let Some(first) = held.second.try_first()
        {
            first.set_ready();
            self.become_first(ctx, first);
            if let Some(held) = self.second.take() {
                self.take_session_back(held);
            }
        }
        let handed: Vec<_> =
            self.first.as_ref().map(|f| std::iter::from_fn(|| f.next()).collect()).unwrap_or_default();
        for request in handed {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            if let instance::Request::Open(path) = request {
                self.open_from_outside(path);
            }
        }
    }

    /// The other window is gone, and saved the session as it closed: that save
    /// is never lost. A desk left as it started gives way to it; a desk worked
    /// on stays, the other one's save first copied aside.
    fn take_session_back(&mut self, held: SessionHeld) {
        let Some(path) = held.session else {
            self.not_saving = false;
            self.say(t!("app.first_again"));
            return;
        };
        if !path.is_file() {
            // The other window never saved: nothing to lose.
            self.say(t!("app.first_again"));
        } else if self.with(|s| s.project == held.desk_at_start) {
            let saved = Saved::load_from(&path);
            self.engine.load_project(saved.project.clone());
            self.after_load(None, None);
            self.saved = saved;
            self.say(t!("app.session_taken_back"));
        } else {
            let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
            let aside = path.with_file_name(format!("session.{stamp}.json"));
            if std::fs::copy(&path, &aside).is_err() {
                self.say(t!("app.session_not_set_aside"));
                return;
            }
            let shown = aside.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            self.say(t!("app.session_set_aside", file = shown));
        }
        self.session_file = Some(path);
        self.not_saving = false;
    }

    /// A project named on the command line, as a double-clicked file is,
    /// replaces the session's. A .vmix comes in as an unnamed copy; a saved
    /// project comes in as itself, so that saving goes back to that file.
    /// The session keeps its other settings either way.
    fn start(
        cc: &eframe::CreationContext<'_>,
        mut saved: Saved,
        shot_path: Option<String>,
        open: &str,
        project_file: Option<String>,
        remote: bool,
    ) -> Self {
        let (mut imported, mut import_report, mut current_file, mut failed) = (None, None, None, None);
        let mut backed_up = false;
        if let Some(path) = project_file {
            let read = if path.to_lowercase().ends_with(".vmix") {
                crate::vmix_import::load(&path).map(|(p, r)| {
                    imported = Some(r.name.clone());
                    import_report = Some(r);
                    p
                })
            } else {
                bars::read_project(&path).inspect(|_| current_file = Some(path.clone()))
            };
            match read {
                Ok(p) => {
                    // The session is saved on its own as the desk changes, so what
                    // was on the desk would be gone for good: keep one copy.
                    if remote && back_up_session(&saved_path()) {
                        backed_up = true;
                    }
                    saved.project = p;
                    saved.last_file = Some(path);
                }
                Err(e) => failed = Some(t!("bars.unreadable_project", error = e)),
            }
        }
        let mut app = Self::assemble(cc, saved, shot_path, open, imported, import_report, remote);
        app.current_file = current_file;
        if let Some(e) = failed {
            app.say(e);
        } else if backed_up {
            app.say(t!("bars.session_backed_up"));
        }
        app
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
        let api_lan = remote && saved.remote_lan;
        if remote {
            let cfg = ApiConfig { lan: saved.remote_lan, password: saved.remote_password.clone() };
            message = engine.start_api(cfg).map(|e| (e, Instant::now()));
        }

        let devices = Arc::new(Mutex::new((Vec::new(), Vec::new())));
        let encoders = Arc::new(Mutex::new(vec!["libx264".to_string()]));
        let asks_ffmpeg = remote || open == "apropos";
        let (d, e) = (devices.clone(), encoders.clone());
        std::thread::spawn(move || {
            *d.lock().unwrap() = crate::platform::list_devices();
            if asks_ffmpeg {
                *e.lock().unwrap() = output::list_encoders();
            }
        });

        // Real media needs a moment to open before a check picture means much.
        let shot_delay = if imported.is_some() {
            10
        } else if open == "apropos" {
            12
        } else {
            0
        };
        let p = &saved.project;
        let format_draft = (p.out_w, p.out_h, p.fps);
        let edit_tab = [
            "general",
            "couleur",
            "cle",
            "correction",
            "effets",
            "position",
            "couches",
            "declencheurs",
            "audio",
        ]
        .iter()
        .position(|t| *t == open)
        .unwrap_or(0);
        // The sound tab needs an input with sound: the example's test tone.
        let shown = if open == "audio" { 16 } else { 9 };
        let editing =
            (open.starts_with('c') && open != "clocks" || edit_tab > 0 || open == "general").then_some(shown);
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
            settings_open: open == "reglages"
                || open == "options"
                || open == "sorties"
                || open == "enregistrement"
                || open == "apropos",
            settings_tab: match open {
                "options" => 1,
                "sorties" => 2,
                "enregistrement" => 3,
                "apropos" => 10,
                _ => 0,
            },
            playlist_open: open == "playlist",
            overlays_open: open == "incrustations",
            plugins_for: None,
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
            screens: crate::platform::list_screens(),
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
            net_found: Arc::new(Mutex::new([None, None])),
            net_looking: Arc::new(AtomicBool::new(false)),
            net_at: None,
            message,
            shot_path,
            frames: 0,
            shot_sent: false,
            shot_not_before: Instant::now() + Duration::from_secs(shot_delay),
            shot_done: Arc::new(AtomicBool::new(false)),
            api_lan,
            session_file: remote.then(saved_path),
            update_at: remote.then(|| Instant::now() + updates::CHECK_AFTER),
            update_answer: Arc::new(Mutex::new(None)),
            update_checking: updates::Checking::Idle,
            open_in_browser: if remote { crate::platform::open_in_browser } else { |_| {} },
            update_fetch: if remote { crate::update::fetch } else { |_| Ok(None) },
            asks_ffmpeg,
            first: None,
            second: None,
            not_saving: false,
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
        self.screens = crate::platform::list_screens();
        let devices = self.devices.clone();
        std::thread::spawn(move || *devices.lock().unwrap() = crate::platform::list_devices());
    }

    /// Network sources come and go: while a list of them is on screen, it is
    /// asked for again every second, away from the window's thread. The first
    /// look also loads the runtimes, which takes a moment.
    fn look_for_net_sources(&mut self) {
        if self.net_at.is_some_and(|t| t.elapsed() < Duration::from_secs(1))
            || self.net_looking.swap(true, Ordering::AcqRel)
        {
            return;
        }
        self.net_at = Some(Instant::now());
        let (found, looking) = (self.net_found.clone(), self.net_looking.clone());
        std::thread::spawn(move || {
            for net in Net::ALL {
                let list = net.sources();
                found.lock().unwrap()[net as usize] = Some(list);
            }
            looking.store(false, Ordering::Release);
        });
    }

    fn net_found(&self, net: Net) -> Option<Result<Vec<String>, String>> {
        self.net_found.lock().unwrap()[net as usize].clone()
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
        let Some(path) = self.session_file.clone() else { return };
        if self.last_autosave.elapsed() < Duration::from_secs(30) {
            return;
        }
        self.last_autosave = Instant::now();
        self.saved.project = project.clone();
        let snapshot = self.saved.clone();
        std::thread::spawn(move || snapshot.store(&path));
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
    /// Runs while the window is minimised too, so a later start finds it.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(first) = &self.first {
            first.set_ready();
        }
        self.take_handed_over(ctx);
    }

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
        self.bus_plugins_window(&ctx, &project);
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
        self.follow_update_check();
        self.autosave(&project);
        if secret::take_dropped() {
            self.say(t!("secret.not_saved"));
        }
        self.handle_screenshot(&ctx);
        // Clock and meters tick over even when the picture is still.
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(path) = self.session_file.clone() {
            self.saved.project = self.with(|s| s.project.clone());
            self.saved.store(&path);
        }
        // Outputs can take a while to close: a later start meanwhile finds the
        // name free and the session saved, not a window that will not answer.
        self.first = None;
        self.engine.shutdown();
    }
}
