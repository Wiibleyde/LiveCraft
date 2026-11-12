//! Recording and streaming. One ffmpeg process per output takes raw pictures on
//! stdin and raw sound over a local socket, because stdin is its only pipe.
//! Several destinations at the same quality share one encode through ffmpeg's
//! tee muxer, which costs one encoder instead of five.

use crate::clock::{self, FrameLine, SampleLine};
use crate::frame::Frame;
use crate::model::AudioPick;
use crate::sources::{end_process_tree, ffmpeg};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub enum Target {
    /// `split_minutes` above 0 rolls to a new numbered file.
    File {
        path: String,
        split_minutes: u32,
    },
    Rtmp(Vec<String>),
    Srt(String),
    /// A file meant to be read while it is still growing.
    Growing(String),
}

pub struct Spec {
    pub label: String,
    pub target: Target,
    pub codec: String,
    pub w: usize,
    pub h: usize,
    pub fps: u32,
    pub kbps: u32,
    pub audio_kbps: u32,
    pub audio: AudioPick,
}

pub struct Encoder {
    video: Option<SyncSender<(u64, Arc<Frame>)>>,
    audio: Option<SyncSender<(u64, Vec<i16>)>>,
    writers: Option<(JoinHandle<Option<PictureTail>>, JoinHandle<Option<SoundTail>>)>,
    size: (usize, usize),
    fps: u32,
    channels: usize,
    pub label: String,
    pub codec: String,
    pub pick: AudioPick,
    pub dropped: AtomicU64,
    pub sent: AtomicU64,
    /// Pictures written again for periods no new one arrived in time for.
    pub repeated: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
    log: Arc<Mutex<String>>,
    warning: Arc<Mutex<Option<String>>>,
    child: Arc<Mutex<Option<Child>>>,
    /// Where the file starts on the shared clock, `NOT_YET` until ffmpeg is ready.
    began: Arc<AtomicU64>,
}

const NOT_YET: u64 = u64::MAX;

/// What a writer hands back once its queue is empty, so that sound and
/// picture can be brought to the same end before the pipes close.
struct PictureTail {
    pipe: ChildStdin,
    last: Option<Arc<Frame>>,
    frames: u64,
}

struct SoundTail {
    sock: TcpStream,
    samples: u64,
}

/// A writer's result, unless it is still stuck on its pipe at `deadline`.
fn join_by<T>(job: JoinHandle<Option<T>>, deadline: Instant) -> Option<T> {
    while !job.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    if job.is_finished() { job.join().ok().flatten() } else { None }
}

/// Pads the shorter of picture and sound to the end of the other: a file
/// ends with both, not with one of them missing for its last moment.
fn even_ends(mut p: PictureTail, mut s: SoundTail, fps: u32, channels: usize) {
    let Some(last) = p.last.take() else { return };
    let (frames, samples) = clock::even_end(p.frames, s.samples, fps);
    for _ in p.frames..frames {
        if p.pipe.write_all(&last.data).is_err() {
            return;
        }
    }
    let _ = s.sock.write_all(&vec![0u8; (samples - s.samples) as usize * channels * 2]);
}

fn add_quality(cmd: &mut Command, s: &Spec, low_latency: bool) {
    let kbps = s.kbps.max(100);
    let keyframes = (s.fps * 2).to_string();
    cmd.args(["-c:v", &s.codec, "-pix_fmt", "yuv420p", "-g", &keyframes]);
    match s.codec.as_str() {
        "libx264" | "libx265" => {
            cmd.args(["-preset", "veryfast"]);
            if low_latency {
                cmd.args(["-tune", "zerolatency"]);
            }
        }
        "libsvtav1" => {
            cmd.args(["-preset", "10"]);
        }
        _ => {}
    }
    cmd.args(["-b:v", &format!("{kbps}k"), "-maxrate", &format!("{kbps}k")])
        .args(["-bufsize", &format!("{}k", kbps * 2)])
        .args(["-c:a", "aac", "-b:a", &format!("{}k", s.audio_kbps.max(32))]);
}

