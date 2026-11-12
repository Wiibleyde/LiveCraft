//! `--soak <folder> [duration]`: a whole show left running, with memory,
//! timing and outputs watched throughout. A leak or a slow drift only shows
//! after many minutes, which `--selftest` never reaches.
//!
//! Every five seconds a line goes to `soak.csv`; the verdict and the figures
//! behind it go to `soak.json` at the end.

use crate::api::ApiConfig;
use crate::engine::{Engine, TICK_BUCKETS, TICK_STEP_US};
use crate::model::*;
use crate::playtest::{make, until};
use crate::selftest::http_get;
use crate::sources::{AUDIO_RATE, ffmpeg};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::hash::Hash;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::sleep;
use std::time::{Duration, Instant};

const SAMPLE_EVERY: Duration = Duration::from_secs(5);
const STEP_EVERY: Duration = Duration::from_secs(6);
const STEPS: usize = 12;
const STREAM_URL: &str = "rtmp://127.0.0.1:19350/live/soak";
const DEFAULT_LENGTH: Duration = Duration::from_secs(30 * 60);

/// Start-up fills buffers, loads the page and opens encoders: not a trend.
const WARMUP_MIN: Duration = Duration::from_secs(120);
/// Private memory wanders by tens of megabytes as buffers come and go; a leak
/// keeps climbing. Both the slope and the rise it made over the run must be
/// large before it counts.
const MEM_SLOPE_MB_H: f64 = 60.0;
const MEM_RISE_MB: f64 = 40.0;
/// Every thread and process holds handles, so a leak of either shows here.
const HANDLE_SLOPE_H: f64 = 600.0;
const HANDLE_RISE: f64 = 200.0;
/// Processes come and go with replays; leaked ones raise the floor.
const CHILDREN_RISE: i64 = 2;
/// A window missing this share of its ticks is late...
const LATE_WINDOW: f64 = 0.10;
/// ...and late windows in a row for this long are a show in trouble.
const SUSTAINED_LATE: Duration = Duration::from_secs(30);
/// Ticks missed over the whole run, once warmed up.
const MISSED_MAX: f64 = 0.01;
/// Pictures an encoder could not take in time.
const DROPPED_MAX: f64 = 0.005;
/// Picture and sound of one file must end together.
const AV_SKEW_MAX_S: f64 = 0.25;
/// A file must last as long as the time it covered.
const LENGTH_SKEW: f64 = 0.002;
const LENGTH_SKEW_MIN_S: f64 = 1.0;
/// Error shown by an input for this many samples in a row.
const STATUS_SAMPLES: u32 = 3;
/// A playing source whose picture did not change for this many samples.
const FROZEN_SAMPLES: u32 = 2;
/// Decoders and the browser may take a while to get going on a busy machine.
const SETTLE: Duration = Duration::from_secs(30);
/// Seconds each source, then the web page, may take to show a first picture.
const SOURCE_START_S: f32 = 30.0;
const PAGE_START_S: f32 = 90.0;

pub fn run(dir: &str, length: Option<&str>) -> i32 {
    let length = match length {
        None => DEFAULT_LENGTH,
        Some(text) => match parse_length(text) {
            Some(d) => d,
            None => {
                println!("ECHEC: durée illisible « {text} » (exemples : 45m, 90s, 1h)");
                return 2;
            }
        },
    };
    match soak(dir, length) {
        Ok(summary) => {
            println!("OK: {summary}");
            0
        }
        Err(e) => {
            println!("ECHEC: {e}");
            1
        }
    }
}

/// `90s`, `45m`, `1h`, or a bare number of minutes.
fn parse_length(text: &str) -> Option<Duration> {
    let text = text.trim();
    let split = text.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(text.len());
    let n: f64 = text[..split].parse().ok()?;
    let seconds = match &text[split..] {
        "s" => n,
        "" | "m" | "min" => n * 60.0,
        "h" => n * 3600.0,
        _ => return None,
    };
    (seconds >= 1.0).then(|| Duration::from_secs_f64(seconds))
}

/// Milliseconds at quantile `q` of a tick histogram, as the upper edge of its
/// step, so the figure never flatters.
fn percentile(counts: &[u64], q: f64) -> f64 {
    let total: u64 = counts.iter().sum();
    if total == 0 {
        return 0.0;
    }
    let want = ((total as f64 * q).ceil() as u64).max(1);
    let mut seen = 0;
    for (step, n) in counts.iter().enumerate() {
        seen += n;
        if seen >= want {
            return ((step as u64 + 1) * TICK_STEP_US) as f64 / 1000.0;
        }
    }
    (TICK_BUCKETS as u64 * TICK_STEP_US) as f64 / 1000.0
}

fn minus(now: &[u64], before: &[u64]) -> Vec<u64> {
    now.iter().zip(before).map(|(a, b)| a.saturating_sub(*b)).collect()
}

/// Least-squares slope of `(seconds, value)` points, per hour.
fn slope_per_hour(points: &[(f64, f64)]) -> f64 {
    let n = points.len() as f64;
    if n < 2.0 {
        return 0.0;
    }
    let (mt, mv) = points.iter().fold((0.0, 0.0), |(t, v), p| (t + p.0 / n, v + p.1 / n));
    let (cov, var) =
        points.iter().fold((0.0, 0.0), |(c, s), p| (c + (p.0 - mt) * (p.1 - mv), s + (p.0 - mt).powi(2)));
    if var == 0.0 { 0.0 } else { cov / var * 3600.0 }
}

/// Seconds of picture and of sound in a file, counted packet by packet:
/// FLV carries no duration per stream, and a growing file none at all.
/// AAC packs 1024 samples in each packet.
fn av_seconds(path: &str) -> Result<(f64, f64), String> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-count_packets", "-of", "json", "-show_entries"])
        .arg("stream=codec_type,sample_rate,avg_frame_rate,nb_read_packets")
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("ffprobe : {e}"))?;
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|e| format!("ffprobe {path} : {e}"))?;
    let number = |v: &Value| v.as_str().and_then(|s| s.parse::<f64>().ok());
    let (mut video, mut audio) = (None, None);
    for s in v["streams"].as_array().into_iter().flatten() {
        let packets = number(&s["nb_read_packets"]).unwrap_or(0.0);
        match s["codec_type"].as_str() {
            Some("video") if video.is_none() => {
                let rate = s["avg_frame_rate"].as_str().and_then(|r| {
                    let (a, b) = r.split_once('/')?;
                    Some(a.parse::<f64>().ok()? / b.parse::<f64>().ok()?)
                });
                video = rate.filter(|r| r.is_finite() && *r > 0.0).map(|r| packets / r);
            }
            Some("audio") if audio.is_none() => {
                audio = number(&s["sample_rate"]).filter(|r| *r > 0.0).map(|r| packets * 1024.0 / r);
            }
            _ => {}
        }
    }
    match (video, audio) {
        (Some(v), Some(a)) => Ok((v, a)),
        _ => Err(format!("{path} : image ou son absent")),
    }
}

