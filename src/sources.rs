//! Live inputs. Each runs on its own thread and publishes its latest frame;
//! nothing waits on a slow source. Files, cameras, the screen and microphones
//! are decoded by an ffmpeg process that already scales to the output size.

use crate::frame::Frame;
use crate::model::{Input, Kind, ListItem, TitleObject, TitleStyle};
use crate::titles::{self, Stroke};
use crate::{playback, text};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const AUDIO_RATE: usize = 48_000;
pub(crate) fn ffmpeg_missing() -> &'static str {
    t!("sources.ffmpeg_missing")
}

/// Bounded queue of interleaved stereo samples.
pub struct Ring {
    q: Mutex<VecDeque<f32>>,
    cap: usize,
}

impl Ring {
    pub fn new(seconds: f32) -> Self {
        let cap = (AUDIO_RATE as f32 * 2.0 * seconds) as usize;
        Self { q: Mutex::new(VecDeque::with_capacity(cap)), cap }
    }

    pub fn room(&self) -> usize {
        self.cap.saturating_sub(self.q.lock().unwrap().len())
    }

    /// Adds samples, dropping the oldest if the reader fell behind.
    pub fn push(&self, s: &[f32]) {
        let mut q = self.q.lock().unwrap();
        q.extend(s.iter().copied());
        let extra = q.len().saturating_sub(self.cap);
        q.drain(..extra);
    }

    /// Fills `out`, padding with silence on underrun.
    pub fn pull(&self, out: &mut [f32]) {
        let mut q = self.q.lock().unwrap();
        for v in out.iter_mut() {
            *v = q.pop_front().unwrap_or(0.0);
        }
    }

    pub fn clear(&self) {
        self.q.lock().unwrap().clear();
    }

    pub fn is_empty(&self) -> bool {
        self.q.lock().unwrap().is_empty()
    }
}

/// A replay clip to play, set by the engine and read by the Replay input.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplayPlay {
    /// One per camera shown, up to four.
    pub clips: Vec<ReplayClip>,
    /// Seconds of the show from the first picture to the out mark.
    pub span_s: f64,
    pub speed: f32,
}

/// Part of one recording, and when each of its pictures was taken.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplayClip {
    pub file: String,
    /// Where reading starts and stops, in seconds from the file's first picture.
    pub from_s: f64,
    pub to_s: f64,
    /// When each picture read was taken, in seconds from the start of the
    /// moment. Empty when the recording kept no log: pictures then follow at
    /// the frame rate, as they did in the file.
    pub times: Vec<f64>,
}

impl ReplayClip {
    /// When picture `n` of the clip is due, or None past its last one.
    pub fn due(&self, n: usize, fps: u32) -> Option<f64> {
        if self.times.is_empty() { Some(n as f64 / fps.max(1) as f64) } else { self.times.get(n).copied() }
    }
}

/// Running state of one input, shared with the mixer and the UI.
pub struct InputRt {
    pub frame: Mutex<Option<Arc<Frame>>>,
    pub seq: AtomicU64,
    pub stop: AtomicBool,
    pub status: Mutex<Option<String>>,
    pub playing: AtomicBool,
    pub pos_ms: AtomicU64,
    pub dur_ms: AtomicU64,
    pub seek_ms: AtomicI64,
    /// Asks a List or Photos input to jump to an item.
    pub select: AtomicI64,
    pub item: AtomicU64,
    pub items: AtomicU64,
    /// On air, in preview or going to an output. A source nobody is watching
    /// only has its card to feed, and can do far less.
    pub showing: AtomicBool,
    /// Where playback stops short of the end of the file; 0 is the end.
    pub end_ms: AtomicU64,
    /// Counts each time playback or a countdown reached its end.
    pub completions: AtomicU64,
    pub audio: Option<Arc<Ring>>,
    pub peak: AtomicU32,
    pub replay: Mutex<Option<ReplayPlay>>,
    pub strokes: Mutex<Vec<Stroke>>,
    pub strokes_gen: AtomicU64,
    children: Mutex<Vec<Child>>,
}