/// Buses are separate mixes, not surround: each stereo pair gets its own track.
pub fn audio_maps(tracks: usize) -> Vec<String> {
    if tracks <= 1 {
        return vec!["-map".into(), "0:a".into()];
    }
    let graph: Vec<String> =
        (0..tracks).map(|n| format!("[0:a]pan=stereo|c0=c{}|c1=c{}[a{n}]", n * 2, n * 2 + 1)).collect();
    let mut args = vec!["-filter_complex".to_string(), graph.join(";")];
    for n in 0..tracks {
        args.push("-map".into());
        args.push(format!("[a{n}]"));
    }
    args
}

/// A file killed mid-way stays playable: pieces are complete as they go.
fn add_file_target(cmd: &mut Command, path: &str, split_minutes: u32) {
    let (stem, ext) = path.rsplit_once('.').unwrap_or((path, "mp4"));
    let fragmented = matches!(ext, "mp4" | "mov");
    if split_minutes > 0 {
        let seconds = (split_minutes * 60).to_string();
        cmd.args(["-f", "segment", "-segment_time", &seconds, "-reset_timestamps", "1"]);
        if fragmented {
            cmd.args(["-segment_format_options", "movflags=+frag_keyframe+empty_moov"]);
        }
        // The pattern is read by ffmpeg, so a % in a folder name must be doubled.
        cmd.arg(format!("{}-%03d.{ext}", stem.replace('%', "%%")));
    } else {
        if fragmented {
            cmd.args(["-movflags", "+frag_keyframe+empty_moov+default_base_moof"]);
        }
        cmd.arg(path);
    }
}

/// Tee addresses need their own separators protected.
pub fn tee_list(urls: &[String]) -> String {
    urls.iter()
        .map(|u| {
            let safe = u.replace('\\', "\\\\").replace('|', "\\|");
            format!("[f=flv:onfail=ignore:flvflags=no_duration_filesize]{safe}")
        })
        .collect::<Vec<_>>()
        .join("|")
}

/// The line worth showing: ffmpeg ends a failure with a generic one, so the
/// first line naming the problem beats the last.
fn is_cause(line: &str) -> bool {
    let l = line.to_lowercase();
    ["error", "invalid", "unknown", "failed", "refused", "not found", "denied", "unable"]
        .iter()
        .any(|w| l.contains(w))
        && !l.contains("conversion failed")
}

impl Encoder {
    pub fn start(s: Spec) -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let tracks = s.audio.count.max(1) as usize;
        let channels = tracks * 2;
        // Sound is opened first: ffmpeg reads an input before opening the next,
        // and pictures only start once the sound socket is connected.
        let mut cmd = ffmpeg();
        cmd.args(["-y", "-f", "s16le", "-ar", "48000", "-ac", &channels.to_string(), "-i"])
            .arg(format!("tcp://127.0.0.1:{port}"))
            .args(["-f", "rawvideo", "-pix_fmt", "rgba", "-s"])
            .arg(format!("{}x{}", s.w, s.h))
            .args(["-r", &s.fps.to_string(), "-i", "pipe:0"])
            .args(["-map", "1:v"])
            .args(audio_maps(tracks));
        let live = matches!(s.target, Target::Srt(_) | Target::Growing(_));
        add_quality(&mut cmd, &s, live);
        match &s.target {
            Target::File { path, split_minutes } => add_file_target(&mut cmd, path, *split_minutes),
            Target::Rtmp(urls) if urls.len() == 1 => {
                cmd.args(["-f", "flv", "-flvflags", "no_duration_filesize"]).arg(&urls[0]);
            }
            Target::Rtmp(urls) => {
                cmd.args(["-flags", "+global_header", "-f", "tee"]).arg(tee_list(urls));
            }
            Target::Srt(url) => {
                cmd.args(["-f", "mpegts"]).arg(url);
            }
            Target::Growing(path) => {
                // Read while it grows: held back in a buffer, the latest
                // seconds would not exist yet for a replay.
                cmd.args(["-flush_packets", "1", "-f", "mpegts"]).arg(path);
            }
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| t!("output.ffmpeg_missing").to_string())?;

