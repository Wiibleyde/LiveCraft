//! Decoding on the graphics card, only where it was measured to pay off.
//!
//! On an RTX 4070 with a Ryzen 7 5800X, decoding and fitting a 4K H.264 or
//! HEVC file to 1080p kept 1.5 to 2.8 cores busy in software and 0.6 to 0.75
//! with D3D11. At 1080p the card saved at most a quarter of a core but made
//! every start and seek 0.2 to 1.5 s slower, which an operator feels on each
//! cue. So files go to the card only above 1080p60's worth of pixels; a file
//! not yet measured starts in software while ffprobe is asked on the side. A
//! network stream is opened once and not seeked, so it always goes to the card.

use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// What is known of one file: how it looked when asked about (a file that
/// changes is asked about again) and what ffprobe said.
struct Entry {
    stamp: Option<(SystemTime, u64)>,
    state: State,
}

enum State {
    /// ffprobe is reading it.
    Probing,
    Known(bool),
    /// ffprobe could not tell (a share that is down, a locked file). Not
    /// remembered for long: the next question after a while tries again.
    Failed(Instant),
}

static KNOWN: Mutex<BTreeMap<String, Entry>> = Mutex::new(BTreeMap::new());

/// How long ffprobe may take, and how long a failed answer is believed.
const PROBE_LIMIT: Duration = Duration::from_secs(10);
const RETRY_AFTER: Duration = Duration::from_secs(30);

/// The ffmpeg input options that move decoding to the graphics card.
pub type Hardware = &'static [&'static str];

/// D3D11 is offered by every vendor's driver on Windows. For a codec the card
/// does not know, ffmpeg quietly decodes in software instead. Linux and macOS
/// have no such common name, so decoding stays on the processor there.
#[cfg(windows)]
pub const CARD: Option<Hardware> = Some(&["-hwaccel", "d3d11va"]);
#[cfg(not(windows))]
pub const CARD: Option<Hardware> = None;

/// The most pixels per second still decoded by the processor: 1080p60.
const SOFTWARE_LIMIT: u64 = 1920 * 1080 * 60;

/// Codecs D3D11 decodes; anything else would only pay for opening the card.
const CARD_CODECS: [&str; 7] = ["h264", "hevc", "vp9", "av1", "mpeg2video", "vc1", "wmv3"];

/// What the first video stream of a file holds.
#[derive(Clone, Debug, PartialEq)]
struct Picture {
    codec: String,
    width: u64,
    height: u64,
    fps: f64,
}

impl Picture {
    /// Reads ffprobe's `codec_name,width,height,avg_frame_rate` line.
    fn parse(line: &str) -> Option<Self> {
        let mut f = line.trim().split(',');
        let codec = f.next()?.to_string();
        let width = f.next()?.parse().ok()?;
        let height = f.next()?.parse().ok()?;
        let fps = match f.next()?.split_once('/') {
            Some((n, d)) => {
                n.parse::<f64>().ok()? / d.parse::<f64>().ok().filter(|d| *d > 0.0).unwrap_or(1.0)
            }
            None => 0.0,
        };
        // Some containers do not say; the common rate is the fair guess.
        let fps = if fps > 0.0 { fps } else { 30.0 };
        Some(Self { codec, width, height, fps })
    }

    fn worth_the_card(&self) -> bool {
        CARD_CODECS.contains(&self.codec.as_str())
            && (self.width * self.height) as f64 * self.fps > SOFTWARE_LIMIT as f64 + 1.0
    }
}

/// Runs a command and returns what it printed, or `None` when it could not
/// start, failed, or did not finish within `limit` (its whole tree is then
/// ended, so a dead share cannot leave probes behind).
fn run_limited(mut cmd: Command, limit: Duration) -> Option<String> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().ok()?;
    let mut out = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut text = String::new();
        let mut bytes = Vec::new();
        if out.read_to_end(&mut bytes).is_ok() {
            text.push_str(&String::from_utf8_lossy(&bytes));
            let _ = tx.send(text);
        }
    });
    let text = rx.recv_timeout(limit).ok();
    if text.is_none() {
        crate::platform::end_process_tree(&mut child);
    }
    let status = child.wait().ok()?;
    if status.success() { text } else { None }
}

/// `Err` when ffprobe gave no answer, `Ok(None)` when the file has no picture.
fn probe(path: &str) -> Result<Option<Picture>, ()> {
    let mut cmd = Command::new("ffprobe");
    cmd.args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=codec_name,width,height,avg_frame_rate", "-of", "csv=p=0"])
        .arg(path);
    let text = run_limited(cmd, PROBE_LIMIT).ok_or(())?;
    Ok(text.lines().find_map(Picture::parse))
}

type Stamp = Option<(SystemTime, u64)>;

/// A file's modification time and size; `None` for what is not a local file.
fn stamp(path: &str) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

enum Need {
    Answer(Option<bool>),
    Probe,
}

