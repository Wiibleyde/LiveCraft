//! The live engine: owns the running inputs and shared state. The threads that
//! do the work live in `mixer` (pictures) and `audio` (sound); the rules of the
//! desk live in `desk`, and starting outputs in `outputs`.

pub use crate::desk::*;
use crate::frame::Frame;
use crate::model::*;
use crate::netvideo::NetOut;
use crate::output::Encoder;
use crate::plugins::PluginHost;
use crate::replay::ReplayState;
use crate::sources::{self, InputRt};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const THUMB_W: usize = 256;
pub const THUMB_H: usize = 144;

#[derive(Default)]
pub struct Stats {
    pub mix_us: AtomicU64,
    pub ticks_per_sec: AtomicU32,
    pub composed_per_sec: AtomicU32,
    pub late_ticks: AtomicU64,
    /// Percent of the whole machine, this program and everything.
    /// Several seconds in a row without keeping up with the output rate.
    pub struggling: AtomicBool,
    pub cpu_self: AtomicU32,
    pub cpu_total: AtomicU32,
    pub tick_times: TickTimes,
}

pub const TICK_STEP_US: u64 = 250;
pub const TICK_BUCKETS: usize = 512;

/// How long each tick of the video loop took, in 250 µs steps up to 128 ms.
/// One counter per step costs an addition a tick, and two readings give the
/// percentiles of any stretch of a long show.
pub struct TickTimes {
    buckets: [AtomicU64; TICK_BUCKETS],
    max_us: AtomicU64,
}

impl Default for TickTimes {
    fn default() -> Self {
        Self { buckets: std::array::from_fn(|_| AtomicU64::new(0)), max_us: AtomicU64::new(0) }
    }
}

impl TickTimes {
    pub fn record(&self, took: Duration) {
        let us = took.as_micros() as u64;
        let step = ((us / TICK_STEP_US) as usize).min(TICK_BUCKETS - 1);
        self.buckets[step].fetch_add(1, Ordering::Relaxed);
        self.max_us.fetch_max(us, Ordering::Relaxed);
    }

    pub fn counts(&self) -> Vec<u64> {
        self.buckets.iter().map(|b| b.load(Ordering::Relaxed)).collect()
    }

    /// The longest tick since the previous call.
    pub fn take_max_us(&self) -> u64 {
        self.max_us.swap(0, Ordering::Relaxed)
    }
}

/// What composes the pictures, as the settings show it.
#[derive(Clone, PartialEq, Debug)]
pub enum Compositor {
    Starting,
    /// The graphics card, by name.
    Gpu(String),
    /// The processor, and why.
    Cpu(String),
}

/// A window showing something other than the plain output.
pub const ROUTES: usize = 3;

pub struct Engine {
    pub state: Mutex<State>,
    pub(crate) rts: Mutex<HashMap<u64, Arc<InputRt>>>,
    pub program: Mutex<Arc<Frame>>,
    pub program_seq: AtomicU64,
    pub preview: Mutex<Option<Arc<Frame>>>,
    pub preview_seq: AtomicU64,
    pub thumbs: Mutex<HashMap<u64, (u64, Arc<Frame>)>>,
    /// Picture for each open screen window: Fullscreen 1, 2, External.
    pub routes: Mutex<[Option<Route>; ROUTES]>,
    pub route_frames: Mutex<[Option<Arc<Frame>>; ROUTES]>,
    pub route_seq: AtomicU64,
    pub stats: Stats,
    pub bus_peaks: [AtomicU32; 8],
    pub audio_devices: Mutex<Vec<String>>,
    pub plugins: PluginHost,
    pub recorders: Mutex<Vec<Encoder>>,
    pub streamers: Mutex<Vec<Encoder>>,
    pub multi: Mutex<Vec<(u64, Encoder)>>,
    pub srt: Mutex<Vec<(Route, Encoder)>>,
    /// NDI and OMT senders of the program.
    pub net: Mutex<Vec<NetOut>>,
    pub replay: Mutex<ReplayState>,
    /// Failure message from an output that stopped by itself.
    pub output_error: Mutex<Option<String>>,
    pub record_started: Mutex<Option<Instant>>,
    pub stream_started: Mutex<Option<Instant>>,
    pub(crate) repaint: Mutex<Option<eframe::egui::Context>>,
    pub(crate) quit: AtomicBool,
    /// Compose on the graphics card when there is a suitable one.
    pub use_gpu: AtomicBool,
    pub compositor: Mutex<Compositor>,
    next_id: AtomicU64,
    /// Counts the projects opened, so that what ran for the last one is not kept.
    pub(crate) project_generation: AtomicU64,
    pub(crate) thumb_clock: AtomicU64,
    pub script_stop: AtomicU64,
}

