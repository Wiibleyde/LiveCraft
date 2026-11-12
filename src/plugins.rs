//! Third-party sound plug-ins (CLAP) in the chain of every input, bus and
//! Master: finding them, running them on the sound thread, keeping their
//! settings in the project, and opening their own windows.
//!
//! Two threads share the work, as CLAP asks. The plug-in thread starts and
//! stops instances, saves their state and drives their windows. The sound
//! thread only runs blocks through them; it never waits on a lock, and
//! takes no memory once a chain is set up.

use crate::audio::FRAMES;
use crate::clap::{self, Library, Param, Plugin, PluginInfo, Processor, Requests};
use crate::engine::Engine;
use crate::model::{PluginSlot, Project};
use crate::sources::AUDIO_RATE;
use base64::Engine as _;
use base64::prelude::BASE64_STANDARD;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Whose chain: an input, or a bus, 0 being Master and 1 to 7 buses A to G.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Owner {
    Input(u64),
    Bus(u8),
}

pub fn chain(p: &Project, owner: Owner) -> Option<&Vec<PluginSlot>> {
    match owner {
        Owner::Input(id) => p.inputs.iter().find(|i| i.id == id).map(|i| &i.audio.plugins),
        Owner::Bus(0) => Some(&p.master_plugins),
        Owner::Bus(b) => p.buses.get(b as usize - 1).map(|b| &b.plugins),
    }
}

pub fn chain_mut(p: &mut Project, owner: Owner) -> Option<&mut Vec<PluginSlot>> {
    match owner {
        Owner::Input(id) => p.inputs.iter_mut().find(|i| i.id == id).map(|i| &mut i.audio.plugins),
        Owner::Bus(0) => Some(&mut p.master_plugins),
        Owner::Bus(b) => p.buses.get_mut(b as usize - 1).map(|b| &mut b.plugins),
    }
}

/// A plug-in both threads hold.
struct Live {
    plugin: Plugin,
    bypass: AtomicBool,
    /// Parameter changes from our own window, waiting for the next block.
    pending: Mutex<Vec<(u32, f64)>>,
    requests: Arc<Requests>,
}

#[derive(Default)]
struct Published {
    version: u64,
    chains: HashMap<Owner, Vec<Arc<Live>>>,
}

/// Plug-ins found on this computer.
#[derive(Clone, Default)]
pub struct Catalog {
    pub scanned: bool,
    pub scanning: bool,
    pub found: Vec<PluginInfo>,
    /// Files that could not be read, and why.
    pub problems: Vec<String>,
}

pub enum Command {
    SetParam { owner: Owner, key: u64, id: u32, value: f64 },
    ToggleEditor { owner: Owner, key: u64 },
}

/// What the window shows of one slot.
#[derive(Clone, Default, Debug)]
pub struct SlotView {
    pub running: bool,
    pub error: Option<String>,
    /// Samples of delay the plug-in adds.
    pub latency: u32,
    pub params: Vec<Param>,
    pub has_editor: bool,
    pub editor_open: bool,
}

#[derive(Default)]
pub struct PluginHost {
    published: Mutex<Published>,
    /// The last version the sound thread took up.
    seen: AtomicU64,
    audio_done: AtomicBool,
    catalog: Arc<Mutex<Catalog>>,
    commands: Mutex<Vec<Command>>,
    views: Mutex<HashMap<(Owner, u64), SlotView>>,
}

impl PluginHost {
    pub fn view(&self, owner: Owner, key: u64) -> Option<SlotView> {
        self.views.lock().unwrap().get(&(owner, key)).cloned()
    }

    pub fn send(&self, c: Command) {
        self.commands.lock().unwrap().push(c);
    }

    pub fn catalog(&self) -> Catalog {
        self.catalog.lock().unwrap().clone()
    }

    #[cfg(test)]
    pub fn set_catalog(&self, found: Vec<PluginInfo>) {
        *self.catalog.lock().unwrap() = Catalog { scanned: true, found, ..Catalog::default() };
    }