        let failed = Arc::new(AtomicBool::new(false));
        let log = Arc::new(Mutex::new(String::new()));
        let warning = Arc::new(Mutex::new(None));
        if let Some(err) = child.stderr.take() {
            let (sink, warn) = (log.clone(), warning.clone());
            thread::spawn(move || {
                let mut found_cause = false;
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    // One tee destination dropping out leaves the others running.
                    if line.contains("Slave") && line.to_lowercase().contains("fail") {
                        *warn.lock().unwrap() = Some(t!("output.destination_not_responding").to_string());
                    }
                    if is_cause(&line) && !found_cause {
                        found_cause = true;
                        *sink.lock().unwrap() = line;
                    } else if !found_cause {
                        *sink.lock().unwrap() = line;
                    }
                }
            });
        }

        // The file starts once ffmpeg has both inputs open. From then on each
        // picture and each block of sound goes where its stamp says: counted
        // as they come, a picture lost to a full queue would move every later
        // one a frame earlier than its sound.
        let began = Arc::new(AtomicU64::new(NOT_YET));
        let repeated = Arc::new(AtomicU64::new(0));
        let mut pipe = child.stdin.take().unwrap();
        let (vtx, vrx) = sync_channel::<(u64, Arc<Frame>)>(6);
        let (fail, start, again, fps) = (failed.clone(), began.clone(), repeated.clone(), s.fps);
        let pictures = thread::spawn(move || {
            let mut line: Option<FrameLine> = None;
            let mut last: Option<Arc<Frame>> = None;
            while let Ok((at, f)) = vrx.recv() {
                let origin = start.load(Ordering::Acquire);
                if origin == NOT_YET {
                    continue;
                }
                let Some(gap) = line.get_or_insert_with(|| FrameLine::new(origin, fps)).place(at) else {
                    continue;
                };
                let fill = last.as_ref().unwrap_or(&f);
                for _ in 0..gap {
                    if pipe.write_all(&fill.data).is_err() {
                        fail.store(true, Ordering::Relaxed);
                        return None;
                    }
                }
                again.fetch_add(gap, Ordering::Relaxed);
                if pipe.write_all(&f.data).is_err() {
                    fail.store(true, Ordering::Relaxed);
                    return None;
                }
                last = Some(f);
            }
            Some(PictureTail { pipe, last, frames: line.map_or(0, |l| l.written()) })
        });

        let (atx, arx) = sync_channel::<(u64, Vec<i16>)>(100);
        let (fail, start) = (failed.clone(), began.clone());
        let sound = thread::spawn(move || {
            let _ = listener.set_nonblocking(true);
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut sock = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    // Stopped before ffmpeg was ready: there is no file to end.
                    Err(_) if matches!(arx.try_recv(), Err(TryRecvError::Disconnected)) => return None,
                    Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                    Err(_) => {
                        fail.store(true, Ordering::Relaxed);
                        return None;
                    }
                }
            };
            let _ = sock.set_nonblocking(false);
            let origin = clock::now();
            start.store(origin, Ordering::Release);
            let mut line = SampleLine::new(origin);
            let mut bytes = Vec::new();
            while let Ok((at, block)) = arx.recv() {
                let fit = line.place(at, block.len() / channels);
                bytes.clear();
                bytes.resize(fit.silence as usize * channels * 2, 0);
                bytes.extend(block[fit.skip * channels..].iter().flat_map(|v| v.to_le_bytes()));
                if sock.write_all(&bytes).is_err() {
                    fail.store(true, Ordering::Relaxed);
                    return None;
                }
            }
            Some(SoundTail { sock, samples: line.written() })
        });

        Ok(Self {
            video: Some(vtx),
            audio: Some(atx),
            writers: Some((pictures, sound)),
            size: (s.w, s.h),
            fps: s.fps,
            channels,
            label: s.label,
            codec: s.codec,
            pick: s.audio,
            dropped: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            repeated,
            failed,
            log,
            warning,
            child: Arc::new(Mutex::new(Some(child))),
            began,
        })
    }

    /// When the file starts: the moment ffmpeg was ready for both inputs.
    pub fn began(&self) -> Option<Instant> {
        let at = self.began.load(Ordering::Acquire);
        (at != NOT_YET).then(|| clock::instant(at))
    }

    /// `at` is the time on the shared clock the picture stands for.
    pub fn push_video(&self, f: Arc<Frame>, at: u64) {
        // A frame of another size would scramble the raw stream.
        if (f.w, f.h) != self.size {
            return;
        }
        if let Some(tx) = &self.video {
            match tx.try_send((at, f)) {
                Ok(()) => {
                    self.sent.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Full(_)) => {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => self.failed.store(true, Ordering::Relaxed),
            }
        }
    }

    /// `at` is the time on the shared clock of the block's first frame.
    pub fn push_audio(&self, s: Vec<i16>, at: u64) {
        if let Some(tx) = &self.audio {
            let _ = tx.try_send((at, s));
        }
    }

    /// The error to show, once the process gave up.
    pub fn failure(&self) -> Option<String> {
        let exited = self.child.lock().unwrap().as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(Some(_))));
        (exited || self.failed.load(Ordering::Relaxed)).then(|| self.log.lock().unwrap().clone())
    }

    /// Something went wrong without stopping this output.
    pub fn take_warning(&self) -> Option<String> {
        self.warning.lock().unwrap().take()
    }

    /// Closes the pipes so ffmpeg finishes the file. Returns once it has, or
    /// after `patience`, in which case it is stopped.
    pub fn finish(mut self, patience: Duration) -> bool {
        self.video = None;
        self.audio = None;
        let deadline = Instant::now() + patience;
        if let Some((pictures, sound)) = self.writers.take()
            && let (Some(p), Some(s)) = (join_by(pictures, deadline), join_by(sound, deadline))
        {
            even_ends(p, s, self.fps, self.channels);
        }
        let Some(mut child) = self.child.lock().unwrap().take() else { return false };
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
                _ => {
                    end_process_tree(&mut child);
                    let _ = child.wait();
                    return false;
                }
            }
        }
    }
}