fn clip(dir: &str, name: &str, picture: &str, tone: u32, seconds: u32) -> Result<String, String> {
    let picture = format!("{picture}=size=640x360:rate=30:duration={seconds}");
    let sound = format!("sine=frequency={tone}:duration={seconds}");
    let args = ["-f", "lavfi", "-i", &picture, "-f", "lavfi", "-i", &sound];
    let codecs = ["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-c:a", "aac"];
    make(&[&args[..], &codecs[..]].concat(), &format!("{dir}/{name}.mp4"))
}

/// Memory and processes, as the system sees them.
struct Machine {
    sys: sysinfo::System,
    pid: sysinfo::Pid,
}

struct Usage {
    private_mb: f64,
    working_set_mb: f64,
    handles: u32,
    children: u32,
    children_mb: f64,
}

impl Machine {
    fn new() -> Result<Self, String> {
        let pid = sysinfo::get_current_pid().map_err(|e| e.to_string())?;
        let sys = sysinfo::System::new().map_err(|e| e.to_string())?;
        Ok(Self { sys, pid })
    }

    fn usage(&mut self) -> Usage {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate};
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_memory(),
        );
        let mb = |b: u64| b as f64 / 1_048_576.0;
        let me = self.sys.process(self.pid);
        let rows: Vec<_> =
            self.sys.processes().iter().map(|(pid, p)| (*pid, p.parent(), p.start_time())).collect();
        // ffmpeg may run through a launcher and a browser is a tree of its
        // own: the whole family counts.
        let family = descendants(self.pid, &rows);
        let children_bytes: u64 =
            family.iter().filter_map(|pid| self.sys.process(*pid)).map(|p| p.virtual_memory()).sum();
        Usage {
            private_mb: me.map_or(0.0, |p| mb(p.virtual_memory())),
            working_set_mb: me.map_or(0.0, |p| mb(p.memory())),
            handles: handle_count(),
            children: family.len() as u32,
            children_mb: mb(children_bytes),
        }
    }
}

/// The processes below `root`, from `(pid, parent, start in seconds)` rows.
///
/// Windows only keeps the number of a process's parent, and gives that number
/// to a new process once the parent is gone. Every chain of parents ends at
/// one that is gone (wininit's, for one), so when one of the show's own
/// ffmpeg is handed that number, the whole chain down to this program reads
/// as that ffmpeg's family: the walk went round forever and a twenty-minute
/// show died asking for 64 GB. A child cannot start before its parent, and
/// no process is counted twice. Start 0 means the system would not say, as
/// for its own processes, which a show never starts.
fn descendants<P: Copy + Eq + Hash>(root: P, rows: &[(P, Option<P>, u64)]) -> Vec<P> {
    let born: HashMap<P, u64> = rows.iter().map(|r| (r.0, r.2)).collect();
    let mut by_parent: HashMap<P, Vec<(P, u64)>> = HashMap::new();
    for &(pid, parent, start) in rows {
        if let Some(parent) = parent {
            by_parent.entry(parent).or_default().push((pid, start));
        }
    }
    let mut seen = HashSet::from([root]);
    let (mut todo, mut found) = (vec![root], Vec::new());
    while let Some(pid) = todo.pop() {
        let parent_start = born.get(&pid).copied().unwrap_or(0);
        for &(child, start) in by_parent.get(&pid).into_iter().flatten() {
            if start != 0 && start >= parent_start && seen.insert(child) {
                found.push(child);
                todo.push(child);
            }
        }
    }
    found
}

#[cfg(windows)]
fn handle_count() -> u32 {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
    let mut n = 0;
    // SAFETY: the pseudo handle of this process is always valid, and `n`
    // outlives the call.
    unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut n) };
    n
}

#[cfg(not(windows))]
fn handle_count() -> u32 {
    0
}

struct Sample {
    t_s: f64,
    usage: Usage,
    cpu_self: u32,
    cpu_total: u32,
    ticks: u64,
    missed: u64,
    late_events: u64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    max_ms: f64,
    rec: Feed,
    stream: Feed,
    replay_frames: u64,
    replay_dropped: u64,
    launches: u64,
    step: String,
}

/// The moment of the show an encoder's file begins with.
fn file_start(e: &crate::output::Encoder) -> Option<Instant> {
    e.pictures().lock().unwrap().first()
}

/// What the engine handed to one kind of encoder, against the clock.
#[derive(Clone, Copy, Default)]
struct Feed {
    frames: u64,
    dropped: u64,
    video_s: f64,
    audio_s: f64,
    audio_lost_s: f64,
    wall_s: f64,
}

impl Feed {
    fn of(list: &[crate::output::Encoder], fps: f64) -> Self {
        let total = |f: fn(&crate::output::Encoder) -> u64| list.iter().map(f).sum::<u64>();
        let frames = total(|e| e.sent.load(Ordering::Relaxed));
        let rate = AUDIO_RATE as f64 * list.len().max(1) as f64;
        Self {
            frames,
            dropped: total(|e| e.dropped.load(Ordering::Relaxed)),
            video_s: frames as f64 / fps / list.len().max(1) as f64,
            audio_s: total(|e| e.audio_sent.load(Ordering::Relaxed)) as f64 / rate,
            audio_lost_s: total(|e| e.audio_dropped.load(Ordering::Relaxed)) as f64 / rate,
            wall_s: list.first().and_then(file_start).map_or(0.0, |b| b.elapsed().as_secs_f64()),
        }
    }

    fn csv(&self) -> String {
        format!(
            "{},{},{:.2},{:.2},{:.2},{:.2}",
            self.frames, self.dropped, self.video_s, self.audio_s, self.audio_lost_s, self.wall_s
        )
    }
}

const CSV_HEADER: &str = "t_s,private_mb,working_set_mb,handles,children,children_mb,cpu_self,cpu_total,\
ticks,missed,late_events,p50_ms,p95_ms,p99_ms,max_ms,\
rec_frames,rec_dropped,rec_video_s,rec_audio_s,rec_audio_lost_s,rec_wall_s,\
stream_frames,stream_dropped,stream_video_s,stream_audio_s,stream_audio_lost_s,stream_wall_s,\
replay_frames,replay_dropped,launches,step";

impl Sample {
    fn csv(&self) -> String {
        let u = &self.usage;
        format!(
            "{:.1},{:.1},{:.1},{},{},{:.1},{},{},{},{},{},{:.2},{:.2},{:.2},{:.2},{},{},{},{},{},\"{}\"",
            self.t_s,
            u.private_mb,
            u.working_set_mb,
            u.handles,
            u.children,
            u.children_mb,
            self.cpu_self,
            self.cpu_total,
            self.ticks,
            self.missed,
            self.late_events,
            self.p50_ms,
            self.p95_ms,
            self.p99_ms,
            self.max_ms,
            self.rec.csv(),
            self.stream.csv(),
            self.replay_frames,
            self.replay_dropped,
            self.launches,
            self.step.replace('"', "'"),
        )
    }
}

