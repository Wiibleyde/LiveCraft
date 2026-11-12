//! Everything with a timeline: video files, image sequences, audio files,
//! lists and replay clips. A file is decoded by ffmpeg and read at the output
//! rate, so a paused file simply stops being read and costs no decoding.

use crate::decode::{self, Hardware};
use crate::frame::Frame;
use crate::model::ListItem;
use crate::sources::*;
use std::io::Read;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// What to open and how to play it.
#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    pub input_args: Vec<String>,
    pub looped: bool,
    pub mark_in: u64,
    pub mark_out: u64,
    pub rate: f32,
    pub with_audio: bool,
}

impl Spec {
    pub fn file(path: String, looped: bool, mark_in: u64, mark_out: u64, rate: f32, audio: bool) -> Self {
        // One limit for pictures and sound, or they would run at different speeds.
        let rate = if rate > 0.0 { rate.clamp(0.05, 8.0) } else { 1.0 };
        Self { input_args: vec!["-i".into(), path], looped, mark_in, mark_out, rate, with_audio: audio }
    }

    /// Turns the first file of a numbered series into ffmpeg's pattern.
    pub fn sequence(first: &str, fps: f32, looped: bool) -> Option<Self> {
        let (stem, ext) = first.rsplit_once('.')?;
        let digits = stem.chars().rev().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let (head, number) = stem.split_at(stem.len() - digits);
        let start: u64 = number.parse().ok()?;
        let pattern = format!("{head}%0{digits}d.{ext}");
        let args = vec![
            "-framerate".into(),
            format!("{fps}"),
            "-start_number".into(),
            start.to_string(),
            "-i".into(),
            pattern,
        ];
        Some(Self { input_args: args, looped, mark_in: 0, mark_out: 0, rate: 1.0, with_audio: false })
    }

    /// Marks restart the file by hand; a plain file can let ffmpeg loop it
    /// without any gap.
    fn native_loop(&self) -> bool {
        self.looped && self.mark_in == 0 && self.mark_out == 0
    }

    fn has_marks(&self) -> bool {
        self.mark_out > self.mark_in
    }

    /// Keeps a position inside the marks.
    fn clamp(&self, at: u64) -> u64 {
        let at = at.max(self.mark_in);
        if self.has_marks() && at >= self.mark_out { self.mark_in } else { at }
    }

    /// Where software takes over after the card failed having shown pictures:
    /// a couple of frames before the position reached, so no picture is lost,
    /// and never wrapped back to the start of the marks.
    fn resume_point(&self, at: u64, frame_ms: f64) -> u64 {
        let back = (2.0 * frame_ms * self.rate as f64) as u64;
        let at = at.saturating_sub(back).max(self.mark_in);
        if self.has_marks() && at >= self.mark_out {
            self.mark_out.saturating_sub(1).max(self.mark_in)
        } else {
            at
        }
    }

    fn time_args(&self, start_ms: u64) -> Vec<String> {
        let mut a = Vec::new();
        if start_ms > 0 {
            a.extend(["-ss".to_string(), format!("{:.3}", start_ms as f64 / 1000.0)]);
        }
        if self.mark_out > start_ms {
            a.extend(["-to".to_string(), format!("{:.3}", self.mark_out as f64 / 1000.0)]);
        }
        if self.native_loop() {
            a.extend(["-stream_loop".to_string(), "-1".to_string()]);
        }
        a
    }
}

/// atempo only spans 0.5 to 2 per stage, so slow speeds chain several.
pub fn tempo_filter(rate: f32) -> Option<String> {
    if (rate - 1.0).abs() < 0.001 {
        return None;
    }
    let mut left = rate.clamp(0.05, 8.0);
    let mut stages = Vec::new();
    while left < 0.5 {
        stages.push("atempo=0.5".to_string());
        left /= 0.5;
    }
    while left > 2.0 {
        stages.push("atempo=2.0".to_string());
        left /= 2.0;
    }
    stages.push(format!("atempo={left:.4}"));
    Some(stages.join(","))
}