/// Whether an entry still holds good for a file that looks like `stamp` now.
fn consult(entry: Option<&Entry>, stamp: Stamp, now: Instant) -> Need {
    match entry {
        None => Need::Probe,
        Some(e) if e.stamp != stamp => Need::Probe,
        Some(Entry { state: State::Probing, .. }) => Need::Answer(None),
        Some(Entry { state: State::Known(heavy), .. }) => Need::Answer(Some(*heavy)),
        Some(Entry { state: State::Failed(at), .. }) if now.duration_since(*at) >= RETRY_AFTER => Need::Probe,
        Some(Entry { state: State::Failed(_), .. }) => Need::Answer(None),
    }
}

/// Whether this file is heavy enough for the card, once known. The first
/// question starts finding out on the side: a picture never waits for it.
pub fn verdict(path: &str) -> Option<bool> {
    let now = stamp(path);
    {
        let known = KNOWN.lock().unwrap();
        if let Need::Answer(a) = consult(known.get(path), now, Instant::now()) {
            return a;
        }
    }
    ask_ahead(vec![path.to_string()]);
    None
}

/// Finds out about files that will play later, one after the other.
pub fn ask_ahead(paths: Vec<String>) {
    let fresh: Vec<(String, Stamp)> = {
        let mut known = KNOWN.lock().unwrap();
        let mut fresh = Vec::new();
        for path in paths {
            let now = stamp(&path);
            if let Need::Probe = consult(known.get(&path), now, Instant::now()) {
                known.insert(path.clone(), Entry { stamp: now, state: State::Probing });
                fresh.push((path, now));
            }
        }
        fresh
    };
    if fresh.is_empty() {
        return;
    }
    thread::spawn(move || {
        for (path, stamp) in fresh {
            let state = match probe(&path) {
                Ok(picture) => State::Known(picture.is_some_and(|p| p.worth_the_card())),
                Err(()) => State::Failed(Instant::now()),
            };
            KNOWN.lock().unwrap().insert(path, Entry { stamp, state });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_ffprobe_says() {
        let p = Picture::parse("hevc,3840,2160,30000/1001\r").unwrap();
        assert_eq!((p.codec.as_str(), p.width, p.height), ("hevc", 3840, 2160));
        assert!((p.fps - 29.97).abs() < 0.01);
        assert_eq!(Picture::parse("h264,1920,1080,0/0").unwrap().fps, 30.0, "unknown rate");
        assert_eq!(Picture::parse(""), None);
        assert_eq!(Picture::parse("png,N/A,N/A,25/1"), None);
    }

    #[test]
    fn asking_again_keeps_what_is_known() {
        let path = "C:/déjà/lu.mp4";
        KNOWN.lock().unwrap().insert(path.into(), Entry { stamp: None, state: State::Known(true) });
        ask_ahead(vec![path.into(), path.into()]);
        assert_eq!(verdict(path), Some(true), "must not be asked again and forgotten meanwhile");
    }

    #[test]
    fn a_changed_file_is_asked_about_again() {
        let then = Some((SystemTime::UNIX_EPOCH + Duration::from_secs(10), 100));
        let known = Entry { stamp: then, state: State::Known(true) };
        let now = Instant::now();
        assert!(matches!(consult(Some(&known), then, now), Need::Answer(Some(true))));
        let rewritten = Some((SystemTime::UNIX_EPOCH + Duration::from_secs(20), 100));
        assert!(matches!(consult(Some(&known), rewritten, now), Need::Probe), "new modification time");
        let longer = Some((SystemTime::UNIX_EPOCH + Duration::from_secs(10), 200));
        assert!(matches!(consult(Some(&known), longer, now), Need::Probe), "new size");
        assert!(matches!(consult(None, then, now), Need::Probe));
    }

    #[test]
    fn a_failed_probe_is_tried_again_after_a_while() {
        let failed = Entry { stamp: None, state: State::Failed(Instant::now()) };
        assert!(matches!(consult(Some(&failed), None, Instant::now()), Need::Answer(None)));
        let later = Instant::now() + RETRY_AFTER;
        assert!(matches!(consult(Some(&failed), None, later), Need::Probe));
    }

    // Uses `cmd` and `ping`.
    #[cfg(windows)]
    #[test]
    fn a_probe_that_hangs_is_ended() {
        let mut cmd = Command::new("ping");
        cmd.args(["-n", "100", "127.0.0.1"]);
        let started = Instant::now();
        assert_eq!(run_limited(cmd, Duration::from_millis(300)), None);
        // The command would run for 100 s; the slack is for a machine under load.
        assert!(started.elapsed() < Duration::from_secs(40), "must not wait for the command");
        let mut quick = Command::new("cmd");
        quick.args(["/c", "echo", "hello"]);
        assert_eq!(run_limited(quick, Duration::from_secs(10)).as_deref().map(str::trim), Some("hello"));
    }

    #[test]
    fn only_heavy_pictures_go_to_the_card() {
        let at = |line: &str| Picture::parse(line).unwrap().worth_the_card();
        assert!(at("hevc,3840,2160,25/1"), "4K is where the card wins");
        assert!(at("h264,2560,1440,60/1"));
        assert!(!at("h264,1920,1080,60/1"), "1080p60 starts faster in software");
        assert!(!at("h264,1920,1080,30/1"));
        assert!(!at("h264,1280,720,60/1"));
        assert!(!at("prores,3840,2160,25/1"), "the card cannot decode it");
    }
}