impl InputRt {
    pub(crate) fn new(with_audio: bool, playing: bool) -> Arc<Self> {
        Arc::new(Self {
            frame: Mutex::new(None),
            seq: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            status: Mutex::new(None),
            playing: AtomicBool::new(playing),
            pos_ms: AtomicU64::new(0),
            dur_ms: AtomicU64::new(0),
            seek_ms: AtomicI64::new(-1),
            select: AtomicI64::new(-1),
            item: AtomicU64::new(0),
            items: AtomicU64::new(0),
            showing: AtomicBool::new(true),
            end_ms: AtomicU64::new(0),
            completions: AtomicU64::new(0),
            audio: with_audio.then(|| Arc::new(Ring::new(1.0))),
            peak: AtomicU32::new(0),
            replay: Mutex::new(None),
            strokes: Mutex::new(Vec::new()),
            strokes_gen: AtomicU64::new(0),
            children: Mutex::new(Vec::new()),
        })
    }

    pub fn latest(&self) -> Option<Arc<Frame>> {
        self.frame.lock().unwrap().clone()
    }

    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    pub fn peak(&self) -> f32 {
        f32::from_bits(self.peak.load(Ordering::Relaxed))
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    pub fn status(&self) -> Option<String> {
        self.status.lock().unwrap().clone()
    }

    pub(crate) fn set_status(&self, s: impl Into<String>) {
        *self.status.lock().unwrap() = Some(s.into());
    }

    pub(crate) fn clear_status(&self) {
        *self.status.lock().unwrap() = None;
    }

    pub(crate) fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Returns the buffer it replaced when nobody else still uses it.
    pub fn publish(&self, f: Frame) -> Option<Frame> {
        let old = self.frame.lock().unwrap().replace(Arc::new(f));
        self.seq.fetch_add(1, Ordering::Release);
        old.and_then(|a| Arc::try_unwrap(a).ok())
    }

    /// Time left, up to the out point if there is one.
    pub fn remaining_ms(&self) -> u64 {
        let end = self.end_ms.load(Ordering::Relaxed);
        let end = if end > 0 { end } else { self.dur_ms.load(Ordering::Relaxed) };
        end.saturating_sub(self.pos_ms.load(Ordering::Relaxed))
    }

    /// Where this input stands, for a replacement source to carry on from.
    pub fn resume(&self) -> Resume {
        Resume {
            playing: self.is_playing(),
            pos_ms: self.pos_ms.load(Ordering::Relaxed),
            item: self.item.load(Ordering::Relaxed),
        }
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.kill_children();
    }

    pub(crate) fn adopt(&self, child: Child) {
        self.children.lock().unwrap().push(child);
    }

    pub(crate) fn kill_children(&self) {
        for mut c in self.children.lock().unwrap().drain(..) {
            thread::spawn(move || {
                end_process_tree(&mut c);
                let _ = c.wait();
            });
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Resume {
    pub playing: bool,
    pub pos_ms: u64,
    pub item: u64,
}

/// A child can have children of its own: ffmpeg is often started through a
/// launcher, and a browser is dozens of processes. Stopping only the first
/// would leave the real work running.
pub fn end_process_tree(child: &mut Child) {
    let mut kill = Command::new("taskkill");
    kill.args(["/T", "/F", "/PID", &child.id().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        kill.creation_flags(0x0800_0000);
    }
    let _ = kill.status();
    let _ = child.kill();
}

/// Everything started from now on ends with this program, however it stops,
/// so nothing is left running in the background after a crash.
pub fn tie_children_to_this_process() {
    use windows_sys::Win32::System::JobObjects::*;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: the job handle is checked, the information block is sized for the
    // call, and the handle stays open on purpose until this process ends.
    ONCE.call_once(|| unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of_val(&info) as u32,
        );
        AssignProcessToJobObject(job, GetCurrentProcess());
    });
}

pub fn ffmpeg() -> Command {
    let mut c = Command::new("ffmpeg");
    c.args(["-hide_banner", "-nostats"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

pub(crate) fn fit_filter(w: usize, h: usize, fps: u32) -> String {
    format!("fps={fps},{}", fit_scale(w, h))
}

/// Fits a picture into `w`×`h` without touching its timing.
pub(crate) fn fit_scale(w: usize, h: usize) -> String {
    format!(
        "scale={w}:{h}:force_original_aspect_ratio=decrease:flags=fast_bilinear,\
         format=rgba,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black@0"
    )
}

pub fn start(input: &Input, w: usize, h: usize, fps: u32) -> Arc<InputRt> {
    start_seeded(input, w, h, fps, None, None)
}

/// Like `start`, showing `seed` until the source has its first picture, and
/// carrying on from `resume` when it replaces a source that was running.
pub fn start_seeded(
    input: &Input,
    w: usize,
    h: usize,
    fps: u32,
    seed: Option<Arc<Frame>>,
    resume: Option<Resume>,
) -> Arc<InputRt> {
    let has_audio = input.kind.has_audio();
    // Files and countdowns wait until something starts them.
    let countdown = matches!(input.kind, Kind::Title { style: TitleStyle::Countdown, .. });
    let waits = input.kind.has_timeline() || countdown;
    let rt = InputRt::new(has_audio, !waits);
    *rt.frame.lock().unwrap() = seed;
    if let (Some(r), true) = (resume, waits) {
        rt.playing.store(r.playing, Ordering::Relaxed);
        rt.pos_ms.store(r.pos_ms, Ordering::Relaxed);
        rt.item.store(r.item, Ordering::Relaxed);
    }
    let kind = input.kind.clone();
    let worker = rt.clone();
    thread::spawn(move || run(worker, kind, w, h, fps));
    rt
}

fn run(rt: Arc<InputRt>, kind: Kind, w: usize, h: usize, fps: u32) {
    match kind {
        Kind::Bars => {
            rt.publish(titles::test_card(w, h));
        }
        Kind::Solid { color } => {
            rt.publish(Frame::solid(w, h, color));
        }
        // These are built by the mixer, which sees all the inputs.
        Kind::Scene | Kind::Mix { .. } => {}
        Kind::Tone => run_tone(&rt),
        Kind::SystemAudio => run_system_audio(&rt),
        Kind::Motion => run_motion(&rt, w, h, fps),
        Kind::Clock => run_clock(&rt, w, h),
        Kind::Image { path } => match titles::load_image(&path, w, h) {
            Ok(f) => {
                rt.publish(f);
            }
            Err(e) => rt.set_status(t!("sources.unreadable_image", error = e)),
        },
        Kind::Photos { folder, seconds } => run_photos(&rt, &folder, seconds, w, h),
        Kind::Title { text, sub, style, color, accent, ticker_speed, countdown_ms, font, size, .. } => {
            match style {
                TitleStyle::Ticker => run_ticker(&rt, &text, color, accent, ticker_speed, w, h, fps),
                TitleStyle::Countdown => run_countdown(&rt, countdown_ms, color, w, h),
                TitleStyle::LowerThird => {
                    rt.publish(titles::lower_third(&text, &sub, color, accent, w, h));
                }
                TitleStyle::Centered => {
                    rt.publish(titles::centered(&text, &sub, color, &font, size, w, h));
                }
            }
        }
        Kind::Designed { objects, .. } => run_designed(&rt, &objects, w, h),
        Kind::Video { path, looped, mark_in, mark_out, rate } => {
            rt.end_ms.store(mark_out, Ordering::Relaxed);
            let spec = playback::Spec::file(path, looped, mark_in, mark_out, rate, true);
            playback::run_file(&rt, spec, w, h, fps);
        }
        Kind::ImageSequence { first, fps: seq_fps, looped } => {
            match playback::Spec::sequence(&first, seq_fps, looped) {
                Some(spec) => playback::run_file(&rt, spec, w, h, fps),
                None => rt.set_status(t!("sources.sequence_needs_number")),
            }
        }
        Kind::AudioFile { path, looped } => playback::run_audio_file(&rt, &path, looped),
        Kind::List { items, auto_next, looped, shuffle, .. } => {
            let enabled: Vec<ListItem> = items.into_iter().filter(|i| i.enabled).collect();
            playback::run_list(&rt, enabled, auto_next, looped, shuffle, w, h, fps);
        }
        Kind::Camera { device, .. } => {
            let args = vec![
                "-f".into(),
                "dshow".into(),
                "-rtbufsize".into(),
                "128M".into(),
                "-i".into(),
                format!("video={device}"),
            ];
            run_capture(&rt, args, w, h, fps);
        }
        Kind::Network { url, buffer_ms } => {
            let mut args: Vec<String> = Vec::new();
            if url.starts_with("rtsp://") {
                args.extend(["-rtsp_transport".into(), "tcp".into()]);
            }
            if buffer_ms == 0 {
                args.extend(["-fflags".into(), "nobuffer".into(), "-flags".into(), "low_delay".into()]);
            }
            args.extend(["-i".into(), url]);
            run_capture(&rt, args, w, h, fps);
        }
        Kind::Screen => {
            let args = vec![
                "-f".into(),
                "gdigrab".into(),
                "-framerate".into(),
                fps.to_string(),
                "-i".into(),
                "desktop".into(),
            ];
            run_capture(&rt, args, w, h, fps);
        }
        Kind::Microphone { device } => run_microphone(&rt, &device),
        Kind::Replay => playback::run_replay(&rt, w, h, fps),
        Kind::Browser { url, alpha } => crate::browser::run_browser(&rt, &url, alpha, w, h, fps),
        Kind::Telestrator { .. } => run_telestrator(&rt, w, h),
    }
}

/// A steady 1 kHz tone at -20 dB, for checking levels.
fn run_tone(rt: &InputRt) {
    let ring = rt.audio.as_ref().unwrap();
    let step = 1000.0 * std::f32::consts::TAU / AUDIO_RATE as f32;
    let mut phase = 0f32;
    let mut block = [0f32; 1920];
    while !rt.stopped() {
        if ring.room() < block.len() {
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        for pair in block.as_chunks_mut::<2>().0 {
            let v = phase.sin() * 0.1;
            pair[0] = v;
            pair[1] = v;
            phase = (phase + step) % std::f32::consts::TAU;
        }
        ring.push(&block);
    }
}

/// Everything the computer plays, taken from the default output device.
fn run_system_audio(rt: &Arc<InputRt>) {
    let opened = cpal::default_host().default_output_device().and_then(|device| {
        let config: cpal::StreamConfig = device.default_output_config().ok()?.into();
        let channels = config.channels as usize;
        let step = config.sample_rate as f64 / AUDIO_RATE as f64;
        let ring = rt.audio.clone()?;
        let mut phase = 0f64;
        let stream = device
            .build_input_stream(
                config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let mut out = Vec::with_capacity(data.len() * 2 / channels.max(1));
                    let frames = data.len() / channels.max(1);
                    while (phase as usize) < frames {
                        let i = phase as usize * channels;
                        out.push(data[i]);
                        out.push(data[i + (channels > 1) as usize]);
                        phase += step;
                    }
                    phase -= frames as f64;
                    ring.push(&out);
                },
                |_| {},
                None,
            )
            .ok()?;
        stream.play().ok()?;
        Some(stream)
    });
    if opened.is_none() {
        rt.set_status(t!("sources.system_audio_unavailable"));
    }
    while !rt.stopped() {
        thread::sleep(Duration::from_millis(100));
    }
}

fn run_motion(rt: &InputRt, w: usize, h: usize, fps: u32) {
    let mut base = Frame::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let (u, v) = (x as f32 / w as f32, y as f32 / h as f32);
            base.data[i] = (14.0 + 26.0 * u) as u8;
            base.data[i + 1] = (42.0 + 34.0 * (1.0 - v)) as u8;
            base.data[i + 2] = (42.0 + 30.0 * v) as u8;
            base.data[i + 3] = 255;
        }
    }
    let t0 = Instant::now();
    let period = Duration::from_secs_f32(2.0 / fps as f32);
    let mut buf = Some(base.clone());
    while !rt.stopped() {
        let mut f = buf.take().unwrap_or_else(|| base.clone());
        f.copy_from(&base);
        let secs = t0.elapsed().as_secs_f32();
        let band_w = w / 10;
        let span = (w + band_w) as f32;
        let bx = (secs * span / 4.0) % span - band_w as f32;
        // A faint full-height sweep shows the picture is alive.
        let (x0, x1) = (bx.max(0.0) as usize, ((bx + band_w as f32).max(0.0) as usize).min(w));
        for y in 0..h {
            for c in &mut f.data[(y * w + x0) * 4..(y * w + x1) * 4] {
                *c = c.saturating_add(9);
            }
            for x in x0..x1 {
                f.data[(y * w + x) * 4 + 3] = 255;
            }
        }
        let stamp = format!("{:02}:{:02}", (secs / 60.0) as u32, secs as u32 % 60);
        let px = h as f32 * 0.04;
        let tw = text::measure(&stamp, px);
        text::draw(&mut f, &stamp, w as f32 - tw - px, h as f32 - px * 2.0, px, [150, 190, 190], false);
        buf = rt.publish(f);
        thread::sleep(period);
    }
}

fn run_clock(rt: &InputRt, w: usize, h: usize) {
    let mut shown = String::new();
    while !rt.stopped() {
        let now = chrono::Local::now().format("%H:%M:%S").to_string();
        if now != shown {
            let mut f = Frame::solid(w, h, [14, 16, 22]);
            let px = h as f32 * 0.26;
            let tw = text::measure(&now, px);
            text::draw(&mut f, &now, (w as f32 - tw) / 2.0, (h as f32 - px) / 2.2, px, [255, 255, 255], true);
            rt.publish(f);
            shown = now;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Orders names as a person would: img2 before img10.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let split = |s: &str| {
        let mut parts: Vec<(String, u64)> = Vec::new();
        let mut text = String::new();
        let mut digits = String::new();
        for c in s.to_lowercase().chars() {
            if c.is_ascii_digit() {
                digits.push(c);
            } else {
                if !digits.is_empty() {
                    parts.push((std::mem::take(&mut text), digits.parse().unwrap_or(u64::MAX)));
                    digits.clear();
                }
                text.push(c);
            }
        }
        let number = digits.parse().unwrap_or(0);
        parts.push((text, number));
        parts
    };
    split(a).cmp(&split(b))
}

fn run_photos(rt: &InputRt, folder: &str, seconds: f32, w: usize, h: usize) {
    let mut files: Vec<_> =
        std::fs::read_dir(folder).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    files.retain(|p: &std::path::PathBuf| {
        let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase());
        matches!(ext.as_deref(), Some("png" | "jpg" | "jpeg" | "webp" | "bmp"))
    });
    files.sort_by(|a, b| natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
    if files.is_empty() {
        rt.set_status(t!("sources.no_photos"));
        return;
    }
    rt.items.store(files.len() as u64, Ordering::Relaxed);
    let mut index = 0usize;
    let mut shown = usize::MAX;
    let mut since = Instant::now();
    while !rt.stopped() {
        let jump = rt.select.swap(-1, Ordering::Relaxed);
        if jump >= 0 {
            index = jump as usize % files.len();
            since = Instant::now();
        }
        if seconds > 0.0 && since.elapsed().as_secs_f32() >= seconds {
            index = (index + 1) % files.len();
            since = Instant::now();
        }
        if index != shown {
            shown = index;
            rt.item.store(index as u64, Ordering::Relaxed);
            match titles::load_image(&files[index].to_string_lossy(), w, h) {
                Ok(f) => {
                    rt.publish(f);
                }
                Err(e) => rt.set_status(t!("sources.unreadable_photo", error = e)),
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[allow(clippy::too_many_arguments)]
fn run_ticker(
    rt: &InputRt,
    msg: &str,
    color: [u8; 3],
    accent: [u8; 3],
    speed: f32,
    w: usize,
    h: usize,
    fps: u32,
) {
    let band = (h as f32 * 0.075) as usize;
    let px = band as f32 * 0.6;
    let gap = w as f32 * 0.25;
    let strip_w = ((text::measure(msg, px) + gap) as usize).max(w);
    let mut strip = Frame::solid(strip_w, band, accent);
    text::draw(&mut strip, msg, gap / 2.0, band as f32 * 0.14, px, color, true);
    let top = h - band - (h as f32 * 0.04) as usize;
    let per_second = speed.max(0.1) * fps as f32;
    let t0 = Instant::now();
    let period = Duration::from_secs_f32(1.0 / fps as f32);
    let mut buf = None;
    while !rt.stopped() {
        let mut f = buf.take().unwrap_or_else(|| Frame::new(w, h));
        let off = (t0.elapsed().as_secs_f32() * per_second) as usize % strip_w;
        for row in 0..band {
            let src = &strip.data[row * strip_w * 4..(row + 1) * strip_w * 4];
            let dst = &mut f.data[(top + row) * w * 4..(top + row + 1) * w * 4];
            let first = (strip_w - off).min(w);
            dst[..first * 4].copy_from_slice(&src[off * 4..(off + first) * 4]);
            dst[first * 4..].copy_from_slice(&src[..(w - first) * 4]);
        }
        buf = rt.publish(f);
        thread::sleep(period);
    }
}

/// Counts down while `playing`. `pos_ms` holds the elapsed time, `dur_ms` the
/// total, so remote control and triggers read it like a timeline.
fn run_countdown(rt: &InputRt, total_ms: u64, color: [u8; 3], w: usize, h: usize) {
    rt.dur_ms.store(total_ms, Ordering::Relaxed);
    let mut last = Instant::now();
    let mut shown = u64::MAX;
    let mut was_playing = false;
    while !rt.stopped() {
        let now = Instant::now();
        let dt = now.duration_since(last).as_millis() as u64;
        last = now;
        let dur = rt.dur_ms.load(Ordering::Relaxed);
        let seek = rt.seek_ms.swap(-1, Ordering::Relaxed);
        if seek >= 0 {
            rt.pos_ms.store((seek as u64).min(dur), Ordering::Relaxed);
        }
        let mut pos = rt.pos_ms.load(Ordering::Relaxed).min(dur);
        let playing = rt.is_playing();
        // Start on a finished countdown runs it again.
        if playing && !was_playing && pos >= dur {
            pos = 0;
        }
        was_playing = playing;
        if playing {
            pos = (pos + dt).min(dur);
            if pos >= dur {
                rt.playing.store(false, Ordering::Relaxed);
                was_playing = false;
                rt.completions.fetch_add(1, Ordering::Relaxed);
            }
        }
        rt.pos_ms.store(pos, Ordering::Relaxed);
        let left = dur.saturating_sub(pos);
        let left_s = left.div_ceil(1000);
        if left_s != shown {
            shown = left_s;
            rt.publish(titles::countdown(left, color, w, h));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn run_designed(rt: &InputRt, objects: &[TitleObject], w: usize, h: usize) {
    rt.publish(titles::designed(objects, w, h));
}

fn run_telestrator(rt: &InputRt, w: usize, h: usize) {
    let mut drawn = u64::MAX;
    while !rt.stopped() {
        let gen_now = rt.strokes_gen.load(Ordering::Acquire);
        if gen_now != drawn {
            drawn = gen_now;
            let strokes = rt.strokes.lock().unwrap().clone();
            rt.publish(titles::draw_strokes(&strokes, w, h));
        }
        thread::sleep(Duration::from_millis(16));
    }
}

/// Reads ffmpeg's log on the side: keeps the file length and the last line.
pub(crate) fn watch_log(rt: &Arc<InputRt>, child: &mut Child) -> Arc<Mutex<String>> {
    let last = Arc::new(Mutex::new(String::new()));
    if let Some(err) = child.stderr.take() {
        let (rt, sink) = (rt.clone(), last.clone());
        thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                if let Some(ms) = parse_duration(&line) {
                    rt.dur_ms.store(ms, Ordering::Relaxed);
                }
                *sink.lock().unwrap() = line;
            }
        });
    }
    last
}

pub(crate) fn parse_duration(line: &str) -> Option<u64> {
    let rest = line.trim_start().strip_prefix("Duration: ")?;
    let stamp = rest.split(',').next()?;
    let mut p = stamp.split(':');
    let h: f64 = p.next()?.parse().ok()?;
    let m: f64 = p.next()?.parse().ok()?;
    let s: f64 = p.next()?.parse().ok()?;
    Some(((h * 3600.0 + m * 60.0 + s) * 1000.0) as u64)
}

fn run_capture(rt: &Arc<InputRt>, input_args: Vec<String>, w: usize, h: usize, fps: u32) {
    let size = w * h * 4;
    while !rt.stopped() {
        let mut cmd = ffmpeg();
        cmd.args(&input_args);
        cmd.args(["-map", "0:v:0", "-vf", &fit_filter(w, h, fps)]);
        cmd.args(["-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"]);
        // Sound leaves over a local socket: stdout already carries pictures.
        let audio_port = rt.audio.as_ref().and_then(|_| TcpListener::bind("127.0.0.1:0").ok());
        if let Some(port) = audio_port.as_ref().and_then(|l| l.local_addr().ok()).map(|a| a.port()) {
            cmd.args(["-map", "0:a:0?", "-f", "s16le", "-ar", "48000", "-ac", "2"]);
            cmd.arg(format!("tcp://127.0.0.1:{port}"));
        }
        let spawned = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(_) => {
                rt.set_status(ffmpeg_missing());
                return;
            }
        };
        let log = watch_log(rt, &mut child);
        let mut out = child.stdout.take().unwrap();
        rt.adopt(child);
        if let Some(listener) = audio_port {
            let worker = rt.clone();
            thread::spawn(move || {
                let _ = listener.set_nonblocking(true);
                let deadline = Instant::now() + Duration::from_secs(20);
                while Instant::now() < deadline && !worker.stopped() {
                    match listener.accept() {
                        Ok((sock, _)) => {
                            let _ = sock.set_nonblocking(false);
                            pump_audio(&worker, sock, None);
                            return;
                        }
                        Err(_) => thread::sleep(Duration::from_millis(50)),
                    }
                }
            });
        }
        let mut buf = Frame::new(w, h);
        let mut got_any = false;
        while !rt.stopped() {
            if out.read_exact(&mut buf.data[..size]).is_err() {
                break;
            }
            if !got_any {
                got_any = true;
                rt.clear_status();
            }
            buf = rt.publish(buf).unwrap_or_else(|| Frame::new(w, h));
        }
        rt.kill_children();
        if rt.stopped() {
            return;
        }
        let why = log.lock().unwrap().clone();
        rt.set_status(t!("sources.source_lost", reason = why));
        thread::sleep(Duration::from_secs(2));
    }
}

fn run_microphone(rt: &Arc<InputRt>, device: &str) {
    while !rt.stopped() {
        let spawned = ffmpeg()
            .args(["-f", "dshow", "-audio_buffer_size", "40", "-i"])
            .arg(format!("audio={device}"))
            .args(audio_out_args())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(_) => {
                rt.set_status(ffmpeg_missing());
                return;
            }
        };
        let log = watch_log(rt, &mut child);
        let out = child.stdout.take().unwrap();
        rt.adopt(child);
        pump_audio(rt, out, None);
        rt.kill_children();
        if rt.stopped() {
            return;
        }
        rt.set_status(t!("sources.mic_lost", reason = log.lock().unwrap()));
        thread::sleep(Duration::from_secs(2));
    }
}

pub(crate) fn audio_out_args() -> [&'static str; 8] {
    ["-vn", "-f", "s16le", "-ar", "48000", "-ac", "2", "pipe:1"]
}

/// Moves samples from a pipe to the ring. Waits when the ring is full, so a
/// paused file holds ffmpeg back instead of losing sound.
pub(crate) fn pump_audio(rt: &InputRt, mut out: impl Read, session_stop: Option<&AtomicBool>) {
    let ring = rt.audio.as_ref().unwrap();
    let mut bytes = [0u8; 3840];
    let mut samples = Vec::with_capacity(1920);
    let done = || rt.stopped() || session_stop.is_some_and(|s| s.load(Ordering::Relaxed));
    let mut filled = 0;
    while !done() {
        match out.read(&mut bytes[filled..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => filled += n,
        }
        let whole = filled & !1;
        samples.clear();
        samples.extend(
            bytes[..whole]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0),
        );
        bytes.copy_within(whole..filled, 0);
        filled -= whole;
        while ring.room() < samples.len() {
            if done() {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        ring.push(&samples);
    }
}

/// Capture devices ffmpeg can open: (cameras, microphones).
pub fn list_devices() -> (Vec<String>, Vec<String>) {
    let Ok(out) = ffmpeg()
        .args(["-list_devices", "true", "-f", "dshow", "-i", "dummy"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
    else {
        return (Vec::new(), Vec::new());
    };
    parse_devices(&String::from_utf8_lossy(&out.stderr))
}

fn parse_devices(log: &str) -> (Vec<String>, Vec<String>) {
    let (mut video, mut audio) = (Vec::new(), Vec::new());
    for line in log.lines() {
        let mut parts = line.split('"');
        let (Some(_), Some(name), Some(tail)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if tail.contains("(video)") {
            video.push(name.to_string());
        } else if tail.contains("(audio)") {
            audio.push(name.to_string());
        }
    }
    (video, audio)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_file_length_from_log() {
        assert_eq!(parse_duration("  Duration: 00:01:02.50, start: 0.000000, bitrate: 1 kb/s"), Some(62_500));
        assert_eq!(parse_duration("Stream #0:0: Video"), None);
    }

    #[test]
    fn reads_device_list() {
        let log = "[dshow @ 01] \"HD Webcam\" (video)\n\
                   [dshow @ 01]   Alternative name \"@device_pnp_x\"\n\
                   [dshow @ 01] \"Micro (USB)\" (audio)\n";
        let (v, a) = parse_devices(log);
        assert_eq!(v, ["HD Webcam"]);
        assert_eq!(a, ["Micro (USB)"]);
    }

    #[test]
    fn photos_sort_the_way_people_count() {
        let mut v = vec!["img10.png", "img2.png", "img1.png"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["img1.png", "img2.png", "img10.png"]);
    }

    #[test]
    fn time_left_stops_at_the_out_point() {
        let rt = InputRt::new(false, false);
        rt.dur_ms.store(60_000, Ordering::Relaxed);
        rt.pos_ms.store(4_000, Ordering::Relaxed);
        assert_eq!(rt.remaining_ms(), 56_000);
        rt.end_ms.store(10_000, Ordering::Relaxed);
        assert_eq!(rt.remaining_ms(), 6_000);
    }

    #[test]
    fn ring_pads_with_silence_and_drops_oldest() {
        let r = Ring::new(0.0001); // 9 samples
        r.push(&[1.0; 20]);
        let mut out = [5.0; 12];
        r.pull(&mut out);
        assert_eq!(out[0], 1.0);
        assert_eq!(out[11], 0.0);
    }
}