enum End {
    Finished,
    /// Nothing could be decoded at all.
    Failed,
    Stopped,
    Seek(u64),
    Jump,
    /// The graphics card failed; software carries on from the position reached.
    /// `shown` tells whether the card had put up a picture before failing.
    CardFailed {
        shown: bool,
    },
}

fn start_audio(
    rt: &Arc<InputRt>,
    spec: &Spec,
    start_ms: u64,
    stop: Arc<AtomicBool>,
) -> Option<thread::JoinHandle<()>> {
    let mut cmd = ffmpeg();
    cmd.args(spec.time_args(start_ms)).args(&spec.input_args);
    if let Some(f) = tempo_filter(spec.rate) {
        cmd.args(["-af", &f]);
    }
    let spawned =
        cmd.args(audio_out_args()).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
    let mut child = spawned.ok()?;
    let out = child.stdout.take()?;
    rt.adopt(child);
    let rt = rt.clone();
    Some(thread::spawn(move || pump_audio(&rt, out, Some(&stop))))
}

/// Lets the sound reader finish, so nothing old lands after the ring is cleared.
fn wait_for_pump(pump: Option<thread::JoinHandle<()>>) {
    let Some(pump) = pump else { return };
    let deadline = Instant::now() + Duration::from_millis(500);
    while !pump.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
}

/// Whether the graphics card decodes a file: allowed by the desk, known to pay
/// off for this file, and not already failed on it.
struct CardChoice<'a> {
    allowed: Option<Hardware>,
    path: &'a str,
    failed: bool,
    /// Software takes over after a card that had been showing pictures: when
    /// it finds none left, the file had ended (a damaged tail made the card
    /// fail at the very end), which is not a failure to play.
    resumed: bool,
}

impl CardChoice<'_> {
    fn now(&self) -> Option<Hardware> {
        self.allowed.filter(|_| !self.failed && decode::verdict(self.path) == Some(true))
    }
}

/// Plays from `start_ms` until the file ends or something interrupts it.
fn play_once(
    rt: &Arc<InputRt>,
    spec: &Spec,
    start_ms: u64,
    w: usize,
    h: usize,
    fps: u32,
    choice: &CardChoice,
) -> End {
    let size = w * h * 4;
    let frame_ms = 1000.0 / fps as f64;
    let vf = if (spec.rate - 1.0).abs() < 0.001 {
        fit_filter(w, h, fps)
    } else {
        format!("setpts=PTS/{},{}", spec.rate, fit_filter(w, h, fps))
    };
    let card = choice.now();
    let mut cmd = ffmpeg();
    if let Some(card) = card {
        // A decoding error then ends ffmpeg, so a card failing mid-file hands
        // over to software instead of leaving a frozen or broken picture.
        cmd.args(card).arg("-xerror");
    }
    rt.hardware.store(card.is_some(), Ordering::Relaxed);
    let spawned = cmd
        .args(spec.time_args(start_ms))
        .args(&spec.input_args)
        .args(["-an", "-vf", &vf, "-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(_) => {
            rt.set_status(ffmpeg_missing());
            return End::Stopped;
        }
    };
    let log = watch_log(rt, &mut child);
    let mut out = child.stdout.take().unwrap();
    let pid = child.id();
    rt.adopt(child);

    let audio_stop = Arc::new(AtomicBool::new(false));
    let pump = spec.with_audio.then(|| start_audio(rt, spec, start_ms, audio_stop.clone())).flatten();
    let mut buf = Frame::new(w, h);
    let mut shown: u64 = 0;
    let mut anchor = Instant::now();
    let mut first_done = false;
    let end = loop {
        if rt.stopped() {
            break End::Stopped;
        }
        let seek = rt.seek_ms.swap(-1, Ordering::Relaxed);
        if seek >= 0 {
            break End::Seek(seek as u64);
        }
        if rt.select.load(Ordering::Relaxed) >= 0 {
            break End::Jump;
        }
        // The first frame shows even when paused, so the card isn't black.
        if !rt.is_playing() && first_done {
            // A file cued before the card was known to pay off moves to it
            // before anyone sees it play.
            if card.is_none() && choice.now().is_some() {
                break End::Seek(rt.pos_ms.load(Ordering::Relaxed));
            }
            thread::sleep(Duration::from_millis(15));
            anchor = Instant::now();
            shown = 0;
            continue;
        }
        if out.read_exact(&mut buf.data[..size]).is_err() {
            if card.is_some() && !rt.stopped() && (!first_done || rt.ended_badly(pid)) {
                break End::CardFailed { shown: first_done };
            }
            break if first_done || choice.resumed { End::Finished } else { End::Failed };
        }
        first_done = true;
        rt.clear_status();
        buf = rt.publish(buf).unwrap_or_else(|| Frame::new(w, h));
        if !rt.is_playing() {
            continue;
        }
        let pos = rt.pos_ms.load(Ordering::Relaxed) as f64 + frame_ms * spec.rate as f64;
        let dur = rt.dur_ms.load(Ordering::Relaxed) as f64;
        let wrapped = if spec.native_loop() && dur > 0.0 { pos % dur } else { pos };
        rt.pos_ms.store(wrapped as u64, Ordering::Relaxed);
        shown += 1;
        let due = anchor + Duration::from_secs_f64(shown as f64 * frame_ms / 1000.0);
        let now = Instant::now();
        if due > now {
            thread::sleep(due - now);
        }
    };
    audio_stop.store(true, Ordering::Relaxed);
    rt.kill_children();
    wait_for_pump(pump);
    // At a natural end the last second of sound is still waiting to be heard.
    if !matches!(end, End::Finished)
        && let Some(ring) = &rt.audio
    {
        ring.clear();
    }
    if matches!(end, End::Failed) {
        rt.set_status(t!("playback.cannot_play", reason = log.lock().unwrap()));
    }
    end
}