    /// Looks through the usual plug-in folders, beside the show.
    pub fn scan(&self) {
        let catalog = self.catalog.clone();
        {
            let mut c = catalog.lock().unwrap();
            if c.scanning {
                return;
            }
            c.scanning = true;
        }
        thread::spawn(move || {
            let mut files = Vec::new();
            for dir in plugin_folders() {
                find_plugin_files(&dir, 0, &mut files);
            }
            files.sort();
            files.dedup();
            let (found, problems) = describe_all(&files);
            *catalog.lock().unwrap() = Catalog { scanned: true, scanning: false, found, problems };
        });
    }

    /// Adds the plug-ins of a file picked by hand.
    pub fn scan_file(&self, path: PathBuf) {
        let catalog = self.catalog.clone();
        thread::spawn(move || {
            let (found, problems) = describe_all(&[path]);
            let mut c = catalog.lock().unwrap();
            for info in found {
                if !c.found.iter().any(|f| f.path == info.path && f.id == info.id) {
                    c.found.push(info);
                }
            }
            c.problems.extend(problems);
        });
    }
}

/// Where CLAP plug-ins install themselves, as the CLAP specification lists,
/// plus any folder named in `CLAP_PATH`.
pub fn plugin_folders() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(list) = std::env::var_os("CLAP_PATH") {
        dirs.extend(std::env::split_paths(&list));
    }
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    if cfg!(windows) {
        dirs.extend(env("COMMONPROGRAMFILES").map(|d| d.join("CLAP")));
        dirs.extend(env("LOCALAPPDATA").map(|d| d.join("Programs").join("Common").join("CLAP")));
    } else if cfg!(target_os = "macos") {
        dirs.push(PathBuf::from("/Library/Audio/Plug-Ins/CLAP"));
        dirs.extend(env("HOME").map(|d| d.join("Library/Audio/Plug-Ins/CLAP")));
    } else {
        dirs.extend(env("HOME").map(|d| d.join(".clap")));
        dirs.push(PathBuf::from("/usr/lib/clap"));
    }
    dirs
}

fn find_plugin_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(list) = std::fs::read_dir(dir) else { return };
    for entry in list.flatten() {
        let path = entry.path();
        let is_clap = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("clap"));
        if is_clap {
            out.push(path);
        } else if depth < 6 && path.is_dir() {
            find_plugin_files(&path, depth + 1, out);
        }
    }
}

fn describe_all(files: &[PathBuf]) -> (Vec<PluginInfo>, Vec<String>) {
    let mut found = Vec::new();
    let mut problems = Vec::new();
    for file in files {
        match describe_apart(file) {
            Ok(list) => found.extend(list.into_iter().filter(|p| p.treats_sound())),
            Err(e) => problems.push(format!("{} : {e}", file.display())),
        }
    }
    found.sort_by_key(|p| p.name.to_lowercase());
    (found, problems)
}

/// Reads a plug-in file in a process of its own: a plug-in that crashes
/// while being looked at must not take the show with it.
fn describe_apart(file: &Path) -> Result<Vec<PluginInfo>, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = Process::new(exe);
    cmd.arg("--scan-clap").arg(file).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take().ok_or(t!("plugins.no_output"))?;
    let reader = thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut stdout, &mut text);
        text
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(t!("plugins.no_answer").into());
        }
        thread::sleep(Duration::from_millis(30));
    };
    let text = reader.join().unwrap_or_default();
    if !status.success() {
        return Err(t!("plugins.crashed_scan").into());
    }
    serde_json::from_str::<Result<Vec<PluginInfo>, String>>(&text).map_err(|e| e.to_string())?
}

/// The `--scan-clap` side of `describe_apart`.
pub fn scan_one_and_print(file: &str) -> i32 {
    let answer = clap::describe(Path::new(file));
    println!("{}", serde_json::to_string(&answer).unwrap_or_default());
    0
}

/// One slot as the plug-in thread needs it, without the saved state.
#[derive(Clone, Debug, PartialEq)]
pub struct Want {
    pub key: u64,
    pub path: String,
    pub id: String,
    pub name: String,
    pub bypass: bool,
}

