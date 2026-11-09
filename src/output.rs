//! Recording and streaming. One ffmpeg process per output takes raw pictures on
//! stdin and raw sound over a local socket, because stdin is its only pipe.
//! Several destinations at the same quality share one encode through ffmpeg's
//! tee muxer, which costs one encoder instead of five.

use crate::frame::Frame;
use crate::model::AudioPick;
use crate::replay::FrameLog;
use crate::sources::{end_process_tree, ffmpeg};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
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

/// ffmpeg's AAC encoder starts the sound 1024 samples early, and a file's time
/// starts with its earliest stream, so the first picture sits this far in.
pub const PICTURE_LEAD_S: f64 = 1024.0 / 48000.0;

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
    video: Option<SyncSender<(Arc<Frame>, Instant)>>,
    audio: Option<SyncSender<Vec<i16>>>,
    size: (usize, usize),
    pub label: String,
    pub codec: String,
    pub pick: AudioPick,
    pub dropped: AtomicU64,
    pub sent: AtomicU64,
    failed: Arc<AtomicBool>,
    log: Arc<Mutex<String>>,
    warning: Arc<Mutex<Option<String>>>,
    child: Arc<Mutex<Option<Child>>>,
    pictures: Arc<Mutex<FrameLog>>,
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
        let channels = (tracks * 2).to_string();
        // Sound is opened first: ffmpeg reads an input before opening the next,
        // and pictures only start once the sound socket is connected.
        let mut cmd = ffmpeg();
        cmd.args(["-y", "-f", "s16le", "-ar", "48000", "-ac", &channels, "-i"])
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
            .map_err(|_| "ffmpeg est introuvable : installez-le pour cette sortie".to_string())?;

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
                        *warn.lock().unwrap() = Some("une destination ne répond plus".to_string());
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

        // Pictures and sound start together, once ffmpeg has both inputs open.
        // Whatever piled up during its start-up would otherwise offset them.
        let ready = Arc::new(AtomicBool::new(false));
        let mut stdin = child.stdin.take().unwrap();
        let (vtx, vrx) = sync_channel::<(Arc<Frame>, Instant)>(6);
        let (fail, go) = (failed.clone(), ready.clone());
        let pictures = Arc::new(Mutex::new(FrameLog::new(s.fps)));
        let written = pictures.clone();
        thread::spawn(move || {
            while let Ok((f, taken)) = vrx.recv() {
                if !go.load(Ordering::Acquire) {
                    continue;
                }
                if stdin.write_all(&f.data).is_err() {
                    fail.store(true, Ordering::Relaxed);
                    break;
                }
                written.lock().unwrap().push(taken);
            }
        });

        let (atx, arx) = sync_channel::<Vec<i16>>(100);
        let fail = failed.clone();
        thread::spawn(move || {
            let _ = listener.set_nonblocking(true);
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut sock = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                    Err(_) => {
                        fail.store(true, Ordering::Relaxed);
                        return;
                    }
                }
            };
            let _ = sock.set_nonblocking(false);
            while arx.try_recv().is_ok() {}
            ready.store(true, Ordering::Release);
            let mut bytes = Vec::new();
            while let Ok(s) = arx.recv() {
                bytes.clear();
                bytes.extend(s.iter().flat_map(|v| v.to_le_bytes()));
                if sock.write_all(&bytes).is_err() {
                    fail.store(true, Ordering::Relaxed);
                    break;
                }
            }
        });

        Ok(Self {
            video: Some(vtx),
            audio: Some(atx),
            size: (s.w, s.h),
            label: s.label,
            codec: s.codec,
            pick: s.audio,
            dropped: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            failed,
            log,
            warning,
            child: Arc::new(Mutex::new(Some(child))),
            pictures,
        })
    }

    /// When each picture in the file was taken, which places a moment of the
    /// show in the file even after pictures were lost on the way.
    pub fn pictures(&self) -> Arc<Mutex<FrameLog>> {
        self.pictures.clone()
    }

    /// `taken` is the moment of the show the picture belongs to.
    pub fn push_video(&self, f: Arc<Frame>, taken: Instant) {
        // A frame of another size would scramble the raw stream.
        if (f.w, f.h) != self.size {
            return;
        }
        if let Some(tx) = &self.video {
            match tx.try_send((f, taken)) {
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

    pub fn push_audio(&self, s: Vec<i16>) {
        if let Some(tx) = &self.audio {
            let _ = tx.try_send(s);
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
        let Some(mut child) = self.child.lock().unwrap().take() else { return false };
        let t0 = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) if t0.elapsed() < patience => thread::sleep(Duration::from_millis(25)),
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