impl Engine {
    pub fn start(project: Project) -> Arc<Self> {
        sources::tie_children_to_this_process();
        let (w, h) = (project.out_w, project.out_h);
        let next = project.inputs.iter().map(|i| i.id).max().unwrap_or(0) + 1;
        let engine = Arc::new(Self {
            state: Mutex::new(State::new(project)),
            rts: Mutex::new(HashMap::new()),
            program: Mutex::new(Arc::new(Frame::solid(w, h, [0, 0, 0]))),
            program_seq: AtomicU64::new(1),
            preview: Mutex::new(None),
            preview_seq: AtomicU64::new(1),
            thumbs: Mutex::new(HashMap::new()),
            routes: Mutex::new([None; ROUTES]),
            route_frames: Mutex::new([None, None, None]),
            route_seq: AtomicU64::new(0),
            stats: Stats::default(),
            bus_peaks: Default::default(),
            audio_devices: Mutex::new(Vec::new()),
            plugins: PluginHost::default(),
            recorders: Mutex::new(Vec::new()),
            streamers: Mutex::new(Vec::new()),
            multi: Mutex::new(Vec::new()),
            srt: Mutex::new(Vec::new()),
            net: Mutex::new(Vec::new()),
            replay: Mutex::new(ReplayState::default()),
            output_error: Mutex::new(None),
            record_started: Mutex::new(None),
            stream_started: Mutex::new(None),
            repaint: Mutex::new(None),
            quit: AtomicBool::new(false),
            next_id: AtomicU64::new(next),
            use_gpu: AtomicBool::new(true),
            compositor: Mutex::new(Compositor::Starting),
            project_generation: AtomicU64::new(0),
            thumb_clock: AtomicU64::new(0),
            script_stop: AtomicU64::new(0),
        });
        engine.start_all_inputs(false);
        engine.state.lock().unwrap().note_project_on_air();
        for job in [Self::video_loop, Self::audio_loop, Self::cpu_loop, Self::data_loop, Self::plugin_loop] {
            let e = engine.clone();
            thread::spawn(move || job(e));
        }
        engine
    }

    pub fn set_repaint(&self, ctx: eframe::egui::Context) {
        *self.repaint.lock().unwrap() = Some(ctx);
    }

    pub(crate) fn ping(&self) {
        if let Some(ctx) = self.repaint.lock().unwrap().as_ref() {
            ctx.request_repaint();
        }
    }

    fn cpu_loop(self: Arc<Self>) {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
        let Ok(mut sys) = System::new() else { return };
        let pid = sysinfo::get_current_pid().ok();
        let cores = thread::available_parallelism().map_or(1, |n| n.get()) as f32;
        while !self.quit.load(Ordering::Relaxed) {
            sys.refresh_cpu_usage();
            if let Some(pid) = pid {
                sys.refresh_processes_specifics(
                    ProcessesToUpdate::Some(&[pid]),
                    true,
                    ProcessRefreshKind::nothing().with_cpu(),
                );
                if let Some(p) = sys.process(pid) {
                    self.stats.cpu_self.store((p.cpu_usage() / cores) as u32, Ordering::Relaxed);
                }
            }
            self.stats.cpu_total.store(sys.global_cpu_usage() as u32, Ordering::Relaxed);
            thread::sleep(Duration::from_secs(1));
        }
    }

