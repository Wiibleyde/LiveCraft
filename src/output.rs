//! Recording and streaming. One ffmpeg process per output takes raw pictures on
//! stdin and raw sound over a local socket, because stdin is its only pipe.
//! Several destinations at the same quality share one encode through ffmpeg's
//! tee muxer, which costs one encoder instead of five.

use crate::clock::{self, FrameLine, SampleLine};
use crate::frame::Frame;
use crate::model::AudioPick;
use crate::replay::FrameLog;
use crate::sources::{AUDIO_RATE, end_process_tree, ffmpeg};
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
    /// Sound per channel, in samples, taken in and refused for lack of room:
    /// set against `sent`, they show picture and sound drifting apart.
    pub audio_sent: AtomicU64,
    pub audio_dropped: AtomicU64,
    /// Pictures written again for periods no new one arrived in time for.
    pub repeated: Arc<AtomicU64>,
    /// Pictures of a stream cut after a long stall, with as much sound.
    pub cut: Arc<AtomicU64>,
    /// What finishing added for picture and sound to end together.
    pub padded: Arc<Mutex<Option<Padding>>>,
    failed: Arc<AtomicBool>,
    log: Arc<Mutex<String>>,
    warning: Arc<Mutex<Option<String>>>,
    child: Arc<Mutex<Option<Child>>>,
    pictures: Arc<Mutex<FrameLog>>,
}

/// Pictures and sound frames added at the end of a file. Both lines follow
/// the same clock, so more than a frame means they had drifted apart.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Padding {
    pub pictures: u64,
    pub samples: u64,
}

/// Where a file starts on the shared clock until ffmpeg is ready.
const NOT_YET: u64 = u64::MAX;

/// Writes `frames` of silence a second at a time at most: after the machine
/// slept or a virtual machine was paused, the gap can be hours long.
fn write_silence(out: &mut impl Write, frames: u64, channels: usize) -> std::io::Result<()> {
    let chunk = vec![0u8; (frames as usize).min(AUDIO_RATE) * channels * 2];
    let mut left = frames as usize * channels * 2;
    while left > 0 {
        let n = left.min(chunk.len());
        out.write_all(&chunk[..n])?;
        left -= n;
    }
    Ok(())
}

/// What a writer hands back once its queue is empty, so that sound and
/// picture can be brought to the same end before the pipes close.
struct PictureTail {
    pipe: ChildStdin,
    /// The last picture written and the time on the clock it stands for.
    last: Option<(u64, Arc<Frame>)>,
    frames: u64,
    log: Arc<Mutex<FrameLog>>,
}

struct SoundTail {
    sock: TcpStream,
    samples: u64,
}

/// A writer's result, or `Err` while it is still stuck on its pipe at `deadline`.
fn join_by<T>(job: JoinHandle<Option<T>>, deadline: Instant) -> Result<Option<T>, ()> {
    while !job.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    if job.is_finished() { Ok(job.join().ok().flatten()) } else { Err(()) }
}

/// What the shorter of picture and sound lacks to end with the other, once
/// there is a picture to repeat.
fn padding(p: &PictureTail, s: &SoundTail, fps: u32) -> Option<Padding> {
    p.last.as_ref()?;
    let (frames, samples) = clock::even_end(p.frames, s.samples, fps);
    Some(Padding { pictures: frames - p.frames, samples: samples - s.samples })
}

