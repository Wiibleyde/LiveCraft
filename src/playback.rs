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
    CardFailed,
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
                break End::CardFailed;
            }
            break if first_done { End::Finished } else { End::Failed };
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

pub fn run_file(rt: &Arc<InputRt>, spec: Spec, w: usize, h: usize, fps: u32, card: Option<Hardware>) {
    let path = spec.input_args.last().cloned().unwrap_or_default();
    let mut choice = CardChoice { allowed: card, path: &path, failed: false };
    // A replacement source carries on from where the old one stood.
    let mut start = spec.clamp(rt.pos_ms.load(Ordering::Relaxed));
    rt.pos_ms.store(start, Ordering::Relaxed);
    loop {
        rt.seek_ms.store(-1, Ordering::Relaxed);
        match play_once(rt, &spec, start, w, h, fps, &choice) {
            End::Stopped | End::Failed => return,
            // Once failed on this file, the card is not asked again.
            End::CardFailed => {
                choice.failed = true;
                start = spec.clamp(rt.pos_ms.load(Ordering::Relaxed));
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
    if card.is_some() {
        // Asked ahead, so the next item knows its answer when its turn comes.
        decode::ask_ahead(items.iter().map(|i| i.path.clone()).collect());
    }
    loop {
        rt.item.store(index as u64, Ordering::Relaxed);
        rt.pos_ms.store(start, Ordering::Relaxed);
        let path = &items[index].path;
        let choice = CardChoice { allowed: card, path, failed: card_failed[index] };
        let spec = Spec::file(path.clone(), false, 0, 0, 1.0, true);
        let end = play_once(rt, &spec, start, w, h, fps, &choice);
        start = 0;
        match end {
            End::Stopped => return,
            End::CardFailed => {
                card_failed[index] = true;
                start = rt.pos_ms.load(Ordering::Relaxed);
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
    while !rt.stopped() {
        let Some(play) = rt.replay.lock().unwrap().take() else {
            thread::sleep(Duration::from_millis(30));
            continue;
        };
        rt.playing.store(true, Ordering::Relaxed);
        rt.pos_ms.store(0, Ordering::Relaxed);
        rt.dur_ms.store(((play.end_s - play.start_s).max(0.0) * 1000.0) as u64, Ordering::Relaxed);
        play_clip(rt, &play, w, h, fps);
        rt.completions.fetch_add(1, Ordering::Relaxed);
        rt.playing.store(false, Ordering::Relaxed);
    }
}

fn clip_decoder(file: &str, play: &ReplayPlay, w: usize, h: usize, fps: u32) -> Option<std::process::Child> {
    let vf = format!("setpts=PTS/{},{}", play.speed.max(0.01), fit_filter(w, h, fps));
    ffmpeg()
        .args(["-ss", &format!("{:.3}", play.start_s), "-to", &format!("{:.3}", play.end_s)])
        .args(["-i", file, "-an", "-vf", &vf, "-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

fn play_clip(rt: &Arc<InputRt>, play: &ReplayPlay, w: usize, h: usize, fps: u32) {
    let quad = play.files.len() > 1;
    let (cw, ch) = if quad { (w / 2, h / 2) } else { (w, h) };
    let mut outs = Vec::new();
    for file in play.files.iter().take(4) {
        let Some(mut child) = clip_decoder(file, play, cw, ch, fps) else {
            rt.set_status(ffmpeg_missing());
            return;
        };
        outs.push(child.stdout.take().unwrap());
        rt.adopt(child);
    }
    let period = Duration::from_secs_f64(1.0 / fps as f64);
    let mut tiles = vec![Frame::new(cw, ch); outs.len()];
    let mut next = Instant::now();
    let mut frames = 0u64;
    'clip: while !rt.stopped() {
        if rt.replay.lock().unwrap().is_some() {
            break;
        }
        if !rt.is_playing() {
            thread::sleep(Duration::from_millis(15));
            next = Instant::now();
            continue;
        }
        for (out, tile) in outs.iter_mut().zip(&mut tiles) {
            if out.read_exact(&mut tile.data).is_err() {
                break 'clip;
            }
        }
        let mut picture = Frame::solid(w, h, [0, 0, 0]);
        for (n, tile) in tiles.iter().enumerate() {
            if quad {
                let (x, y) = ((n % 2 * cw) as f32, (n / 2 * ch) as f32);
                picture.blit_region(tile, [0.0; 4], x, y, cw as f32, ch as f32, 255, 0.0);
            } else {
                picture.copy_from(tile);
            }
        }
        rt.publish(picture);
        frames += 1;
        let done = frames as f64 / fps as f64 * play.speed as f64 * 1000.0;
        rt.pos_ms.store(done as u64, Ordering::Relaxed);
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