pub fn wanted(p: &Project) -> Vec<(Owner, Vec<Want>)> {
    let want = |s: &PluginSlot| Want {
        key: s.key,
        path: s.path.clone(),
        id: s.id.clone(),
        name: s.name.clone(),
        bypass: s.bypass,
    };
    let mut out = Vec::new();
    for i in p.inputs.iter().filter(|i| i.kind.has_audio() && !i.audio.plugins.is_empty()) {
        out.push((Owner::Input(i.id), i.audio.plugins.iter().map(want).collect()));
    }
    if !p.master_plugins.is_empty() {
        out.push((Owner::Bus(0), p.master_plugins.iter().map(want).collect()));
    }
    for (n, b) in p.buses.iter().enumerate().filter(|(_, b)| !b.plugins.is_empty()) {
        out.push((Owner::Bus(n as u8 + 1), b.plugins.iter().map(want).collect()));
    }
    out
}

pub fn decode_state(text: &str) -> Option<Vec<u8>> {
    if text.is_empty() { None } else { BASE64_STANDARD.decode(text).ok() }
}

struct Instance {
    path: String,
    id: String,
    name: String,
    live: Arc<Live>,
    editor: Option<clap::editor::Editor>,
    message: Option<String>,
}

/// The plug-in thread's side: every instance, and the libraries they came from.
#[derive(Default)]
pub struct MainSide {
    libs: HashMap<String, Arc<Library>>,
    instances: HashMap<(Owner, u64), Instance>,
    order: HashMap<Owner, Vec<u64>>,
    /// Slots that would not start, left alone until their plug-in changes.
    failed: HashMap<(Owner, u64), (String, String, String)>,
    /// Removed, but perhaps still in the sound thread's hands.
    retiring: Vec<(u64, Arc<Live>)>,
    version: u64,
    saved_at: Option<Instant>,
    viewed_at: Option<Instant>,
}

impl MainSide {
    /// Starts, stops and reorders instances to match the project.
    pub fn sync(
        &mut self,
        host: &PluginHost,
        wanted: &[(Owner, Vec<Want>)],
        state_of: impl Fn(Owner, u64) -> Option<Vec<u8>>,
    ) {
        let find = |k: &(Owner, u64)| {
            wanted.iter().filter(|(o, _)| *o == k.0).flat_map(|(_, w)| w).find(|w| w.key == k.1)
        };
        let mut changed = false;
        let gone: Vec<(Owner, u64)> = self
            .instances
            .iter()
            .filter(|(k, i)| !find(k).is_some_and(|w| w.path == i.path && w.id == i.id))
            .map(|(k, _)| *k)
            .collect();
        for k in gone {
            self.retire(k);
            changed = true;
        }
        self.failed.retain(|k, f| find(k).is_some_and(|w| w.path == f.0 && w.id == f.1));
        host.views.lock().unwrap().retain(|k, _| find(k).is_some());
        for (owner, wants) in wanted {
            for w in wants {
                let k = (*owner, w.key);
                let failed_before = self.failed.get(&k).is_some_and(|f| f.0 == w.path && f.1 == w.id);
                if !self.instances.contains_key(&k) && !failed_before {
                    match self.start(w, state_of(*owner, w.key)) {
                        Ok(inst) => {
                            host.views.lock().unwrap().insert(k, view_of(&inst));
                            self.instances.insert(k, inst);
                        }
                        Err(e) => {
                            let view = SlotView { error: Some(e.clone()), ..SlotView::default() };
                            host.views.lock().unwrap().insert(k, view);
                            self.failed.insert(k, (w.path.clone(), w.id.clone(), e));
                        }
                    }
                }
                if let Some(i) = self.instances.get(&k) {
                    i.live.bypass.store(w.bypass, Ordering::Relaxed);
                }
            }
            let order: Vec<u64> = wants
                .iter()
                .filter(|w| self.instances.contains_key(&(*owner, w.key)))
                .map(|w| w.key)
                .collect();
            if self.order.get(owner) != Some(&order) {
                self.order.insert(*owner, order);
                changed = true;
            }
        }
        let before = self.order.len();
        self.order.retain(|o, _| wanted.iter().any(|(w, _)| w == o));
        if changed || self.order.len() != before {
            self.publish(host);
        }
        self.release(host);
    }