/// Encoders this ffmpeg offers, software ones first.
pub fn list_encoders() -> Vec<String> {
    let mut found = vec!["libx264".to_string()];
    let Ok(out) = ffmpeg().args(["-encoders"]).stdin(Stdio::null()).stderr(Stdio::null()).output() else {
        return found;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    for name in [
        "libx265",
        "libsvtav1",
        "h264_nvenc",
        "hevc_nvenc",
        "av1_nvenc",
        "h264_qsv",
        "hevc_qsv",
        "h264_amf",
        "hevc_amf",
    ] {
        if text.contains(name) {
            found.push(name.to_string());
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tee_keeps_every_destination_apart() {
        let urls = ["rtmp://a/live/k1".to_string(), "rtmp://b/live/k|2".to_string()];
        let list = tee_list(&urls);
        assert_eq!(list.matches("[f=flv").count(), 2);
        assert!(list.contains("k\\|2"));
    }

    #[test]
    fn paired_buses_become_separate_tracks() {
        let args = audio_maps(2).join(" ");
        assert!(args.contains("pan=stereo|c0=c0|c1=c1[a0]"));
        assert!(args.contains("pan=stereo|c0=c2|c1=c3[a1]"));
        assert_eq!(args.matches("-map").count(), 2);
    }

    #[test]
    fn the_cause_beats_the_closing_line() {
        assert!(is_cause("Connection to tcp://x failed: Connection refused"));
        assert!(!is_cause("Conversion failed!"));
    }
}
