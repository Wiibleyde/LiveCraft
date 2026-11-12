//! Instant replay: each chosen input records to its own growing file while the
//! show runs; marked moments play back from those files at any speed, on one
//! camera or four at a time.
//!
//! Marks are moments of the show, not positions in a file: a file loses
//! pictures when its encoder falls behind or the mixer runs late, and from then
//! on lags the clock. Each file's [`FrameLog`] says when its pictures were
//! taken, which turns a moment back into the right picture on every camera.

use crate::engine::Engine;
use crate::model::{AudioPick, Input, Kind};
use crate::output::{Encoder, Spec, Target};
use crate::sources::{ReplayClip, ReplayPlay};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const MAX_CAMERAS: usize = 8;

pub struct Camera {
    pub input: u64,
    pub name: String,
    pub path: String,
    pub encoder: Encoder,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Event {
    /// Seconds of the session, counted from the first picture of its first
    /// camera, which is also where that camera's file begins.
    pub start_s: f64,
    pub end_s: f64,
    pub camera: usize,
    pub text: String,
    pub tags: Vec<String>,
    pub speed: f32,
    /// The recordings it was marked on: a later session records to new files,
    /// and this moment must keep pointing at the old ones.
    pub files: Vec<String>,
    /// The tags as typed, so a comma can be typed before the next tag.
    #[serde(skip)]
    pub tag_text: Option<String>,
}

/// When each picture of a recording was taken. Pictures normally follow at
/// the frame rate, so only breaks in that beat are kept: a steady hour is one
/// entry, and each lost or late picture adds one. The copies the mixer makes
/// to fill a late tick share its instant and are counted on one entry.
#[derive(Clone, Debug)]
pub struct FrameLog {
    period: Duration,
    count: u64,
    breaks: Vec<Break>,
}

/// `same` pictures from pictures `first`, all taken at `at`, then one per period.
#[derive(Clone, Copy, Debug)]
struct Break {
    first: u64,
    at: Instant,
    same: u64,
}

impl Break {
    /// The last picture taken at `at`: the beat goes on from it.
    fn anchor(&self) -> u64 {
        self.first + self.same - 1
    }
}

fn apart(a: Instant, b: Instant) -> Duration {
    a.max(b) - a.min(b)
}

impl FrameLog {
    pub fn new(fps: u32) -> Self {
        // Rounded as the mixer rounds its beat, so a steady hour stays on it.
        Self { period: Duration::from_secs_f64(1.0 / fps.max(1) as f64), count: 0, breaks: Vec::new() }
    }

    /// Notes the next picture written to the file.
    pub fn push(&mut self, taken: Instant) {
        let n = self.count;
        self.count += 1;
        if let Some(last) = self.breaks.last_mut()
            && last.anchor() == n - 1
            && last.at == taken
        {
            last.same += 1;
            return;
        }
        // An eighth of a period absorbs the scheduler's jitter. More would let
        // a small phase shift after a late tick stay unnoticed, and every later
        // picture would be taken to be that much off.
        if self.taken(n).is_none_or(|beat| apart(beat, taken) > self.period / 8) {
            self.breaks.push(Break { first: n, at: taken, same: 1 });
        }
    }

    /// Where the file begins on the show's clock.
    pub fn first(&self) -> Option<Instant> {
        self.breaks.first().map(|b| b.at)
    }

    /// When picture `n` was taken. Pictures still on their way to the file are
    /// expected on the beat.
    pub fn taken(&self, n: u64) -> Option<Instant> {
        let k = self.breaks.partition_point(|b| b.first <= n).checked_sub(1)?;
        let b = self.breaks[k];
        Some(b.at + self.period * n.saturating_sub(b.anchor()) as u32)
    }

    /// The picture on screen at `at`: the last one taken by then.
    pub fn picture_at(&self, at: Instant) -> u64 {
        let k = self.breaks.partition_point(|b| b.at <= at);
        let Some(b) = k.checked_sub(1).and_then(|k| self.breaks.get(k)) else { return 0 };
        let beats = (at - b.at).as_nanos() / self.period.as_nanos().max(1);
        let n = b.anchor() + beats as u64;
        // A run of pictures ends where the next break begins.
        self.breaks.get(k).map_or(n, |next| n.min(next.first - 1))
    }