    /// Starts a source for every input. With `keep`, each carries on from where
    /// its previous source stood, for when only the format changes.
    fn start_all_inputs(&self, keep: bool) {
        let (inputs, w, h, fps, card) = {
            let s = self.state.lock().unwrap();
            let p = &s.project;
            (p.inputs.clone(), p.out_w, p.out_h, p.fps, p.hardware_decoding)
        };
        let mut rts = self.rts.lock().unwrap();
        let old: HashMap<u64, Arc<InputRt>> = std::mem::take(&mut *rts);
        for i in &inputs {
            let resume = old.get(&i.id).filter(|_| keep).map(|r| r.resume());
            rts.insert(i.id, sources::start_seeded(i, w, h, fps, card, None, resume));
        }
        for rt in old.values() {
            rt.shutdown();
        }
    }

    pub fn rt(&self, id: u64) -> Option<Arc<InputRt>> {
        self.rts.lock().unwrap().get(&id).cloned()
    }

    pub fn new_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn add_input(&self, input: Input) {
        let (w, h, fps, card) = {
            let mut s = self.state.lock().unwrap();
            s.project.inputs.push(input.clone());
            s.version += 1;
            (s.project.out_w, s.project.out_h, s.project.fps, s.project.hardware_decoding)
        };
        let rt = sources::start(&input, w, h, fps, card);
        self.rts.lock().unwrap().insert(input.id, rt);
    }

    pub fn close_input(&self, id: u64) {
        self.state.lock().unwrap().close_input(id);
        if let Some(rt) = self.rts.lock().unwrap().remove(&id) {
            rt.shutdown();
        }
        self.thumbs.lock().unwrap().remove(&id);
        self.stop_outputs_of(id);
    }

    /// Brings back the last closed input and everything that used it.
    pub fn undo(&self) {
        let back = self.state.lock().unwrap().undo.take();
        let Some(closed) = back else { return };
        let (w, h, fps, card) = {
            let s = self.state.lock().unwrap();
            (s.project.out_w, s.project.out_h, s.project.fps, s.project.hardware_decoding)
        };
        // The running source first, so the input never appears without one.
        let rt = sources::start(&closed.input, w, h, fps, card);
        self.rts.lock().unwrap().insert(closed.input.id, rt);
        self.state.lock().unwrap().restore(closed);
    }

    /// Restarts an input after its source settings changed. With `keep`, it
    /// carries on playing from the same place, as on air it must.
    pub fn restart_input(&self, id: u64, keep: bool) {
        let found = {
            let mut s = self.state.lock().unwrap();
            s.version += 1;
            let (w, h, fps, card) =
                (s.project.out_w, s.project.out_h, s.project.fps, s.project.hardware_decoding);
            s.input(id).cloned().map(|i| (i, w, h, fps, card))
        };
        if let Some((input, w, h, fps, card)) = found {
            // The old picture stays up until the new one is ready: editing a
            // title on air must not blink.
            let old = self.rt(id);
            let seed = old.as_ref().and_then(|r| r.latest());
            let resume = old.as_ref().filter(|_| keep).map(|r| r.resume());
            let rt = sources::start_seeded(&input, w, h, fps, card, seed, resume);
            if let Some(old) = self.rts.lock().unwrap().insert(id, rt) {
                old.shutdown();
            }
            self.state.lock().unwrap().version += 1;
        }
    }

    /// Turns decoding on the graphics card on or off. Only the inputs it can
    /// concern restart, and they carry on from where they stood.
    pub fn set_hardware_decoding(&self, on: bool) {
        let concerned: Vec<u64> = {
            let mut s = self.state.lock().unwrap();
            s.project.hardware_decoding = on;
            let p = &s.project;
            p.inputs
                .iter()
                .filter(|i| matches!(i.kind, Kind::Video { .. } | Kind::List { .. } | Kind::Network { .. }))
                .map(|i| i.id)
                .collect()
        };
        for id in concerned {
            self.restart_input(id, true);
        }
    }