/// The inputs the show adds to the sample project.
struct Ids {
    cam_a: u64,
    cam_b: u64,
    list: u64,
    music: u64,
    page: u64,
}

/// A failure is kept once, with when it was first seen and how often.
#[derive(Default)]
struct Failures {
    list: Vec<(String, String, f64, u32)>,
}

impl Failures {
    fn add(&mut self, key: &str, t_s: f64, msg: String) {
        match self.list.iter_mut().find(|f| f.0 == key) {
            Some(f) => f.3 += 1,
            None => {
                println!("  [{}] problème : {msg}", clock(t_s));
                self.list.push((key.to_string(), msg, t_s, 1));
            }
        }
    }

    fn lines(&self) -> Vec<String> {
        self.list
            .iter()
            .map(
                |(_, msg, t, n)| {
                    if *n > 1 { format!("{msg} (à {}, {n} fois)", clock(*t)) } else { msg.clone() }
                },
            )
            .collect()
    }
}

fn clock(t_s: f64) -> String {
    let t = t_s as u64;
    format!("{:02}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60)
}

struct Show {
    engine: Arc<Engine>,
    dir: String,
    ids: Ids,
    started: Instant,
    step: usize,
    steps_done: u64,
    api_calls: u64,
    replays_played: u64,
    last_step: String,
    /// Completions of the replay input before the clip went on air.
    replay_check: Option<u64>,
    /// Files of replay sessions already closed, deleted once released.
    old_replay_files: Vec<String>,
    /// Pictures and drops of replay sessions already closed.
    replay_closed: (u64, u64),
}

impl Show {
    fn t(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    fn number(&self, id: u64) -> Result<usize, String> {
        let s = self.engine.state.lock().unwrap();
        s.project
            .inputs
            .iter()
            .position(|i| i.id == id)
            .map(|n| n + 1)
            .ok_or_else(|| format!("entrée {id} absente"))
    }

    fn api(&mut self, query: &str) -> Result<(), String> {
        self.api_calls += 1;
        let reply = http_get(&format!("/api/?{query}"))?;
        let status = reply.lines().next().unwrap_or_default();
        if status.contains("200 OK") { Ok(()) } else { Err(format!("API {query} : {status}")) }
    }

    fn tally(&self) -> Result<(), String> {
        let mut tcp = TcpStream::connect(("127.0.0.1", 8099)).map_err(|e| format!("port TCP : {e}"))?;
        tcp.write_all(b"TALLY\r\n").map_err(|e| e.to_string())?;
        let mut answer = [0u8; 256];
        let n = tcp.read(&mut answer).map_err(|e| format!("réponse TCP : {e}"))?;
        let text = String::from_utf8_lossy(&answer[..n]);
        if text.starts_with("TALLY OK") { Ok(()) } else { Err(format!("port TCP : {text}")) }
    }

    fn replay_input(&self) -> Option<u64> {
        self.engine.replay.lock().unwrap().input
    }

    /// One minute of replay recording per cycle: sessions start and stop as
    /// they do between segments of a show, and their files are deleted so a
    /// long run does not fill the disk.
    fn replay_session(&mut self) -> Result<(), String> {
        self.old_replay_files.retain(|f| std::fs::remove_file(f).is_err() && Path::new(f).exists());
        let folder = PathBuf::from(&self.dir).join("ralenti");
        self.engine.replay_start(&[self.ids.cam_a, 2], folder).map(|_| ())
    }

    fn replay_close(&mut self) {
        let r = self.engine.replay.lock().unwrap();
        for c in &r.cams {
            self.replay_closed.0 += c.encoder.sent.load(Ordering::Relaxed);
            self.replay_closed.1 += c.encoder.dropped.load(Ordering::Relaxed);
            self.old_replay_files.push(c.path.clone());
        }
        drop(r);
        self.engine.replay_stop();
    }

    /// One step of the show. Twelve make a cycle of a little over a minute
    /// that goes through cameras, scenes, titles, transitions, replay and
    /// remote control, mostly over HTTP as a control surface would.
    fn step(&mut self) -> Result<String, String> {
        let n = self.step % STEPS;
        let cycle = self.step / STEPS + 1;
        self.step += 1;
        self.steps_done += 1;
        let ids = (self.number(self.ids.cam_a)?, self.number(self.ids.cam_b)?, self.number(self.ids.list)?);
        let (cam_a, cam_b, list) = ids;
        let (music, page) = (self.number(self.ids.music)?, self.number(self.ids.page)?);
        Ok(match n {
            0 => {
                self.replay_session()?;
                self.api(&format!("Function=Fade&Duration=1000&Input={cam_a}"))?;
                "ralenti en route, Caméra A en fondu".into()
            }
            1 => {
                self.api(&format!("Function=SetText&Input=11&Value=Direct%20n%C2%B0{cycle}"))?;
                self.api("Function=OverlayInput1In&Input=11")?;
                "titre modifié et incrusté".into()
            }
            2 => {
                self.api("Function=Merge&Duration=800&Input=9")?;
                "scène 2 CAM en fusion".into()
            }
            3 => {
                self.api("Function=OverlayInput2In&Input=12")?;
                "bandeau défilant incrusté".into()
            }
            4 => {
                self.api(&format!("Function=Cube&Duration=600&Input={cam_b}"))?;
                "Caméra B en cube".into()
            }
            5 => {
                self.api("Function=OverlayInput1Out")?;
                self.api("Function=SetCountdown&Input=13&Value=20")?;
                self.api("Function=StartCountdown&Input=13")?;
                self.api("Function=OverlayInput3In&Input=13")?;
                "titre retiré, compte à rebours incrusté".into()
            }
            6 => {
                self.api(&format!("Function=CutDirect&Input={list}"))?;
                self.api(&format!("Function=NextItem&Input={list}"))?;
                "liste en coupe, élément suivant".into()
            }
            7 => {
                self.engine.replay_mark_last(4.0);
                let last = self.engine.replay.lock().unwrap().events.len().checked_sub(1);
                let input = last.and_then(|i| self.engine.replay_play(i)).ok_or("ralenti impossible")?;
                self.replay_check =
                    Some(self.engine.rt(input).map_or(0, |r| r.completions.load(Ordering::Relaxed)));
                self.api(&format!("Function=CutDirect&Input={}", self.number(input)?))?;
                "ralenti à l'antenne".into()
            }
            8 => {
                self.api(&format!("Function=Wipe&Duration=700&Input={page}"))?;
                "page web en volet".into()
            }
            9 => {
                // The clip lasts eight seconds at half speed: it had twelve.
                let before = self.replay_check.take();
                let done = self.replay_input().and_then(|id| self.engine.rt(id));
                let done = done.map_or(0, |r| r.completions.load(Ordering::Relaxed));
                if before.is_some_and(|b| done <= b) {
                    return Err("le ralenti n'a pas fini sa lecture".into());
                }
                self.replays_played += 1;
                self.api("Function=FadeToBlack")?;
                sleep(Duration::from_millis(1500));
                self.api("Function=FadeToBlack")?;
                "fondu au noir aller-retour".into()
            }
            10 => {
                self.tally()?;
                let xml = http_get("/api/")?;
                self.api_calls += 1;
                if !xml.contains(&format!("<active>{page}</active>")) {
                    return Err("l'état XML ne montre pas la page à l'antenne".into());
                }
                self.api("Function=OverlayInput2Out")?;
                self.api("Function=OverlayInput3Out")?;
                self.api("Function=StopCountdown&Input=13")?;
                "tally, état XML, incrustations retirées".into()
            }
            _ => {
                self.api("Function=PreviewInput&Input=7")?;
                self.api("Function=Transition1")?;
                self.api(&format!("Function=SetVolume&Input={music}&Value={}", 40 + cycle % 50))?;
                self.replay_close();
                "écran d'attente, volume musique, ralenti arrêté".into()
            }
        })
    }
}

/// What the previous sample saw, to turn running totals into windows.
struct Watch {
    machine: Machine,
    at: Instant,
    counts: Vec<u64>,
    late: u64,
    /// Picture count, unchanged samples, error samples, per input.
    inputs: HashMap<u64, (u64, u32, u32)>,
    late_since: Option<f64>,
    longest_late_s: f64,
}

impl Watch {
    fn sample(&mut self, show: &Show, failures: &mut Failures) -> Sample {
        let e = &show.engine;
        let t_s = show.t();
        let now = Instant::now();
        let window = now.duration_since(self.at).as_secs_f64();
        self.at = now;
        let counts = e.stats.tick_times.counts();
        let delta = minus(&counts, &self.counts);
        self.counts = counts;
        let ticks: u64 = delta.iter().sum();
        let fps = e.state.lock().unwrap().project.fps as f64;
        let missed = (fps * window - ticks as f64).max(0.0).round() as u64;
        let late = e.stats.late_ticks.load(Ordering::Relaxed);
        let late_events = late - self.late;
        self.late = late;

        if missed as f64 > LATE_WINDOW * fps * window {
            let since = *self.late_since.get_or_insert(t_s - window);
            self.longest_late_s = self.longest_late_s.max(t_s - since);
            if t_s - since >= SUSTAINED_LATE.as_secs_f64() {
                let msg = format!("le moteur prend du retard depuis {:.0} s", t_s - since);
                failures.add("late", t_s, msg);
            }
        } else {
            self.late_since = None;
        }

        let rec = Feed::of(&e.recorders.lock().unwrap(), fps);
        let stream = Feed::of(&e.streamers.lock().unwrap(), fps);
        let (replay_frames, replay_dropped) = {
            let r = e.replay.lock().unwrap();
            for c in &r.cams {
                if let Some(why) = c.encoder.failure() {
                    failures.add("replay-encoder", t_s, format!("enregistrement du ralenti arrêté : {why}"));
                }
            }
            r.cams.iter().fold(show.replay_closed, |(s, d), c| {
                (s + c.encoder.sent.load(Ordering::Relaxed), d + c.encoder.dropped.load(Ordering::Relaxed))
            })
        };
        if let Some(err) = e.output_error.lock().unwrap().take() {
            failures.add(&format!("output:{err}"), t_s, err);
        }
        if !e.is_recording() {
            failures.add("not-recording", t_s, "l'enregistrement s'est arrêté".into());
        }
        if !e.is_streaming() {
            failures.add("not-streaming", t_s, "la diffusion s'est arrêtée".into());
        }

        let inputs: Vec<(u64, String)> =
            e.state.lock().unwrap().project.inputs.iter().map(|i| (i.id, i.name.clone())).collect();
        let mut launches = 0;
        for (id, name) in inputs {
            let Some(rt) = e.rt(id) else { continue };
            launches += rt.launches.load(Ordering::Relaxed);
            let entry = self.inputs.entry(id).or_insert((rt.seq(), 0, 0));
            entry.2 = if rt.status().is_some() { entry.2 + 1 } else { 0 };
            if entry.2 >= STATUS_SAMPLES {
                let msg = format!("{name} : {}", rt.status().unwrap_or_default());
                failures.add(&format!("status:{id}"), t_s, msg);
            }
            // These always play: a picture that stops changing is a frozen source.
            let ids = &show.ids;
            if [ids.cam_a, ids.cam_b, ids.list, ids.page].contains(&id) {
                let seq = rt.seq();
                entry.1 = if seq == entry.0 { entry.1 + 1 } else { 0 };
                entry.0 = seq;
                if entry.1 >= FROZEN_SAMPLES && t_s >= SETTLE.as_secs_f64() {
                    failures.add(&format!("frozen:{id}"), t_s, format!("{name} : image figée"));
                }
                if id != ids.page && !rt.is_playing() {
                    failures.add(&format!("stopped:{id}"), t_s, format!("{name} : lecture arrêtée"));
                }
            }
        }

        Sample {
            t_s,
            usage: self.machine.usage(),
            cpu_self: e.stats.cpu_self.load(Ordering::Relaxed),
            cpu_total: e.stats.cpu_total.load(Ordering::Relaxed),
            ticks,
            missed,
            late_events,
            p50_ms: percentile(&delta, 0.50),
            p95_ms: percentile(&delta, 0.95),
            p99_ms: percentile(&delta, 0.99),
            max_ms: e.stats.tick_times.take_max_us() as f64 / 1000.0,
            rec,
            stream,
            replay_frames,
            replay_dropped,
            launches,
            step: show.last_step.clone(),
        }
    }
}

/// Recording pieces, measured and deleted as soon as the next one begins.
/// A cut falls at the same moment for picture and sound, so any drift lands
/// in the last piece: only the sum of them all is judged.
struct Segments {
    stem: String,
    next: usize,
    found: Vec<(String, f64, f64)>,
}

impl Segments {
    fn path(&self, n: usize) -> String {
        format!("{}-{n:03}.mp4", self.stem)
    }

    fn check(&mut self, t_s: f64, failures: &mut Failures, last: bool) {
        while Path::new(&self.path(self.next + 1)).exists()
            || (last && Path::new(&self.path(self.next)).exists())
        {
            let path = self.path(self.next);
            match av_seconds(&path) {
                Ok((v, a)) => {
                    self.found.push((path.clone(), v, a));
                    let _ = std::fs::remove_file(&path);
                }
                Err(e) => failures.add(&format!("segment:{}", self.next), t_s, e),
            }
            self.next += 1;
        }
    }
}

fn soak(dir: &str, length: Duration) -> Result<String, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let out = Path::new(dir);
    println!("Préparation des médias dans {dir}…");
    let cam_a = clip(dir, "camera-a", "testsrc2", 440, 30)?;
    let cam_b = clip(dir, "camera-b", "smptebars", 660, 20)?;
    let one = clip(dir, "liste-1", "testsrc", 550, 8)?;
    let two = clip(dir, "liste-2", "rgbtestsrc", 770, 6)?;
    let music =
        make(&["-f", "lavfi", "-i", "sine=frequency=330:duration=15"], &format!("{dir}/musique.wav"))?;

    let mut project = Project::sample();
    project.out_w = 1280;
    project.out_h = 720;
    let split_minutes = (length.as_secs() / 60 / 6).clamp(1, 5) as u32;
    project.records[0].folder = dir.to_string();
    project.records[0].video_kbps = 6000;
    project.records[0].split_minutes = split_minutes;
    project.records[1].enabled = false;
    project.streams[0].enabled = true;
    project.streams[0].url = STREAM_URL.into();
    project.streams[0].kbps = 2500;
    let next = project.inputs.iter().map(|i| i.id).max().unwrap_or(0) + 1;
    let ids = Ids { cam_a: next, cam_b: next + 1, list: next + 2, music: next + 3, page: next + 4 };
    let video = |path: &str, mark_in, mark_out| Kind::Video {
        path: path.into(),
        looped: true,
        mark_in,
        mark_out,
        rate: 1.0,
    };
    let item = |path: &str| ListItem { path: path.into(), enabled: true };
    let page = "data:text/html,<body style='margin:0;font:72px sans-serif;color:white'><div id=a \
        style='width:100vw;height:100vh'></div><script>let n=0;setInterval(()=>{a.style.background=\
        'rgb(40,'+(n++%25200+40)+',90)';a.textContent=new Date().toLocaleTimeString()},33)</script>";
    project.inputs.extend([
        // A plain loop never restarts its decoder; marks restart it each time.
        Input::new(ids.cam_a, "Caméra A", video(&cam_a, 0, 0)),
        Input::new(ids.cam_b, "Caméra B", video(&cam_b, 2000, 8000)),
        Input::new(
            ids.list,
            "Liste",
            Kind::List {
                items: vec![item(&one), item(&two)],
                auto_next: true,
                auto_first: false,
                looped: true,
                shuffle: false,
            },
        ),
        Input::new(ids.music, "Musique", Kind::AudioFile { path: music, looped: true }),
        Input::new(ids.page, "Page", Kind::Browser { url: page.into(), alpha: false }),
    ]);
    // They stand in for cameras and a music bed: they play on, on air or not.
    for input in project.inputs.iter_mut().filter(|i| i.id >= ids.cam_a) {
        input.auto_pause = false;
    }

    let mut csv = File::create(out.join("soak.csv")).map_err(|e| e.to_string())?;
    writeln!(csv, "{CSV_HEADER}").map_err(|e| e.to_string())?;
    let engine = Engine::start(project);
    if let Some(e) = engine.start_api(ApiConfig::default()) {
        engine.shutdown();
        return Err(e);
    }
    let received = out.join("diffusion.flv").to_string_lossy().into_owned();
    // It waits for the stream, which only starts once every source has shown
    // a picture: minutes on a loaded machine. Should the run stop before
    // that, `finish` ends it.
    let listen = ((4.0 * SOURCE_START_S + PAGE_START_S) as u32 + 60).to_string();
    let server = ffmpeg()
        .args(["-y", "-listen", "1", "-timeout", &listen, "-f", "flv", "-i", STREAM_URL, "-c", "copy"])
        .arg(&received)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string());
    let server = match server {
        Ok(s) => s,
        Err(e) => {
            engine.shutdown();
            return Err(e);
        }
    };
    let result = show(&engine, dir, ids, length, &mut csv);
    finish(engine, server, &received, result, dir, length)
}