    /// Where picture `n` is in the file, in seconds from its first picture.
    fn position_s(&self, n: u64) -> f64 {
        n as f64 * self.period.as_secs_f64()
    }
}

fn duration(s: f64) -> Duration {
    Duration::try_from_secs_f64(s.max(0.0)).unwrap_or_default()
}

#[derive(Default)]
pub struct ReplayState {
    pub cams: Vec<Camera>,
    pub started: Option<Instant>,
    pub events: Vec<Event>,
    pub mark_in: Option<Instant>,
    pub selected: Option<usize>,
    pub speed: f32,
    pub quad: bool,
    pub input: Option<u64>,
    pub folder: Option<PathBuf>,
    /// The show's frame rate while recording, 0 before any session.
    fps: u32,
    /// Picture logs by file, kept after a session ends for the events on it.
    logs: HashMap<String, Arc<Mutex<FrameLog>>>,
}

impl ReplayState {
    /// Takes over a new session's cameras.
    pub(crate) fn begin(&mut self, cams: Vec<Camera>) {
        for c in &cams {
            self.logs.insert(c.path.clone(), c.encoder.pictures());
        }
        self.cams = cams;
        self.started = Some(Instant::now());
        // A mark left open was taken while other files were recording.
        self.mark_in = None;
    }

    /// Where a session begins: the first picture of its first camera.
    fn origin(&self, first_file: Option<&String>) -> Option<Instant> {
        self.logs.get(first_file?)?.lock().unwrap().first()
    }

    pub fn recorded_s(&self) -> f64 {
        self.origin(self.cams.first().map(|c| &c.path)).map_or(0.0, |o| o.elapsed().as_secs_f64())
    }

    /// Keeps the moment from `start` to `end` on the cameras recording now.
    pub(crate) fn mark(&mut self, start: Instant, end: Instant) {
        let files = self.cams.iter().map(|c| c.path.clone()).collect();
        self.mark_on(files, start, end);
    }

    fn mark_on(&mut self, files: Vec<String>, start: Instant, end: Instant) {
        let origin = self.origin(files.first());
        let secs = |at: Instant| origin.map_or(0.0, |o| at.saturating_duration_since(o).as_secs_f64());
        let (start_s, end_s, speed) = (secs(start), secs(end), self.speed);
        self.events.push(Event { start_s, end_s, speed, files, ..Event::default() });
        self.selected = Some(self.events.len() - 1);
    }