/// Holds the last picture until asked to play again. Returns where to resume.
/// A seek only moves the position: it does not start playing by itself.
fn wait_after_end(rt: &InputRt, from: u64) -> Option<u64> {
    rt.playing.store(false, Ordering::Relaxed);
    while !rt.stopped() {
        let seek = rt.seek_ms.swap(-1, Ordering::Relaxed);
        // A list can still be told to move to another item.
        let jump = rt.items.load(Ordering::Relaxed) > 0 && rt.select.load(Ordering::Relaxed) >= 0;
        if seek >= 0 || jump || rt.is_playing() {
            if let Some(ring) = &rt.audio {
                ring.clear();
            }
            return Some(if seek >= 0 { seek as u64 } else { from });
        }
        thread::sleep(Duration::from_millis(30));
    }
    None
}

/// For `--selftest`: software taking over at `start_ms` after a card that had
/// shown pictures. Whether it found the file already at its end, which counts
/// as a finished file and not as one that cannot be played.
pub(crate) fn resumes_to_the_end(rt: &Arc<InputRt>, spec: &Spec, start_ms: u64, w: usize, h: usize) -> bool {
    let choice = CardChoice { allowed: None, path: "", failed: true, resumed: true };
    matches!(play_once(rt, spec, start_ms, w, h, 30, &choice), End::Finished)
}

pub fn run_file(rt: &Arc<InputRt>, spec: Spec, w: usize, h: usize, fps: u32, card: Option<Hardware>) {
    let path = spec.input_args.last().cloned().unwrap_or_default();
    let mut choice = CardChoice { allowed: card, path: &path, failed: false, resumed: false };
    // A replacement source carries on from where the old one stood.
    let mut start = spec.clamp(rt.pos_ms.load(Ordering::Relaxed));
    rt.pos_ms.store(start, Ordering::Relaxed);
    loop {
        rt.seek_ms.store(-1, Ordering::Relaxed);
        let end = play_once(rt, &spec, start, w, h, fps, &choice);
        choice.resumed = false;
        match end {
            End::Stopped | End::Failed => return,
            // Once failed on this file, the card is not asked again.
            End::CardFailed { shown } => {
                choice.failed = true;
                let at = rt.pos_ms.load(Ordering::Relaxed);
                start = if shown { spec.resume_point(at, 1000.0 / fps as f64) } else { spec.clamp(at) };
                choice.resumed = shown;
                rt.pos_ms.store(start, Ordering::Relaxed);
            }
            End::Seek(at) => {
                start = spec.clamp(at);
                rt.pos_ms.store(start, Ordering::Relaxed);
            }
            End::Jump => {
                rt.select.store(-1, Ordering::Relaxed);
            }
            End::Finished => {
                if spec.looped {
                    start = spec.mark_in;
                    rt.pos_ms.store(start, Ordering::Relaxed);
                    continue;
                }
                rt.completions.fetch_add(1, Ordering::Relaxed);
                let Some(at) = wait_after_end(rt, spec.mark_in) else { return };
                start = spec.clamp(at);
                rt.pos_ms.store(start, Ordering::Relaxed);
            }
        }
    }
}