struct Run {
    show_s: f64,
    samples: Vec<Sample>,
    failures: Failures,
    segments: Segments,
    record_began: Option<Instant>,
    stream_began: Option<Instant>,
    warmup_counts: Vec<u64>,
    end_counts: Vec<u64>,
    warmup_s: f64,
    launches: Vec<(String, u64, Option<String>)>,
    cam_a_launches: (u64, u64),
    longest_late_s: f64,
    show: (u64, u64, u64),
}

fn show(engine: &Arc<Engine>, dir: &str, ids: Ids, length: Duration, csv: &mut File) -> Result<Run, String> {
    for id in [ids.cam_a, ids.cam_b, ids.list, ids.music] {
        let rt = engine.rt(id).ok_or("entrée absente")?;
        until(SOURCE_START_S, "première image ou durée des médias", || {
            rt.seq() >= 1 || rt.dur_ms.load(Ordering::Relaxed) > 0
        })?;
        rt.playing.store(true, Ordering::Relaxed);
    }
    let page = engine.rt(ids.page).ok_or("page absente")?;
    // A browser starting on a loaded machine can take most of a minute. A page
    // that never shows fails the run, but everything else is still worth
    // measuring: a long run is costly to start again.
    let page_start = until(PAGE_START_S, "la page web", || page.seq() >= 1);
    let page_start = page_start.err().map(|e| format!("{e} : {:?}", page.status()));
    sleep(Duration::from_millis(700));
    engine.start_streaming()?;
    let first = engine.start_recording(dir)?;
    let stem = first.strip_suffix(".mp4").unwrap_or(&first).to_string();

    let warmup = WARMUP_MIN.min(length / 2).max(length / 10);
    let cam_a = ids.cam_a;
    let cam_a_launches = || engine.rt(cam_a).map_or(0, |r| r.launches.load(Ordering::Relaxed));
    let mut show = Show {
        engine: engine.clone(),
        dir: dir.to_string(),
        ids,
        started: Instant::now(),
        step: 0,
        steps_done: 0,
        api_calls: 0,
        replays_played: 0,
        last_step: "début".into(),
        replay_check: None,
        old_replay_files: Vec::new(),
        replay_closed: (0, 0),
    };
    let mut watch = Watch {
        machine: Machine::new()?,
        at: Instant::now(),
        counts: engine.stats.tick_times.counts(),
        late: engine.stats.late_ticks.load(Ordering::Relaxed),
        inputs: HashMap::new(),
        late_since: None,
        longest_late_s: 0.0,
    };
    let mut failures = Failures::default();
    if let Some(e) = page_start {
        failures.add("page-start", 0.0, e);
    }
    let mut segments = Segments { stem, next: 0, found: Vec::new() };
    let mut samples = Vec::new();
    let mut warmup_counts = None;
    let mut cam_a_at_warmup = 0;
    let (mut next_sample, mut next_step) = (Instant::now() + SAMPLE_EVERY, Instant::now() + STEP_EVERY);
    let mut next_report = 60.0;
    println!("Émission de {} lancée, relevés dans {dir}/soak.csv", clock(length.as_secs_f64()));
    while show.started.elapsed() < length {
        let now = Instant::now();
        if now >= next_step {
            next_step += STEP_EVERY;
            match show.step() {
                Ok(what) => show.last_step = what,
                Err(e) => {
                    show.last_step = format!("échec : {e}");
                    failures.add(&format!("step:{}", (show.step - 1) % STEPS), show.t(), e);
                }
            }
        }
        if now >= next_sample {
            next_sample += SAMPLE_EVERY;
            let s = watch.sample(&show, &mut failures);
            writeln!(csv, "{}", s.csv()).map_err(|e| e.to_string())?;
            let _ = csv.flush();
            segments.check(s.t_s, &mut failures, false);
            if warmup_counts.is_none() && s.t_s >= warmup.as_secs_f64() {
                warmup_counts = Some(watch.counts.clone());
                cam_a_at_warmup = cam_a_launches();
            }
            if s.t_s >= next_report {
                next_report += 60.0;
                println!(
                    "  [{}] mémoire privée {:.0} Mo, poignées {}, processus {}, images manquées {}, \
                     tick p99 {:.1} ms, max {:.1} ms, CPU machine {} %, enregistreur nourri de {:.1} s \
                     d'image et {:.1} s de son en {:.1} s",
                    clock(s.t_s),
                    s.usage.private_mb,
                    s.usage.handles,
                    s.usage.children,
                    s.missed,
                    s.p99_ms,
                    s.max_ms,
                    s.cpu_total,
                    s.rec.video_s,
                    s.rec.audio_s,
                    s.rec.wall_s,
                );
            }
            samples.push(s);
        }
        sleep(Duration::from_millis(100));
    }
    let end_counts = engine.stats.tick_times.counts();
    let record_began = engine.recorders.lock().unwrap().first().and_then(file_start);
    let stream_began = engine.streamers.lock().unwrap().first().and_then(file_start);
    show.replay_close();
    let inputs: Vec<(u64, String)> =
        engine.state.lock().unwrap().project.inputs.iter().map(|i| (i.id, i.name.clone())).collect();
    let launches = inputs
        .into_iter()
        .filter_map(|(id, name)| {
            engine.rt(id).map(|r| (name, r.launches.load(Ordering::Relaxed), r.status()))
        })
        .collect();
    let show_s = show.t();
    Ok(Run {
        show_s,
        warmup_s: warmup.as_secs_f64(),
        failures,
        segments,
        record_began,
        stream_began,
        warmup_counts: warmup_counts.unwrap_or_else(|| vec![0; TICK_BUCKETS]),
        end_counts,
        launches,
        cam_a_launches: (cam_a_at_warmup, cam_a_launches()),
        longest_late_s: watch.longest_late_s,
        show: (show.steps_done, show.api_calls, show.replays_played),
        samples,
    })
}