    /// What playing an event shows: its camera, or four at once.
    pub(crate) fn clip_of(&self, index: usize) -> Option<ReplayPlay> {
        let e = self.events.get(index)?;
        let all: Vec<String> = if e.files.is_empty() {
            self.cams.iter().map(|c| c.path.clone()).collect()
        } else {
            e.files.clone()
        };
        let files: Vec<&String> =
            if self.quad { all.iter().take(4).collect() } else { vec![all.get(e.camera).or(all.first())?] };
        let speed = if e.speed > 0.0 { e.speed } else { self.speed.max(0.05) };
        let origin = self.origin(all.first());
        // Every camera is timed from the first one's opening picture, so the
        // angles stay together whatever each of them lost.
        let mut zero = None;
        let mut clips = Vec::new();
        for file in files {
            let (Some(origin), Some(log)) = (origin, self.logs.get(file)) else {
                // Without a log, marks are positions in the files, as they
                // were before logs.
                // Half a picture either side, as with a log, since the decoder
                // keeps the pictures stamped inside the bounds.
                let half = 0.5 / if self.fps == 0 { 30 } else { self.fps } as f64;
                clips.push(ReplayClip {
                    file: file.clone(),
                    from_s: (e.start_s - half).max(0.0),
                    to_s: e.end_s + half,
                    times: Vec::new(),
                });
                continue;
            };
            let log = log.lock().unwrap();
            let first = log.picture_at(origin + duration(e.start_s));
            let last = log.picture_at(origin + duration(e.end_s)).max(first);
            let zero = *zero.get_or_insert_with(|| log.taken(first).unwrap_or(origin));
            // Half a picture either side: the cut falls between two pictures,
            // whatever rounding the file's clock does.
            let half = log.period.as_secs_f64() / 2.0;
            let since = |n| log.taken(n).map_or(0.0, |t| t.saturating_duration_since(zero).as_secs_f64());
            clips.push(ReplayClip {
                file: file.clone(),
                from_s: (log.position_s(first) - half).max(0.0),
                to_s: log.position_s(last) + half,
                times: (first..=last).map(since).collect(),
            });
        }
        let span_s = match (zero, origin) {
            (Some(zero), Some(origin)) => {
                (origin + duration(e.end_s)).saturating_duration_since(zero).as_secs_f64()
            }
            _ => e.end_s - e.start_s,
        };
        Some(ReplayPlay { clips, span_s, speed })
    }
}

/// Text search over an event's note and tags.
pub fn matches(e: &Event, query: &str) -> bool {
    let q = query.to_lowercase();
    q.is_empty() || e.text.to_lowercase().contains(&q) || e.tags.iter().any(|t| t.to_lowercase().contains(&q))
}

fn finish_cameras(cams: Vec<Camera>) {
    for c in cams {
        thread::spawn(move || c.encoder.finish(Duration::from_secs(5)));
    }
}

impl Engine {
    /// Starts recording the chosen inputs, adding the Replay input if needed.
    pub fn replay_start(&self, ids: &[u64], folder: PathBuf) -> Result<usize, String> {
        if self.is_replay() {
            return Ok(0);
        }
        std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
        let (inputs, w, h, fps) = {
            let s = self.state.lock().unwrap();
            let p = &s.project;
            let picked: Vec<Input> =
                p.inputs.iter().filter(|i| ids.contains(&i.id)).take(MAX_CAMERAS).cloned().collect();
            (picked, p.out_w, p.out_h, p.fps)
        };
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S-%3f").to_string();
        let mut cams: Vec<Camera> = Vec::new();
        for (n, input) in inputs.into_iter().enumerate() {
            let path = folder.join(format!("ralenti-{stamp}-cam{}.ts", n + 1)).to_string_lossy().into_owned();
            let opened = Encoder::start(Spec {
                label: t!("replay.encoder_label", name = &input.name),
                target: Target::Growing(path.clone()),
                codec: "libx264".into(),
                w,
                h,
                fps,
                kbps: 20_000,
                audio_kbps: 128,
                audio: AudioPick::MASTER,
            });
            let encoder = match opened {
                Ok(e) => e,
                Err(e) => {
                    finish_cameras(cams);
                    return Err(e);
                }
            };
            cams.push(Camera { input: input.id, name: input.name, path, encoder });
        }
        let count = cams.len();
        if count == 0 {
            return Err(t!("replay.choose_camera").into());
        }
        let existing = {
            let s = self.state.lock().unwrap();
            s.project.inputs.iter().find(|i| matches!(i.kind, Kind::Replay)).map(|i| i.id)
        };
        let id = existing.unwrap_or_else(|| {
            let id = self.new_id();
            self.add_input(Input::new(id, t!("replay.input_name"), Kind::Replay));
            id
        });
        let mut r = self.replay.lock().unwrap();
        r.begin(cams);
        r.fps = fps;
        r.input = Some(id);
        r.folder = Some(folder);
        if r.speed <= 0.0 {
            r.speed = 0.5;
        }
        Ok(count)
    }

    pub fn replay_stop(&self) {
        let cams = {
            let mut r = self.replay.lock().unwrap();
            r.started = None;
            std::mem::take(&mut r.cams)
        };
        finish_cameras(cams);
    }

    pub fn replay_mark_in(&self) {
        let mut r = self.replay.lock().unwrap();
        if r.started.is_some() {
            r.mark_in = Some(Instant::now());
        }
    }

    pub fn replay_mark_out(&self) {
        let mut r = self.replay.lock().unwrap();
        if let Some(start) = r.mark_in.take() {
            r.mark(start, Instant::now());
        }
    }

    /// Marks the last few seconds in one press.
    pub fn replay_mark_last(&self, seconds: f64) {
        let mut r = self.replay.lock().unwrap();
        if r.started.is_none() {
            return;
        }
        let end = Instant::now();
        // Before the session began counts as its start.
        r.mark(end.checked_sub(duration(seconds)).unwrap_or(end), end);
    }