pub fn run_audio_file(rt: &Arc<InputRt>, path: &str, looped: bool) {
    let spec = Spec::file(path.to_string(), looped, 0, 0, 1.0, true);
    let mut start = rt.pos_ms.load(Ordering::Relaxed);
    while !rt.stopped() {
        let mut cmd = ffmpeg();
        cmd.args(spec.time_args(start)).args(&spec.input_args).args(audio_out_args());
        let spawned = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(_) => {
                rt.set_status(ffmpeg_missing());
                return;
            }
        };
        watch_log(rt, &mut child);
        let out = child.stdout.take().unwrap();
        rt.adopt(child);
        let stop = Arc::new(AtomicBool::new(false));
        let (flag, worker) = (stop.clone(), rt.clone());
        let pump = thread::spawn(move || pump_audio(&worker, out, Some(&flag)));
        let mut played_ms = 0u64;
        let mut seek = None;
        let mut last = Instant::now();
        let ring = rt.audio.clone().unwrap();
        // The file is done when the last of it has been heard, not when read.
        while (!pump.is_finished() || !ring.is_empty()) && !rt.stopped() {
            thread::sleep(Duration::from_millis(30));
            let now = Instant::now();
            if rt.is_playing() {
                played_ms += now.duration_since(last).as_millis() as u64;
            }
            last = now;
            let dur = rt.dur_ms.load(Ordering::Relaxed);
            let pos = start + played_ms;
            let pos = if looped && dur > 0 { pos % dur } else { pos };
            rt.pos_ms.store(pos, Ordering::Relaxed);
            let s = rt.seek_ms.swap(-1, Ordering::Relaxed);
            if s >= 0 {
                seek = Some(s as u64);
                break;
            }
        }
        stop.store(true, Ordering::Relaxed);
        rt.kill_children();
        wait_for_pump(Some(pump));
        if let Some(at) = seek {
            ring.clear();
            start = at;
            rt.pos_ms.store(at, Ordering::Relaxed);
            continue;
        }
        if rt.stopped() {
            return;
        }
        rt.completions.fetch_add(1, Ordering::Relaxed);
        match wait_after_end(rt, 0) {
            Some(at) => {
                start = at;
                rt.pos_ms.store(at, Ordering::Relaxed);
            }
            None => return,
        }
    }
}