/// Pads the shorter of picture and sound to the end of the other: a file
/// ends with both, not with one of them missing for its last moment.
fn even_ends(mut p: PictureTail, mut s: SoundTail, pad: Padding, channels: usize) {
    let Some((at, last)) = p.last.take() else { return };
    for _ in 0..pad.pictures {
        if p.pipe.write_all(&last.data).is_err() {
            return;
        }
        p.log.lock().unwrap().push(clock::instant(at));
    }
    let _ = write_silence(&mut s.sock, pad.samples, channels);
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
        let cut = Arc::new(AtomicU64::new(0));
        // Time cut from a stream's pictures, which its sound loses as well.
        let cut_ns = Arc::new(AtomicU64::new(0));
        let pictures = Arc::new(Mutex::new(FrameLog::new(s.fps)));
        // After a long stall, a stream repeats half a second at most and
        // drops the rest of the gap: thousands of repeated pictures would
        // only push it further behind. A file keeps every repeat, so that
        // its sound stays whole.
        let most = match s.target {
            Target::Rtmp(_) | Target::Srt(_) => (s.fps / 2).max(1) as u64,
            Target::File { .. } | Target::Growing(_) => u64::MAX,
        };
        let mut pipe = child.stdin.take().unwrap();
        let (vtx, vrx) = sync_channel::<(u64, Arc<Frame>)>(6);
        let (fail, start, again, fps) = (failed.clone(), began.clone(), repeated.clone(), s.fps);
        let (taken, dropped_time, cut_count) = (pictures.clone(), cut_ns.clone(), cut.clone());
        let picture_writer = thread::spawn(move || {
            let mut line: Option<FrameLine> = None;
            let mut last: Option<(u64, Arc<Frame>)> = None;
            while let Ok((at, f)) = vrx.recv() {
                let origin = start.load(Ordering::Acquire);
                if origin == NOT_YET {
                    continue;
                }
                let line = line.get_or_insert_with(|| FrameLine::new(origin, fps));
                let before = line.cut();
                let Some(gap) = line.place_within(at, most) else {
                    continue;
                };
                if line.cut() > before {
                    cut_count.store(line.cut(), Ordering::Relaxed);
                    dropped_time.store(line.cut_ns(), Ordering::Release);
                }
                // A copy shows the picture before it, and is logged as taken
                // when that one was: a replay finds pictures by that moment.
                let (fill_at, fill) = last.as_ref().map_or((at, &f), |(t, p)| (*t, p));
                for _ in 0..gap {
                    if pipe.write_all(&fill.data).is_err() {
                        fail.store(true, Ordering::Relaxed);
                        return None;
                    }
                    taken.lock().unwrap().push(clock::instant(fill_at));
                }
                again.fetch_add(gap, Ordering::Relaxed);
                if pipe.write_all(&f.data).is_err() {
                    fail.store(true, Ordering::Relaxed);
                    return None;
                }
                taken.lock().unwrap().push(clock::instant(at));
                last = Some((at, f));
            }
            Some(PictureTail { pipe, last, frames: line.map_or(0, |l| l.written()), log: taken })
        });

        let (atx, arx) = sync_channel::<(u64, Vec<i16>)>(100);
        let (fail, start, dropped_time) = (failed.clone(), began.clone(), cut_ns.clone());
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
                let at = at.saturating_sub(dropped_time.load(Ordering::Acquire));
                let fit = line.place(at, block.len() / channels);
                bytes.clear();
                bytes.extend(block[fit.skip * channels..].iter().flat_map(|v| v.to_le_bytes()));
                if write_silence(&mut sock, fit.silence, channels)
                    .and_then(|()| sock.write_all(&bytes))
                    .is_err()
                {
                    fail.store(true, Ordering::Relaxed);
                    return None;
                }
            }
            Some(SoundTail { sock, samples: line.written() })
        });

        Ok(Self {
            video: Some(vtx),
            audio: Some(atx),
            writers: Some((picture_writer, sound)),
            size: (s.w, s.h),
            fps: s.fps,
            channels,
            label: s.label,
            codec: s.codec,
            pick: s.audio,
            dropped: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            audio_sent: AtomicU64::new(0),
            audio_dropped: AtomicU64::new(0),
            repeated,
            cut,
            padded: Arc::new(Mutex::new(None)),
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

    /// `at` is the time on the shared clock the picture stands for: the file
    /// places it, and its log notes it, by that time.
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
            let samples = (s.len() / (self.pick.count.max(1) as usize * 2)) as u64;
            let counter = if tx.try_send((at, s)).is_ok() { &self.audio_sent } else { &self.audio_dropped };
            counter.fetch_add(samples, Ordering::Relaxed);
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
        // The writers empty their queues and the ends are evened within the
        // patience. One still stuck on its pipe then means ffmpeg stopped
        // reading: it cannot finish the file, and waiting longer would hold
        // the stop, and closing the program, for good.
        let deadline = Instant::now() + patience;
        let closed = match self.writers.take() {
            Some((pictures, sound)) => match (join_by(pictures, deadline), join_by(sound, deadline)) {
                (Ok(Some(p)), Ok(Some(s))) => match padding(&p, &s, self.fps) {
                    Some(pad) => {
                        *self.padded.lock().unwrap() = Some(pad);
                        let channels = self.channels;
                        let job = thread::spawn(move || {
                            even_ends(p, s, pad, channels);
                            Some(())
                        });
                        join_by(job, deadline).is_ok()
                    }
                    None => true,
                },
                // A writer that gave up has dropped its pipe already.
                (p, s) => p.is_ok() && s.is_ok(),
            },
            None => true,
        };
        // With its inputs closed, ffmpeg writes the end of the file, which
        // gets its own patience: killed in the middle, an mp4 does not play.
        let waiting = Instant::now() + patience;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) if closed && Instant::now() < waiting => thread::sleep(Duration::from_millis(25)),
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

    /// An encoder of 1080p at 30 fps around `child` in place of ffmpeg, whose
    /// picture writer hands its pipe back after `busy`, `behind` pictures
    /// short of ten seconds of sound.
    fn finishing(mut child: Child, busy: Duration, behind: u64) -> Encoder {
        let pipe = child.stdin.take().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let sock = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut heard, _) = listener.accept().unwrap();
        thread::spawn(move || std::io::copy(&mut heard, &mut std::io::sink()));
        let log = Arc::new(Mutex::new(FrameLog::new(30)));
        let last = Some((0, Arc::new(Frame::new(1920, 1080))));
        let tail = PictureTail { pipe, last, frames: 300 - behind, log: log.clone() };
        let pictures = thread::spawn(move || {
            thread::sleep(busy);
            Some(tail)
        });
        let sound = thread::spawn(move || Some(SoundTail { sock, samples: 480_000 }));
        Encoder {
            video: None,
            audio: None,
            writers: Some((pictures, sound)),
            size: (1920, 1080),
            fps: 30,
            channels: 2,
            label: String::new(),
            codec: String::new(),
            pick: AudioPick::MASTER,
            dropped: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            audio_sent: AtomicU64::new(0),
            audio_dropped: AtomicU64::new(0),
            repeated: Arc::default(),
            cut: Arc::default(),
            padded: Arc::default(),
            failed: Arc::default(),
            log: Arc::default(),
            warning: Arc::default(),
            child: Arc::new(Mutex::new(Some(child))),
            pictures: log,
        }
    }

    /// Runs for about `seconds` and never reads its input.
    fn idle_process(seconds: u32) -> Child {
        Command::new("ping")
            .args(["-n", &(seconds + 1).to_string(), "127.0.0.1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    /// Writers that take most of the patience must not cost ffmpeg its time
    /// to write the end of the file: killed then, an mp4 does not play. One
    /// that never ends is given a whole patience of its own after the writers.
    #[test]
    fn the_end_of_the_file_gets_its_own_patience() {
        let enc = finishing(idle_process(120), Duration::from_secs(3), 0);
        let asked = Instant::now();
        assert!(!enc.finish(Duration::from_secs(8)));
        assert!(asked.elapsed() >= Duration::from_secs(11), "ended after {:?}", asked.elapsed());
    }

    /// An ffmpeg that stopped reading cannot hold the stop, nor closing the
    /// program: evening the ends waits no longer than the patience, where it
    /// used to wait for ffmpeg to go away by itself, here two minutes.
    #[test]
    fn a_stalled_encoder_cannot_hold_the_stop() {
        let enc = finishing(idle_process(120), Duration::ZERO, 30);
        let padded = enc.padded.clone();
        let (done, finished) = std::sync::mpsc::channel();
        thread::spawn(move || done.send(enc.finish(Duration::from_secs(1))));
        assert_eq!(finished.recv_timeout(Duration::from_secs(60)), Ok(false));
        assert_eq!(*padded.lock().unwrap(), Some(Padding { pictures: 30, samples: 0 }));
    }
}