    fn library(&mut self, path: &str) -> Result<Arc<Library>, String> {
        if let Some(lib) = self.libs.get(path) {
            return Ok(lib.clone());
        }
        let lib = Library::open(Path::new(path))?;
        self.libs.insert(path.to_string(), lib.clone());
        Ok(lib)
    }

    fn start(&mut self, w: &Want, state: Option<Vec<u8>>) -> Result<Instance, String> {
        let lib = self.library(&w.path)?;
        let requests = Arc::new(Requests::default());
        let mut plugin = Plugin::create(lib, &w.id, requests.clone())?;
        // A state the plug-in no longer understands leaves it at its defaults.
        if let Some(bytes) = state {
            plugin.load_state(&bytes);
        }
        plugin.activate(AUDIO_RATE as f64, FRAMES as u32)?;
        let live =
            Live { plugin, bypass: AtomicBool::new(w.bypass), pending: Mutex::new(Vec::new()), requests };
        Ok(Instance {
            path: w.path.clone(),
            id: w.id.clone(),
            name: w.name.clone(),
            live: Arc::new(live),
            editor: None,
            message: None,
        })
    }

    fn retire(&mut self, k: (Owner, u64)) {
        if let Some(mut inst) = self.instances.remove(&k) {
            if let Some(ed) = inst.editor.take() {
                clap::editor::close(&inst.live.plugin, ed);
            }
            // Gone from the next version the sound thread takes up.
            self.retiring.push((self.version + 1, inst.live));
        }
    }

    fn publish(&mut self, host: &PluginHost) {
        self.version += 1;
        let chains = self
            .order
            .iter()
            .map(|(owner, keys)| {
                let list =
                    keys.iter().filter_map(|k| self.instances.get(&(*owner, *k))).map(|i| i.live.clone());
                (*owner, list.collect())
            })
            .collect();
        *host.published.lock().unwrap() = Published { version: self.version, chains };
    }

    /// Ends instances the sound thread has let go of: CLAP wants them ended
    /// on this thread.
    fn release(&mut self, host: &PluginHost) {
        let seen = host.seen.load(Ordering::Acquire);
        self.retiring.retain(|(v, _)| *v > seen);
    }

    /// Answers the window and the plug-ins, and copies changed states into
    /// the project through `store`.
    pub fn serve(&mut self, host: &PluginHost, mut store: impl FnMut(Owner, u64, String)) {
        let commands = std::mem::take(&mut *host.commands.lock().unwrap());
        let mut refresh = false;
        for c in commands {
            match c {
                Command::SetParam { owner, key, id, value } => {
                    if let Some(i) = self.instances.get(&(owner, key)) {
                        i.live.pending.lock().unwrap().push((id, value));
                        // Shown at once, before the plug-in confirms it.
                        if let Some(p) = host
                            .views
                            .lock()
                            .unwrap()
                            .get_mut(&(owner, key))
                            .and_then(|v| v.params.iter_mut().find(|p| p.id == id))
                        {
                            p.value = value;
                            p.text = i.live.plugin.value_text(id, value);
                        }
                    }
                }
                Command::ToggleEditor { owner, key } => {
                    if let Some(i) = self.instances.get_mut(&(owner, key)) {
                        match i.editor.take() {
                            Some(ed) => clap::editor::close(&i.live.plugin, ed),
                            None => match clap::editor::open(&i.live.plugin, &i.name) {
                                Ok(ed) => {
                                    i.editor = Some(ed);
                                    i.message = None;
                                }
                                Err(e) => i.message = Some(e),
                            },
                        }
                        refresh = true;
                    }
                }
            }
        }
        for hwnd in clap::editor::closed() {
            for i in self.instances.values_mut() {
                if i.editor.as_ref().is_some_and(|e| e.window == Some(hwnd))
                    && let Some(ed) = i.editor.take()
                {
                    clap::editor::close(&i.live.plugin, ed);
                    refresh = true;
                }
            }
        }
        let mut restart = Vec::new();
        for (k, i) in self.instances.iter_mut() {
            let r = &i.live.requests;
            if r.callback.swap(false, Ordering::Relaxed) {
                i.live.plugin.on_main_thread();
            }
            if r.editor_closed.swap(false, Ordering::Relaxed)
                && let Some(ed) = i.editor.take()
            {
                clap::editor::close(&i.live.plugin, ed);
                refresh = true;
            }
            let size = r.resize.swap(0, Ordering::Relaxed);
            if size != 0
                && let Some(ed) = &i.editor
            {
                clap::editor::resize(&i.live.plugin, ed, (size >> 32) as u32, size as u32);
            }
            refresh |= r.rescan.swap(false, Ordering::Relaxed);
            if r.restart.swap(false, Ordering::Relaxed) {
                restart.push(*k);
            }
        }
        let now = Instant::now();
        let save_due = self.saved_at.is_none_or(|t| now - t >= Duration::from_millis(250));
        if save_due || !restart.is_empty() {
            self.saved_at = Some(now);
            for (k, i) in &self.instances {
                let dirty = i.live.requests.dirty.swap(false, Ordering::Relaxed);
                if (dirty || restart.contains(k))
                    && let Some(bytes) = i.live.plugin.save_state()
                {
                    store(k.0, k.1, BASE64_STANDARD.encode(bytes));
                }
            }
        }
        // Started again on the next sync, from the state just saved.
        if !restart.is_empty() {
            for k in restart {
                self.retire(k);
            }
            self.publish(host);
        }
        let view_due = self.viewed_at.is_none_or(|t| now - t >= Duration::from_millis(200));
        if refresh || view_due {
            self.viewed_at = Some(now);
            let mut views = host.views.lock().unwrap();
            for (k, i) in &self.instances {
                views.insert(*k, view_of(i));
            }
        }
        self.release(host);
    }