/// Plays a list of files in turn, as one input.
#[allow(clippy::too_many_arguments)]
pub fn run_list(
    rt: &Arc<InputRt>,
    mut items: Vec<ListItem>,
    auto_next: bool,
    looped: bool,
    shuffle: bool,
    w: usize,
    h: usize,
    fps: u32,
    card: Option<Hardware>,
) {
    if items.is_empty() {
        rt.set_status(t!("playback.empty_list"));
        return;
    }
    if shuffle {
        shuffle_items(&mut items);
    }
    rt.items.store(items.len() as u64, Ordering::Relaxed);
    let mut index = (rt.item.load(Ordering::Relaxed) as usize).min(items.len() - 1);
    let mut start = rt.pos_ms.load(Ordering::Relaxed);
    let mut failures = 0;
    let mut card_failed = vec![false; items.len()];
    let mut resumed = false;
    if card.is_some() {
        // Asked ahead, so the next item knows its answer when its turn comes.
        decode::ask_ahead(items.iter().map(|i| i.path.clone()).collect());
    }
    loop {
        rt.item.store(index as u64, Ordering::Relaxed);
        rt.pos_ms.store(start, Ordering::Relaxed);
        let path = &items[index].path;
        let choice = CardChoice { allowed: card, path, failed: card_failed[index], resumed };
        let spec = Spec::file(path.clone(), false, 0, 0, 1.0, true);
        let end = play_once(rt, &spec, start, w, h, fps, &choice);
        resumed = false;
        start = 0;
        match end {
            End::Stopped => return,
            End::CardFailed { shown } => {
                card_failed[index] = true;
                let at = rt.pos_ms.load(Ordering::Relaxed);
                start = if shown { spec.resume_point(at, 1000.0 / fps as f64) } else { at };
                resumed = shown;
            }
            End::Seek(at) => start = at,
            End::Jump => {
                index = rt.select.swap(-1, Ordering::Relaxed).max(0) as usize % items.len();
            }
            End::Failed => {
                // A bad file is skipped; a list of nothing but bad files stops.
                failures += 1;
                if failures >= items.len() {
                    return;
                }
                index = (index + 1) % items.len();
            }
            End::Finished => {
                failures = 0;
                let last = index + 1 >= items.len();
                let next = (index + 1) % items.len();
                if last && !looped {
                    rt.completions.fetch_add(1, Ordering::Relaxed);
                    let Some(at) = wait_after_end(rt, 0) else { return };
                    index = 0;
                    start = at;
                } else {
                    index = next;
                    if !auto_next {
                        let Some(at) = wait_after_end(rt, 0) else { return };
                        start = at;
                    }
                }
            }
        }
    }
}

/// Mixes the order without pulling in a random number library.
fn shuffle_items(items: &mut [ListItem]) {
    let mut seed =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64)
            | 1;
    for i in (1..items.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        items.swap(i, (seed % (i as u64 + 1)) as usize);
    }
}

/// Waits for clips. One camera plays whole; several share the screen in four.
pub fn run_replay(rt: &Arc<InputRt>, w: usize, h: usize, fps: u32) {
    rt.publish(Frame::solid(w, h, [10, 10, 14]));
    // Where each file's pictures start never changes; asking ffmpeg costs a
    // few hundred milliseconds per press.
    let mut starts = std::collections::HashMap::new();
    while !rt.stopped() {
        let Some(play) = rt.replay.lock().unwrap().take() else {
            thread::sleep(Duration::from_millis(30));
            continue;
        };
        rt.playing.store(true, Ordering::Relaxed);
        rt.pos_ms.store(0, Ordering::Relaxed);
        rt.dur_ms.store((play.span_s.max(0.0) * 1000.0) as u64, Ordering::Relaxed);
        play_clip(rt, &play, w, h, fps, &mut starts);
        rt.completions.fetch_add(1, Ordering::Relaxed);
        rt.playing.store(false, Ordering::Relaxed);
    }
}

/// The timestamp of the first picture in `framecrc` output, in seconds. A
/// file's other streams may start earlier, as AAC primes its sound.
fn first_pts(framecrc: &str) -> Option<f64> {
    let mut unit = None;
    for line in framecrc.lines() {
        if let Some(tb) = line.strip_prefix("#tb 0:") {
            let (num, den) = tb.trim().split_once('/')?;
            unit = Some(num.trim().parse::<f64>().ok()? / den.trim().parse::<f64>().ok()?);
        } else if !line.starts_with('#') {
            return Some(line.split(',').nth(2)?.trim().parse::<f64>().ok()? * unit?);
        }
    }
    None
}