/// Samples at each end of the run over which the lowest number of child
/// processes is read. That number swings by a dozen within each cycle of
/// the show as sources restart and replay sessions come and go, so each end
/// spans several whole cycles: a tenth of a twenty-minute run missed the low
/// point and read 29 then 35 with no leak at all. A leaked process raises
/// every low point after it, so it still shows.
fn floor_span(samples: usize) -> usize {
    let cycles = (STEP_EVERY * (4 * STEPS as u32)).as_secs_f64() / SAMPLE_EVERY.as_secs_f64();
    (samples / 10).max(cycles as usize).min(samples / 2).max(1)
}

/// Measured against a limit: a row of the report.
fn trend(points: &[(f64, f64)], slope_max: f64, rise_max: f64) -> (f64, f64, bool) {
    let slope = slope_per_hour(points);
    let span_h = match (points.first(), points.last()) {
        (Some(a), Some(b)) => (b.0 - a.0) / 3600.0,
        _ => 0.0,
    };
    let rise = slope * span_h;
    (slope, rise, slope > slope_max && rise > rise_max)
}

fn finish(
    engine: Arc<Engine>,
    mut server: Child,
    received: &str,
    result: Result<Run, String>,
    dir: &str,
    length: Duration,
) -> Result<String, String> {
    let stopped = Instant::now();
    let fps = engine.state.lock().unwrap().project.fps;
    engine.shutdown();
    let deadline = Instant::now() + Duration::from_secs(30);
    while matches!(server.try_wait(), Ok(None)) && Instant::now() < deadline {
        sleep(Duration::from_millis(100));
    }
    let _ = server.kill();
    let _ = server.wait();
    let mut run = result?;
    // Replay files only matter while the show runs, and are large.
    let _ = std::fs::remove_dir_all(Path::new(dir).join("ralenti"));
    let t_end = run.show_s;
    run.segments.check(t_end, &mut run.failures, true);
    let failures = &mut run.failures;

    // Memory, handles and processes after warm-up.
    let steady: Vec<&Sample> = run.samples.iter().filter(|s| s.t_s >= run.warmup_s).collect();
    let points = |f: fn(&Sample) -> f64| steady.iter().map(|s| (s.t_s, f(s))).collect::<Vec<_>>();
    let enough = steady.len() >= 12;
    let (mem_slope, mem_rise, mem_bad) = trend(&points(|s| s.usage.private_mb), MEM_SLOPE_MB_H, MEM_RISE_MB);
    let (h_slope, h_rise, h_bad) = trend(&points(|s| s.usage.handles as f64), HANDLE_SLOPE_H, HANDLE_RISE);
    if enough && mem_bad {
        let msg = format!("la mémoire monte de {mem_slope:.0} Mo/h ({mem_rise:+.0} Mo sur la période)");
        failures.add("memory", t_end, msg);
    }
    if enough && h_bad {
        failures.add("handles", t_end, format!("les poignées montent de {h_slope:.0}/h ({h_rise:+.0})"));
    }
    let span = floor_span(steady.len());
    let floor = |list: &[&Sample]| list.iter().map(|s| s.usage.children as i64).min().unwrap_or(0);
    let (children_start, children_end) = if steady.is_empty() {
        (0, 0)
    } else {
        (floor(&steady[..span]), floor(&steady[steady.len() - span..]))
    };
    if enough && children_end > children_start + CHILDREN_RISE {
        let msg = format!("processus enfants : {children_start} puis {children_end} au plus bas");
        failures.add("children", t_end, msg);
    }

    // Timing once warmed up.
    let steady_counts = minus(&run.end_counts, &run.warmup_counts);
    let ticks: u64 = steady_counts.iter().sum();
    let missed: u64 = steady.iter().map(|s| s.missed).sum();
    let expected: u64 = steady.iter().map(|s| s.ticks + s.missed).sum();
    let missed_ratio = if expected > 0 { missed as f64 / expected as f64 } else { 0.0 };
    if missed_ratio > MISSED_MAX {
        failures.add("missed", t_end, format!("{:.2} % des images manquées", missed_ratio * 100.0));
    }
    let max_ms = steady.iter().map(|s| s.max_ms).fold(0.0, f64::max);
    let late_events: u64 = steady.iter().map(|s| s.late_events).sum();
    // Timing means little without the load of the machine beside it.
    let mean =
        |f: fn(&Sample) -> u32| steady.iter().map(|s| f(s) as f64).sum::<f64>() / steady.len().max(1) as f64;
    let (cpu_total, cpu_self) = (mean(|s| s.cpu_total), mean(|s| s.cpu_self));

    // Encoders: pictures and sound refused, and how what they were fed
    // drifted from the clock and from each other.
    let last = run.samples.last();
    let ratio = |d: u64, s: u64| if s + d > 0 { d as f64 / (s + d) as f64 } else { 0.0 };
    let encoders = [
        ("recording", last.map_or((0, 0), |s| (s.rec.frames, s.rec.dropped))),
        ("stream", last.map_or((0, 0), |s| (s.stream.frames, s.stream.dropped))),
        ("replay", last.map_or((0, 0), |s| (s.replay_frames, s.replay_dropped))),
    ];
    for (name, (sent, dropped)) in encoders {
        if ratio(dropped, sent) > DROPPED_MAX {
            let msg = format!("{name} : {dropped} images perdues sur {}", sent + dropped);
            failures.add(&format!("dropped:{name}"), t_end, msg);
        }
    }
    let mut feeds = serde_json::Map::new();
    let picks: [(&str, fn(&Sample) -> Feed); 2] = [("recording", |s| s.rec), ("stream", |s| s.stream)];
    for (name, feed) in picks {
        let end = last.map(feed).unwrap_or_default();
        let lost = end.audio_lost_s / (end.audio_s + end.audio_lost_s).max(1.0);
        if lost > DROPPED_MAX {
            failures.add(
                &format!("audio:{name}"),
                t_end,
                format!("{name} : {:.1} s de son refusées", end.audio_lost_s),
            );
        }
        let sound_behind =
            steady.iter().map(|s| (s.t_s, feed(s).video_s - feed(s).audio_s)).collect::<Vec<_>>();
        let picture_behind =
            steady.iter().map(|s| (s.t_s, feed(s).wall_s - feed(s).video_s)).collect::<Vec<_>>();
        feeds.insert(
            name.into(),
            json!({
                "video_s": end.video_s,
                "audio_s": end.audio_s,
                "audio_lost_s": end.audio_lost_s,
                "wall_s": end.wall_s,
                "picture_ahead_of_sound_s_per_h": slope_per_hour(&sound_behind),
                "picture_behind_clock_s_per_h": slope_per_hour(&picture_behind),
            }),
        );
    }

    // Files against the time they covered.
    let length_ok =
        |media: f64, wall: f64| (media - wall).abs() <= (wall * LENGTH_SKEW).max(LENGTH_SKEW_MIN_S);
    let rec_video: f64 = run.segments.found.iter().map(|s| s.1).sum();
    let rec_audio: f64 = run.segments.found.iter().map(|s| s.2).sum();
    let rec_wall = run.record_began.map_or(0.0, |b| stopped.duration_since(b).as_secs_f64());
    if !length_ok(rec_video, rec_wall)
        || !length_ok(rec_audio, rec_wall)
        || (rec_video - rec_audio).abs() > AV_SKEW_MAX_S
    {
        let msg =
            format!("enregistrement : image {rec_video:.1} s, son {rec_audio:.1} s pour {rec_wall:.1} s");
        failures.add("record-length", t_end, msg);
    }
    let stream_wall = run.stream_began.map_or(0.0, |b| stopped.duration_since(b).as_secs_f64());
    let (stream_video, stream_audio) = match av_seconds(received) {
        Ok(d) => d,
        Err(e) => {
            failures.add("stream-file", t_end, e);
            (0.0, 0.0)
        }
    };
    if (stream_video - stream_audio).abs() > AV_SKEW_MAX_S || !length_ok(stream_video, stream_wall) {
        let msg = format!(
            "diffusion reçue : image {stream_video:.1} s, son {stream_audio:.1} s pour {stream_wall:.1} s"
        );
        failures.add("stream-length", t_end, msg);
    }
    let (cam_a_warm, cam_a_end) = run.cam_a_launches;
    if cam_a_end > cam_a_warm {
        let msg =
            format!("Caméra A relancée {} fois alors qu'elle boucle sans coupure", cam_a_end - cam_a_warm);
        failures.add("cam-a-restarts", t_end, msg);
    }

    let first = steady.first();
    let report = json!({
        "length_s": length.as_secs(),
        "show_s": t_end,
        "warmup_s": run.warmup_s,
        "passed": run.failures.list.is_empty(),
        "failures": run.failures.lines(),
        "thresholds": {
            "memory_slope_mb_per_h": MEM_SLOPE_MB_H,
            "memory_rise_mb": MEM_RISE_MB,
            "handle_slope_per_h": HANDLE_SLOPE_H,
            "handle_rise": HANDLE_RISE,
            "children_rise": CHILDREN_RISE,
            "late_window_share": LATE_WINDOW,
            "sustained_late_s": SUSTAINED_LATE.as_secs(),
            "missed_max_share": MISSED_MAX,
            "dropped_max_share": DROPPED_MAX,
            "av_skew_max_s": AV_SKEW_MAX_S,
            "length_skew_share": LENGTH_SKEW,
            "length_skew_min_s": LENGTH_SKEW_MIN_S,
        },
        "memory": {
            "private_after_warmup_mb": first.map(|s| s.usage.private_mb),
            "private_end_mb": last.map(|s| s.usage.private_mb),
            "private_peak_mb": run.samples.iter().map(|s| s.usage.private_mb).fold(0.0, f64::max),
            "working_set_end_mb": last.map(|s| s.usage.working_set_mb),
            "slope_mb_per_h": mem_slope,
            "rise_mb": mem_rise,
            "children_private_end_mb": last.map(|s| s.usage.children_mb),
        },
        "handles": {
            "after_warmup": first.map(|s| s.usage.handles),
            "end": last.map(|s| s.usage.handles),
            "slope_per_h": h_slope,
            "rise": h_rise,
        },
        "children": { "floor_start": children_start, "floor_end": children_end },
        "machine": { "cpu_total_mean_percent": cpu_total, "cpu_self_mean_percent": cpu_self },
        "timing": {
            "fps": fps,
            "ticks": ticks,
            "missed": missed,
            "missed_share": missed_ratio,
            "late_events": late_events,
            "longest_late_s": run.longest_late_s,
            "tick_p50_ms": percentile(&steady_counts, 0.50),
            "tick_p95_ms": percentile(&steady_counts, 0.95),
            "tick_p99_ms": percentile(&steady_counts, 0.99),
            "tick_p999_ms": percentile(&steady_counts, 0.999),
            "tick_max_ms": max_ms,
        },
        "encoders": encoders.iter().map(|(name, (sent, dropped))| {
            (name.to_string(), json!({ "frames": sent, "dropped": dropped }))
        }).collect::<serde_json::Map<_, _>>(),
        "fed_to_encoders": feeds,
        "recording": {
            "segments": run.segments.found.iter().map(|(p, v, a)| json!({
                "file": Path::new(p).file_name().map(|f| f.to_string_lossy().into_owned()),
                "video_s": v,
                "audio_s": a,
            })).collect::<Vec<_>>(),
            "video_s": rec_video,
            "audio_s": rec_audio,
            "wall_s": rec_wall,
        },
        "stream": { "video_s": stream_video, "audio_s": stream_audio, "wall_s": stream_wall },
        "sources": run.launches.iter().map(|(name, n, status)| json!({
            "name": name, "launches": n, "status": status,
        })).collect::<Vec<_>>(),
        "show": { "steps": run.show.0, "api_calls": run.show.1, "replays": run.show.2 },
    });
    let text = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    std::fs::write(Path::new(dir).join("soak.json"), text).map_err(|e| e.to_string())?;

    let summary = format!(
        "{} d'émission, mémoire privée {:.0} → {:.0} Mo ({mem_slope:+.0} Mo/h), poignées {} → {}, \
         ticks p50 {:.2} / p99 {:.2} / max {max_ms:.1} ms, images manquées {missed} ({:.3} %) \
         avec le processeur occupé à {cpu_total:.0} % (dont {cpu_self:.0} % pour LiveCraft), \
         enregistrement image {rec_video:.1} s / son {rec_audio:.1} s pour {rec_wall:.1} s, \
         diffusion image {stream_video:.1} s / son {stream_audio:.1} s pour {stream_wall:.1} s",
        clock(t_end),
        first.map_or(0.0, |s| s.usage.private_mb),
        last.map_or(0.0, |s| s.usage.private_mb),
        first.map_or(0, |s| s.usage.handles),
        last.map_or(0, |s| s.usage.handles),
        percentile(&steady_counts, 0.50),
        percentile(&steady_counts, 0.99),
        missed_ratio * 100.0,
    );
    if run.failures.list.is_empty() {
        Ok(summary)
    } else {
        Err(format!("{summary}\n  - {}", run.failures.lines().join("\n  - ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_read_as_people_write_them() {
        assert_eq!(parse_length("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_length("45m"), Some(Duration::from_secs(2700)));
        assert_eq!(parse_length("45"), Some(Duration::from_secs(2700)));
        assert_eq!(parse_length("1.5h"), Some(Duration::from_secs(5400)));
        assert_eq!(parse_length("10 minutes"), None);
        assert_eq!(parse_length("0s"), None);
    }

    #[test]
    fn percentiles_never_flatter() {
        let mut counts = vec![0; TICK_BUCKETS];
        counts[3] = 98; // 0.75 to 1 ms
        counts[200] = 2; // 50 to 50.25 ms
        assert_eq!(percentile(&counts, 0.5), 1.0);
        assert_eq!(percentile(&counts, 0.98), 1.0);
        assert_eq!(percentile(&counts, 0.99), 50.25);
        assert_eq!(percentile(&[0; 4], 0.99), 0.0);
    }

    #[test]
    fn a_reused_process_number_never_makes_the_family_go_round() {
        // As on Windows: wininit's parent (1424) is long gone, and the system
        // will not say when its own processes started. Once one of the
        // show's ffmpeg is handed 1424, wininit reads as its child and the
        // chain of parents leads back down to this program.
        let rows = [
            (1536, Some(1424), 0), // wininit
            (1624, Some(1536), 0), // services
            (5788, Some(1624), 0), // the WMI host that started the launcher
            (20, Some(5788), 300), // launcher
            (10, Some(20), 301),   // this program
            (1424, Some(10), 900), // ffmpeg handed the gone number
            (11, Some(10), 400),   // browser
            (12, Some(11), 400),   // its renderer, started in the same second
            (13, Some(12), 350),   // older than its "parent": a stranger
            (14, Some(11), 0),     // will not say when it started
        ];
        let mut family = descendants(10, &rows);
        family.sort();
        assert_eq!(family, [11, 12, 1424]);
        // A launcher whose own parent's number went to an ffmpeg is older
        // than that ffmpeg, so it is not taken for its child either.
        let rows = [(20, Some(7), 300), (10, Some(20), 301), (7, Some(10), 900)];
        assert_eq!(descendants(10, &rows), [7]);
    }

    #[test]
    fn the_process_floor_is_read_over_whole_cycles() {
        // Twenty minutes after two of warm-up: 216 samples, of which a tenth
        // is a cycle and a half. Four cycles of 72 s are 57 samples.
        assert_eq!(floor_span(216), 57);
        assert_eq!(floor_span(1000), 100);
        assert_eq!(floor_span(30), 15);
        assert_eq!(floor_span(1), 1);
        // The 20-minute run of 12 Nov, lowest count per minute after warm-up:
        // no leak, but the last 21 samples never reached a low point.
        let lows = [30, 29, 35, 32, 32, 35, 32, 29, 32, 35, 29, 32, 30, 29, 32, 31, 32, 35];
        let samples: Vec<i64> =
            lows.iter().flat_map(|&low| [low, low + 6, low + 9, low + 6].repeat(3)).collect();
        let rise = |list: &[i64], span: usize| {
            list[list.len() - span..].iter().min().unwrap() - list[..span].iter().min().unwrap()
        };
        assert!(rise(&samples, samples.len() / 10) > CHILDREN_RISE, "a tenth of the run reads a leak");
        assert!(rise(&samples, floor_span(samples.len())) <= CHILDREN_RISE);
        // One process leaked every few minutes does show.
        let leaking: Vec<i64> = samples.iter().enumerate().map(|(i, n)| n + (i / 40) as i64).collect();
        assert!(rise(&leaking, floor_span(leaking.len())) > CHILDREN_RISE);
    }

    #[test]
    fn a_steady_climb_is_a_leak_and_noise_is_not() {
        let leak: Vec<(f64, f64)> = (0..120).map(|i| (i as f64 * 30.0, 500.0 + i as f64)).collect();
        let (slope, rise, bad) = trend(&leak, MEM_SLOPE_MB_H, MEM_RISE_MB);
        assert!((slope - 120.0).abs() < 0.01, "{slope}");
        assert!(bad && rise > 100.0);
        let noise: Vec<(f64, f64)> =
            (0..120).map(|i| (i as f64 * 30.0, 500.0 + if i % 2 == 0 { 30.0 } else { -30.0 })).collect();
        assert!(!trend(&noise, MEM_SLOPE_MB_H, MEM_RISE_MB).2);
    }
}