    /// Lets go of everything once the sound thread has, or never if it
    /// does not answer: ending a plug-in it still runs would crash.
    pub fn shutdown(mut self, host: &PluginHost) {
        for i in self.instances.values_mut() {
            if let Some(ed) = i.editor.take() {
                clap::editor::close(&i.live.plugin, ed);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while !host.audio_done.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if !host.audio_done.load(Ordering::Acquire) {
            std::mem::forget(self);
        }
    }
}

fn view_of(i: &Instance) -> SlotView {
    let p = &i.live.plugin;
    SlotView {
        running: true,
        error: i.message.clone(),
        latency: p.latency(),
        params: p.params(),
        has_editor: p.has_editor(),
        editor_open: i.editor.is_some(),
    }
}

struct Running {
    // Before `live`: the processor must stop before the plug-in can go.
    processor: Processor,
    live: Arc<Live>,
    ran: bool,
}

/// The sound thread's side.
pub struct Rack<'a> {
    host: &'a PluginHost,
    seen: u64,
    chains: HashMap<Owner, Vec<Running>>,
    changes: Vec<(u32, f64)>,
}

impl<'a> Rack<'a> {
    pub fn new(host: &'a PluginHost) -> Self {
        Rack { host, seen: 0, chains: HashMap::new(), changes: Vec::with_capacity(256) }
    }

    /// Takes up a new chain list if there is one and nobody holds it. Called
    /// once per block.
    pub fn refresh(&mut self) {
        let Ok(published) = self.host.published.try_lock() else { return };
        if published.version == self.seen {
            return;
        }
        let mut old: Vec<Running> = self.chains.drain().flat_map(|(_, list)| list).collect();
        let mut chains = HashMap::new();
        for (owner, lives) in &published.chains {
            let mut list = Vec::with_capacity(lives.len());
            for live in lives {
                match old.iter().position(|r| Arc::ptr_eq(&r.live, live)) {
                    Some(n) => list.push(old.swap_remove(n)),
                    None => list.push(Running {
                        processor: live.plugin.processor(FRAMES),
                        live: live.clone(),
                        ran: false,
                    }),
                }
            }
            chains.insert(*owner, list);
        }
        let version = published.version;
        drop(published);
        // Stopped here, on this thread, before the plug-in thread may end them.
        drop(old);
        self.chains = chains;
        self.seen = version;
        self.host.seen.store(version, Ordering::Release);
    }