    pub fn load_project(&self, project: Project) {
        // Anything recording a single input must not end up on another one
        // that happens to carry the same number in the new project.
        self.stop_multi();
        self.stop_srt();
        self.replay_stop();
        let next = project.inputs.iter().map(|i| i.id).max().unwrap_or(0) + 1;
        self.next_id.fetch_max(next, Ordering::Relaxed);
        let (w, h) = (project.out_w, project.out_h);
        {
            let mut s = self.state.lock().unwrap();
            let (version, padlock) = (s.version, s.padlock);
            let (ftb, ftb_level) = (s.ftb, s.ftb_level);
            *s = State::new(project);
            s.version = version + 1;
            s.padlock = padlock;
            s.ftb = ftb;
            s.ftb_level = ftb_level;
            s.note_project_on_air();
        }
        *self.program.lock().unwrap() = Arc::new(Frame::solid(w, h, [0, 0, 0]));
        self.program_seq.fetch_add(1, Ordering::Release);
        self.thumbs.lock().unwrap().clear();
        // After the new project is in place, so that the plug-ins read its states.
        self.project_generation.fetch_add(1, Ordering::Release);
        self.start_all_inputs(false);
        self.state.lock().unwrap().version += 1;
    }

    /// Adds another project's inputs. Their numbers change, so every link
    /// between them follows; links to anything left behind are dropped.
    pub fn append_project(&self, project: Project) {
        let map: HashMap<u64, u64> = project.inputs.iter().map(|i| (i.id, self.new_id())).collect();
        let renumber = |v: &mut Option<u64>| *v = v.and_then(|id| map.get(&id).copied());
        let tabs = self.state.lock().unwrap().project.categories.clone();
        for mut input in project.inputs {
            input.id = map[&input.id];
            input.layers.retain_mut(|l| match map.get(&l.input) {
                Some(new) => {
                    l.input = *new;
                    true
                }
                None => false,
            });
            renumber(&mut input.key.fill_input);
            match &mut input.kind {
                Kind::Mix { preview, output, .. } => {
                    renumber(preview);
                    renumber(output);
                }
                Kind::Telestrator { background } => renumber(background),
                Kind::VirtualSet { background, foreground, .. } => {
                    renumber(background);
                    renumber(foreground);
                }
                Kind::Title { data, .. } => *data = None,
                // Never the link of a call already on this desk.
                Kind::VideoCall { token, .. } => *token = crate::call::new_token(),
                _ => {}
            }
            // A tab keeps its meaning by name, not by its place in the list.
            let name = input.category.and_then(|n| project.categories.get(n)).map(|c| &c.name);
            input.category = name.and_then(|n| tabs.iter().position(|c| !c.name.is_empty() && c.name == *n));
            self.add_input(input);
        }
    }

    pub fn set_format(&self, w: usize, h: usize, fps: u32) {
        {
            let mut s = self.state.lock().unwrap();
            s.project.out_w = w;
            s.project.out_h = h;
            s.project.fps = fps;
            s.version += 1;
        }
        self.thumbs.lock().unwrap().clear();
        self.start_all_inputs(true);
        self.state.lock().unwrap().version += 1;
    }

    pub fn set_inputs_paused(&self, paused: bool) {
        let mut s = self.state.lock().unwrap();
        s.inputs_paused = paused;
        if paused {
            // Only what was playing starts again afterwards.
            let timed: Vec<u64> =
                s.project.inputs.iter().filter(|i| i.kind.has_timeline()).map(|i| i.id).collect();
            let mut stopped = Vec::new();
            for id in timed {
                if let Some(rt) = self.rt(id).filter(|r| r.is_playing()) {
                    rt.playing.store(false, Ordering::Relaxed);
                    stopped.push(id);
                }
            }
            s.paused_by_desk = stopped;
        } else {
            for id in std::mem::take(&mut s.paused_by_desk) {
                if let Some(rt) = self.rt(id) {
                    rt.playing.store(true, Ordering::Relaxed);
                }
            }
        }
    }

    pub fn snapshot_to(&self, path: &str, frame: &Frame) -> Result<(), String> {
        let lower = path.to_lowercase();
        if lower.ends_with(".jpg") || lower.ends_with(".jpeg") || lower.ends_with(".bmp") {
            let rgb: Vec<u8> =
                frame.data.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
            return image::save_buffer(path, &rgb, frame.w as u32, frame.h as u32, image::ColorType::Rgb8)
                .map_err(|e| e.to_string());
        }
        image::save_buffer(path, &frame.data, frame.w as u32, frame.h as u32, image::ColorType::Rgba8)
            .map_err(|e| e.to_string())
    }
}