    /// Plays an event on the Replay input, on its camera or four at once.
    pub fn replay_play(&self, index: usize) -> Option<u64> {
        let (play, input) = {
            let r = self.replay.lock().unwrap();
            (r.clip_of(index)?, r.input?)
        };
        let rt = self.rt(input)?;
        rt.playing.store(true, Ordering::Relaxed);
        *rt.replay.lock().unwrap() = Some(play);
        Some(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FPS: u32 = 30;

    fn beat() -> Duration {
        Duration::from_secs_f64(1.0 / FPS as f64)
    }

    /// The mixer's ticks in a session that stalls for 0.4 s at tick 75.
    fn tick(t0: Instant, n: u32) -> Instant {
        let stall = if n >= 75 { Duration::from_millis(400) } else { Duration::ZERO };
        t0 + beat() * n + stall
    }

    /// A camera that loses ticks 45 to 74, then at the late tick 75 adds 12
    /// copies for the ticks it missed, as the mixer does, then records
    /// steadily: tick 100 is the file's picture 82.
    fn lossy(t0: Instant) -> FrameLog {
        let mut log = FrameLog::new(FPS);
        for n in (0..45).chain(75..150) {
            let copies = if n == 75 { 13 } else { 1 };
            for _ in 0..copies {
                log.push(tick(t0, n));
            }
        }
        log
    }

    fn steady(t0: Instant) -> FrameLog {
        let mut log = FrameLog::new(FPS);
        (0..150).for_each(|n| log.push(tick(t0, n)));
        log
    }

    fn session(cams: Vec<(&str, FrameLog)>) -> ReplayState {
        let mut r = ReplayState { speed: 1.0, ..ReplayState::default() };
        for (file, log) in cams {
            r.logs.insert(file.into(), Arc::new(Mutex::new(log)));
        }
        r
    }

    /// The pictures read for a clip: picture n of a file is n frames after
    /// its first, and reading keeps those inside the bounds.
    fn pictures_read(clip: &ReplayClip) -> (u64, u64) {
        let picture = |s: f64| (s * FPS as f64).ceil() as u64;
        (picture(clip.from_s), picture(clip.to_s) - 1)
    }

    #[test]
    fn marks_find_their_pictures_after_lost_ones() {
        let t0 = Instant::now();
        let mut r = session(vec![("cam1.ts", lossy(t0))]);
        let half = beat() / 2;
        r.mark_on(vec!["cam1.ts".into()], tick(t0, 100) + half, tick(t0, 140) + half);
        let play = r.clip_of(0).unwrap();
        assert_eq!(pictures_read(&play.clips[0]), (82, 122), "the pictures on screen at the marks");
        assert!((play.span_s - 40.5 / FPS as f64).abs() < 1e-6, "lasts as long as marked: {}", play.span_s);
    }

    #[test]
    fn lost_pictures_keep_their_time_in_a_replay() {
        let t0 = Instant::now();
        let mut r = session(vec![("cam1.ts", lossy(t0))]);
        let half = beat() / 2;
        r.mark_on(vec!["cam1.ts".into()], tick(t0, 30) + half, tick(t0, 90) + half);
        let play = r.clip_of(0).unwrap();
        let clip = &play.clips[0];
        assert_eq!(pictures_read(clip), (30, 72));
        // What the player shows at a moment: the last picture due by then.
        let shown = |s: f64| clip.times.iter().filter(|t| **t <= s + 1e-6).count() - 1;
        assert_eq!(shown(1.0), 14, "a second in, the picture before the loss still holds, as on the show");
        assert_eq!(shown(1.9), 27, "the late tick shows once, its copies passed over");
        assert_eq!(shown(1.9 + 1.0 / FPS as f64), 28);
        assert!((play.span_s - (60.5 / FPS as f64 + 0.4)).abs() < 1e-6, "{}", play.span_s);
    }

    #[test]
    fn four_angles_stay_together_when_one_lost_pictures() {
        let t0 = Instant::now();
        let mut r = session(vec![("cam1.ts", steady(t0)), ("cam2.ts", lossy(t0))]);
        r.quad = true;
        let half = beat() / 2;
        r.mark_on(vec!["cam1.ts".into(), "cam2.ts".into()], tick(t0, 100) + half, tick(t0, 140) + half);
        let play = r.clip_of(0).unwrap();
        assert_eq!(pictures_read(&play.clips[0]), (100, 140));
        assert_eq!(pictures_read(&play.clips[1]), (82, 122));
        assert_eq!(play.clips[0].times, play.clips[1].times, "each picture shows at the same moment");
    }

    #[test]
    fn moments_marked_without_a_log_play_from_their_positions() {
        let old = r#"{"start_s":2.0,"end_s":5.0,"camera":0,"text":"But","tags":[],"speed":0.5}"#;
        let mut e: Event = serde_json::from_str(old).unwrap();
        e.files = vec!["ancien.ts".into()];
        let r = ReplayState { events: vec![e], ..ReplayState::default() };
        let play = r.clip_of(0).unwrap();
        let clip = &play.clips[0];
        let half = 0.5 / FPS as f64;
        assert_eq!((clip.from_s, clip.to_s), (2.0 - half, 5.0 + half), "half a picture either side");
        assert_eq!((play.span_s, play.speed), (3.0, 0.5));
        assert_eq!(clip.due(30, FPS), Some(1.0), "pictures follow at the frame rate");
    }

    #[test]
    fn a_steady_hour_costs_one_entry() {
        let t0 = Instant::now();
        let mut log = FrameLog::new(FPS);
        let hour = 3600 * FPS;
        for n in 0..hour {
            // A scheduler wakes a little late, never on the dot.
            log.push(t0 + beat() * n + Duration::from_millis((n % 4) as u64));
        }
        assert_eq!(log.breaks.len(), 1);
        let last = (hour - 1) as u64;
        assert_eq!(log.picture_at(log.taken(last).unwrap() + beat() / 2), last, "no drift over an hour");
    }

    #[test]
    fn many_late_ticks_keep_the_log_small() {
        let t0 = Instant::now();
        let mut log = FrameLog::new(FPS);
        let mut at = t0;
        // Every tenth tick runs 0.2 s late and is repeated for the ticks it missed.
        for n in 0..10_000u32 {
            at += if n % 10 == 9 { beat() * 7 } else { beat() };
            let copies = if n % 10 == 9 { 7 } else { 1 };
            (0..copies).for_each(|_| log.push(at));
        }
        assert_eq!(log.count, 10_000 - 1000 + 7000);
        assert!(log.breaks.len() <= 2000, "one entry per late tick, not per copy: {}", log.breaks.len());
        assert_eq!(log.taken(log.count - 1), Some(at), "the last copy is on its tick");
    }

    #[test]
    fn a_small_phase_shift_does_not_move_later_marks() {
        let t0 = Instant::now();
        let mut log = FrameLog::new(FPS);
        // Tick 50 runs 6 ms late, and the beat goes on from there.
        let at = |n: u32| t0 + beat() * n + if n >= 50 { Duration::from_millis(6) } else { Duration::ZERO };
        (0..100).for_each(|n| log.push(at(n)));
        for n in 50..100u32 {
            let t = log.taken(n as u64).unwrap();
            assert!(apart(t, at(n)) < Duration::from_micros(500), "picture {n} is {:?} off", apart(t, at(n)));
        }
        let before = at(60) - Duration::from_millis(1);
        assert_eq!(log.picture_at(before), 59, "just before picture 60 is still on 59");
        assert_eq!(log.picture_at(at(60)), 60);
    }

    #[test]
    fn moments_without_a_log_keep_half_a_picture_either_side() {
        let mut r = ReplayState { speed: 1.0, fps: FPS, ..ReplayState::default() };
        r.events.push(Event { start_s: 2.0, end_s: 3.0, files: vec!["old.ts".into()], ..Event::default() });
        let clip = &r.clip_of(0).unwrap().clips[0];
        let half = 0.5 / FPS as f64;
        assert!((clip.from_s - (2.0 - half)).abs() < 1e-9 && (clip.to_s - (3.0 + half)).abs() < 1e-9);
        r.events[0].start_s = 0.0;
        assert_eq!(r.clip_of(0).unwrap().clips[0].from_s, 0.0, "never before the file");
    }

    #[test]
    fn events_are_found_by_note_or_tag() {
        let e = Event { text: "But".into(), tags: vec!["Équipe A".into()], ..Event::default() };
        assert!(matches(&e, "but"));
        assert!(matches(&e, "équipe"));
        assert!(!matches(&e, "corner"));
        assert!(matches(&e, ""));
    }
}