    /// Runs one block of interleaved stereo through a chain, in order.
    pub fn run(&mut self, owner: Owner, block: &mut [f32]) {
        let Some(list) = self.chains.get_mut(&owner) else { return };
        for r in list {
            if r.live.bypass.load(Ordering::Relaxed) {
                continue;
            }
            take_changes(&r.live, &mut self.changes);
            r.processor.run(block, &self.changes);
            if !self.changes.is_empty() {
                r.live.requests.dirty.store(true, Ordering::Relaxed);
            }
            r.ran = true;
        }
    }

    /// Plug-ins with no sound this block still take their parameter changes.
    pub fn idle(&mut self) {
        for r in self.chains.values_mut().flatten() {
            if !r.ran {
                take_changes(&r.live, &mut self.changes);
                let asked = r.live.requests.flush.swap(false, Ordering::Relaxed);
                if !self.changes.is_empty() || asked {
                    r.processor.flush(&self.changes);
                }
                if !self.changes.is_empty() {
                    r.live.requests.dirty.store(true, Ordering::Relaxed);
                }
            }
            r.ran = false;
        }
    }

    /// Stops every plug-in, for good.
    pub fn stop(&mut self) {
        self.chains.clear();
        self.host.audio_done.store(true, Ordering::Release);
    }
}

/// Never waits: if the window is adding a change right now, it comes with
/// the next block.
fn take_changes(live: &Live, into: &mut Vec<(u32, f64)>) {
    into.clear();
    if let Ok(mut q) = live.pending.try_lock() {
        into.extend(q.drain(..));
    }
}

impl Engine {
    pub(crate) fn plugin_loop(self: Arc<Self>) {
        clap::this_thread_is_main();
        let mut side = MainSide::default();
        let mut next_sync = Instant::now();
        while !self.quit.load(Ordering::Relaxed) {
            clap::editor::pump();
            if Instant::now() >= next_sync {
                let wanted = wanted(&self.state.lock().unwrap().project);
                side.sync(&self.plugins, &wanted, |owner, key| self.plugin_state(owner, key));
                next_sync = Instant::now() + Duration::from_millis(50);
            }
            side.serve(&self.plugins, |owner, key, state| self.store_plugin_state(owner, key, state));
            thread::sleep(Duration::from_millis(10));
        }
        side.shutdown(&self.plugins);
    }

    fn plugin_state(&self, owner: Owner, key: u64) -> Option<Vec<u8>> {
        let s = self.state.lock().unwrap();
        let slot = chain(&s.project, owner)?.iter().find(|p| p.key == key)?;
        decode_state(&slot.state)
    }