/// When a file's first picture is stamped, in seconds, or None before it has one.
fn first_picture_s(file: &str) -> std::io::Result<Option<f64>> {
    let probe = ffmpeg()
        .args(["-v", "error", "-copyts", "-i", file, "-map", "0:v:0", "-c", "copy"])
        .args(["-frames:v", "1", "-f", "framecrc", "-"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    Ok(first_pts(&String::from_utf8_lossy(&probe.stdout)))
}

/// Decodes the clip's pictures as they are in the file; the player times them
/// itself. Seeking in MPEG-TS lands a few pictures off, so decoding starts
/// well before, past a keyframe (one every two seconds), and the pictures are
/// picked by their own timestamps, `first` being the file's first picture.
fn clip_decoder(clip: &ReplayClip, first: f64, w: usize, h: usize) -> std::io::Result<std::process::Child> {
    let rough = (clip.from_s - 3.0).max(0.0);
    let pick =
        format!("trim=start={:.6}:end={:.6},{}", first + clip.from_s, first + clip.to_s, fit_scale(w, h));
    ffmpeg()
        .args(["-copyts", "-noaccurate_seek", "-ss", &format!("{rough:.6}"), "-i", &clip.file])
        .args([
            "-an",
            "-vf",
            &pick,
            "-fps_mode",
            "passthrough",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
}

struct Tile {
    out: Option<std::process::ChildStdout>,
    picture: Frame,
    read: usize,
    ended: bool,
}

fn play_clip(
    rt: &Arc<InputRt>,
    play: &ReplayPlay,
    w: usize,
    h: usize,
    fps: u32,
    starts: &mut std::collections::HashMap<String, f64>,
) {
    let quad = play.clips.len() > 1;
    let (cw, ch) = if quad { (w / 2, h / 2) } else { (w, h) };
    let mut tiles = Vec::new();
    for clip in play.clips.iter().take(4) {
        // Probing a file takes a while: give up if the show is closing or
        // another moment was asked for meanwhile.
        if rt.stopped() || rt.replay.lock().unwrap().is_some() {
            rt.kill_children();
            return;
        }
        let first = match starts.get(&clip.file) {
            Some(s) => Ok(Some(*s)),
            None => first_picture_s(&clip.file),
        };
        let decoder = match first {
            Ok(Some(first)) => {
                starts.insert(clip.file.clone(), first);
                clip_decoder(clip, first, cw, ch).map(Some)
            }
            other => other.map(|_| None),
        };
        let out = match decoder {
            Err(_) => {
                rt.set_status(ffmpeg_missing());
                return;
            }
            Ok(None) => None,
            Ok(Some(mut child)) => {
                let out = child.stdout.take();
                rt.adopt(child);
                out
            }
        };
        tiles.push(Tile { ended: out.is_none(), out, picture: Frame::new(cw, ch), read: 0 });
    }
    let period = Duration::from_secs_f64(1.0 / fps as f64);
    // Slow motion walks through the moment slower than the show's clock.
    let step = play.speed.max(0.01) as f64 / fps as f64;
    let mut next = Instant::now();
    let mut ticks = 0u64;
    let mut last_shown = false;
    while !rt.stopped() {
        if rt.replay.lock().unwrap().is_some() {
            break;
        }
        if !rt.is_playing() {
            thread::sleep(Duration::from_millis(15));
            next = Instant::now();
            continue;
        }
        let mut clock = ticks as f64 * step;
        if clock >= play.span_s {
            // Ticks rarely land on the out mark. The picture on screen at that
            // moment is the moment's last, so show it once before stopping
            // instead of ending a tick early on the picture before.
            if last_shown {
                break;
            }
            last_shown = true;
            clock = play.span_s - 1e-6;
        }
        // Each camera shows the picture it had at this moment: pictures lost
        // while recording hold the one before for as long as they lasted, and
        // copies made to fill a late tick are passed over.
        for (tile, clip) in tiles.iter_mut().zip(&play.clips) {
            while !tile.ended && clip.due(tile.read, fps).is_some_and(|t| t <= clock + 1e-6) {
                let picture = &mut tile.picture.data;
                if tile.out.as_mut().is_some_and(|out| out.read_exact(picture).is_ok()) {
                    tile.read += 1;
                } else {
                    tile.ended = true;
                }
            }
        }
        if tiles.iter().all(|t| t.ended && t.read == 0) {
            break;
        }
        let mut picture = Frame::solid(w, h, [0, 0, 0]);
        for (n, tile) in tiles.iter().enumerate() {
            if quad {
                let (x, y) = ((n % 2 * cw) as f32, (n / 2 * ch) as f32);
                picture.blit_region(&tile.picture, [0.0; 4], x, y, cw as f32, ch as f32, 255, 0.0);
            } else {
                picture.copy_from(&tile.picture);
            }
        }
        rt.publish(picture);
        ticks += 1;
        rt.pos_ms.store((ticks as f64 * step * 1000.0) as u64, Ordering::Relaxed);
        next += period;
        let now = Instant::now();
        if next > now {
            thread::sleep(next - now);
        } else {
            next = now;
        }
    }
    rt.kill_children();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn software_resumes_just_before_where_the_card_failed() {
        let free = Spec::file("a.mp4".into(), false, 0, 0, 1.0, false);
        assert_eq!(free.resume_point(5000, 1000.0 / 30.0), 5000 - 66);
        assert_eq!(free.resume_point(10, 33.3), 0, "never before the start");
        let marked = Spec::file("a.mp4".into(), false, 2000, 6000, 1.0, false);
        assert_eq!(marked.resume_point(2010, 33.3), 2000, "never before the in mark");
        assert_eq!(marked.resume_point(6000, 33.3), 5934);
        assert_eq!(marked.resume_point(9000, 33.3), 5999, "past the out mark: the end, not the start");
    }

    #[test]
    fn numbered_files_become_a_pattern() {
        let s = Spec::sequence("C:/anim/frame0012.png", 30.0, true).unwrap();
        assert!(s.input_args.contains(&"C:/anim/frame%04d.png".to_string()));
        assert!(s.input_args.contains(&"12".to_string()));
        assert!(Spec::sequence("C:/anim/frame.png", 30.0, true).is_none());
    }

    #[test]
    fn slow_speeds_chain_tempo_stages() {
        assert_eq!(tempo_filter(1.0), None);
        assert_eq!(tempo_filter(1.5).unwrap(), "atempo=1.5000");
        assert_eq!(tempo_filter(0.25).unwrap(), "atempo=0.5,atempo=0.5000");
    }

    #[test]
    fn a_replay_finds_where_its_pictures_start() {
        // As ffmpeg 9 prints it for a replay recording: pictures start 1024
        // sound samples after the file does.
        let crc = "#extradata 0:       36, 0x9ae208ea\n#tb 0: 1/90000\n#media_type 0: video\n\
                   0,     127920,     127920,     3000,      924, 0xde1b4f8f, S=1\n";
        assert_eq!(first_pts(crc), Some(127920.0 / 90000.0));
        assert_eq!(first_pts("#tb 0: 1/90000\n"), None, "no picture recorded yet");
    }

    #[test]
    fn marks_replace_the_native_loop() {
        let plain = Spec::file("a.mp4".into(), true, 0, 0, 1.0, true);
        assert!(plain.time_args(0).contains(&"-stream_loop".to_string()));
        let marked = Spec::file("a.mp4".into(), true, 1000, 5000, 1.0, true);
        assert!(!marked.time_args(1000).contains(&"-stream_loop".to_string()));
        assert!(marked.time_args(1000).contains(&"5.000".to_string()));
    }

    #[test]
    fn positions_stay_inside_the_marks() {
        let s = Spec::file("a.mp4".into(), false, 5000, 10000, 1.0, true);
        assert_eq!(s.clamp(0), 5000, "restart goes to the in point");
        assert_eq!(s.clamp(7000), 7000);
        assert_eq!(s.clamp(20000), 5000, "past the out point goes back in");
    }

    #[test]
    fn speed_has_one_limit_for_sound_and_picture() {
        assert_eq!(Spec::file("a".into(), false, 0, 0, 20.0, true).rate, 8.0);
        assert_eq!(Spec::file("a".into(), false, 0, 0, 0.0, true).rate, 1.0);
    }

    #[test]
    fn shuffle_keeps_every_item() {
        let mut v: Vec<ListItem> = (0..20).map(|i| ListItem { path: i.to_string(), enabled: true }).collect();
        shuffle_items(&mut v);
        let mut names: Vec<usize> = v.iter().map(|i| i.path.parse().unwrap()).collect();
        names.sort();
        assert_eq!(names, (0..20).collect::<Vec<_>>());
    }
}