    /// Sound settings only: the picture has nothing to redo.
    fn store_plugin_state(&self, owner: Owner, key: u64, state: String) {
        let mut s = self.state.lock().unwrap();
        if let Some(slot) = chain_mut(&mut s.project, owner).and_then(|c| c.iter_mut().find(|p| p.key == key))
            && slot.state != state
        {
            slot.state = state;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::{Input, Kind};

    /// The plug-in of `fixtures/clap-test-gain`, built on first use into its
    /// own folder, so that it never waits on the build running these tests.
    static TEST_PLUGIN: std::sync::LazyLock<PathBuf> = std::sync::LazyLock::new(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let target = root.join("target").join("clap-fixture");
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = Process::new(cargo)
            .arg("build")
            .arg("--quiet")
            .arg("--manifest-path")
            .arg(root.join("fixtures").join("clap-test-gain").join("Cargo.toml"))
            .arg("--target-dir")
            .arg(&target)
            .status()
            .expect("cargo");
        assert!(status.success(), "the test plug-in did not build");
        let file = if cfg!(windows) {
            "clap_test_gain.dll"
        } else if cfg!(target_os = "macos") {
            "libclap_test_gain.dylib"
        } else {
            "libclap_test_gain.so"
        };
        target.join("debug").join(file)
    });

    pub(crate) fn test_plugin() -> PluginInfo {
        clap::describe(&TEST_PLUGIN).unwrap().remove(0)
    }

    fn slot(info: &PluginInfo, key: u64) -> PluginSlot {
        PluginSlot {
            key,
            path: info.path.clone(),
            id: info.id.clone(),
            name: info.name.clone(),
            ..Default::default()
        }
    }

    fn tone(amp: f32) -> Vec<f32> {
        (0..FRAMES)
            .flat_map(|n| {
                let v = (std::f32::consts::TAU * 440.0 * n as f32 / AUDIO_RATE as f32).sin() * amp;
                [v, v]
            })
            .collect()
    }

    fn peak(b: &[f32]) -> f32 {
        b.iter().fold(0.0, |m, v| m.max(v.abs()))
    }

    /// Both threads' work, done in turn on this one.
    struct Bench {
        host: PluginHost,
        side: MainSide,
        project: Project,
    }

    impl Bench {
        fn new(project: Project) -> Self {
            clap::this_thread_is_main();
            let mut b = Bench { host: PluginHost::default(), side: MainSide::default(), project };
            b.sync();
            b
        }

        fn sync(&mut self) {
            let wanted = wanted(&self.project);
            let p = &self.project;
            self.side.sync(&self.host, &wanted, |o, k| {
                decode_state(&chain(p, o)?.iter().find(|s| s.key == k)?.state)
            });
        }

        fn serve(&mut self) {
            let p = &mut self.project;
            self.side.serve(&self.host, |o, k, state| {
                chain_mut(p, o).unwrap().iter_mut().find(|s| s.key == k).unwrap().state = state;
            });
        }

        /// Runs `blocks` blocks of tone through the chain, and returns the
        /// last one.
        fn play(&mut self, owner: Owner, blocks: usize, amp: f32) -> Vec<f32> {
            let mut rack = Rack::new(&self.host);
            rack.refresh();
            let mut block = Vec::new();
            for _ in 0..blocks {
                block = tone(amp);
                rack.run(owner, &mut block);
                rack.idle();
            }
            block
        }
    }

    fn one_input(slots: Vec<PluginSlot>) -> Project {
        let mut p = Project::blank();
        let mut input = Input::new(1, "Micro", Kind::Tone);
        input.audio.plugins = slots;
        p.inputs.push(input);
        p
    }

    const MIC: Owner = Owner::Input(1);

    #[test]
    fn the_test_plugin_applies_its_gain_after_its_latency() {
        let info = test_plugin();
        let mut b = Bench::new(one_input(vec![slot(&info, 7)]));
        let view = b.host.view(MIC, 7).unwrap();
        assert!(view.running, "{:?}", view.error);
        assert_eq!(view.latency, 32);
        assert_eq!(view.params[0].name, "Gain");

        let mut rack = Rack::new(&b.host);
        rack.refresh();
        let mut first = tone(0.8);
        rack.run(MIC, &mut first);
        assert!(first[..64].iter().all(|v| *v == 0.0), "held back by 32 samples");
        assert!((peak(&first) - 0.8).abs() < 0.01, "untouched at 0 dB");
        drop(rack);

        b.host.send(Command::SetParam { owner: MIC, key: 7, id: 0, value: -6.0206 });
        b.serve();
        let out = b.play(MIC, 3, 0.8);
        assert!((peak(&out) - 0.4).abs() < 0.01, "half as loud, got {}", peak(&out));
    }

    #[test]
    fn plugin_settings_survive_a_saved_project() {
        let info = test_plugin();
        let mut b = Bench::new(one_input(vec![slot(&info, 3)]));
        b.host.send(Command::SetParam { owner: MIC, key: 3, id: 0, value: -20.0 });
        b.serve();
        b.play(MIC, 1, 0.5);
        // The state is copied at most four times a second.
        thread::sleep(Duration::from_millis(300));
        b.serve();
        let saved = chain(&b.project, MIC).unwrap()[0].state.clone();
        assert!(!saved.is_empty(), "the project must hold the plug-in's state");

        let json = serde_json::to_string(&b.project).unwrap();
        let reopened: Project = serde_json::from_str(&json).unwrap();
        let mut again = Bench::new(reopened);
        let gain = again.host.view(MIC, 3).unwrap().params[0].value;
        assert!((gain + 20.0).abs() < 1e-9, "got {gain}");
        let out = again.play(MIC, 2, 0.5);
        assert!((peak(&out) - 0.05).abs() < 0.005, "got {}", peak(&out));
    }

    #[test]
    fn a_bypassed_plugin_leaves_the_sound_alone() {
        let info = test_plugin();
        let mut s = slot(&info, 1);
        s.state = BASE64_STANDARD.encode((-40f64).to_le_bytes());
        let mut b = Bench::new(one_input(vec![s]));
        assert!(peak(&b.play(MIC, 2, 0.5)) < 0.01);
        chain_mut(&mut b.project, MIC).unwrap()[0].bypass = true;
        b.sync();
        let out = b.play(MIC, 1, 0.5);
        assert_eq!(out, tone(0.5));
    }

    #[test]
    fn plugins_run_in_the_chain_order() {
        let info = test_plugin();
        let quiet = |key| {
            let mut s = slot(&info, key);
            s.state = BASE64_STANDARD.encode((-6.0206f64).to_le_bytes());
            s
        };
        let mut b = Bench::new(one_input(vec![quiet(1), quiet(2)]));
        let out = b.play(MIC, 3, 0.8);
        assert!((peak(&out) - 0.2).abs() < 0.01, "two halvings, got {}", peak(&out));
        // Removing one keeps the other running, with its own setting.
        chain_mut(&mut b.project, MIC).unwrap().remove(0);
        b.sync();
        b.serve();
        let out = b.play(MIC, 3, 0.8);
        assert!((peak(&out) - 0.4).abs() < 0.01, "got {}", peak(&out));
        assert!(b.host.view(MIC, 1).is_none());
    }

    #[test]
    fn a_missing_plugin_says_why_and_lets_sound_through() {
        let bad =
            PluginSlot { key: 5, path: "C:/nulle/part.clap".into(), id: "x".into(), ..Default::default() };
        let mut b = Bench::new(one_input(vec![bad]));
        let view = b.host.view(MIC, 5).unwrap();
        assert!(!view.running && view.error.is_some());
        assert_eq!(b.play(MIC, 1, 0.5), tone(0.5));
    }

    #[test]
    fn a_bus_runs_its_own_chain() {
        let info = test_plugin();
        let mut p = Project::blank();
        let mut s = slot(&info, 9);
        s.state = BASE64_STANDARD.encode((-6.0206f64).to_le_bytes());
        p.buses[0].plugins.push(s);
        let mut b = Bench::new(p);
        let out = b.play(Owner::Bus(1), 3, 0.8);
        assert!((peak(&out) - 0.4).abs() < 0.01);
        assert_eq!(b.play(Owner::Bus(0), 1, 0.8), tone(0.8), "Master has none");
    }

    #[cfg(windows)]
    #[test]
    fn the_plugin_window_opens_inside_ours_and_closes_with_it() {
        use windows_sys::Win32::Foundation::HWND;
        use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowExW, IsWindow, PostMessageW, WM_CLOSE};
        let info = test_plugin();
        let mut b = Bench::new(one_input(vec![slot(&info, 4)]));
        assert!(b.host.view(MIC, 4).unwrap().has_editor);
        b.host.send(Command::ToggleEditor { owner: MIC, key: 4 });
        b.serve();
        let window = b.side.instances[&(MIC, 4)].editor.as_ref().and_then(|e| e.window).expect("our window");
        let class: Vec<u16> = "Static\0".encode_utf16().collect();
        let child =
            unsafe { FindWindowExW(window as HWND, std::ptr::null_mut(), class.as_ptr(), std::ptr::null()) };
        assert!(!child.is_null(), "the plug-in placed its editor inside");
        assert!(b.host.view(MIC, 4).unwrap().editor_open);

        // Closed by the user, from the title bar.
        unsafe { PostMessageW(window as HWND, WM_CLOSE, 0, 0) };
        clap::editor::pump();
        b.serve();
        assert!(!b.host.view(MIC, 4).unwrap().editor_open);
        assert_eq!(unsafe { IsWindow(window as HWND) }, 0);
    }
}
