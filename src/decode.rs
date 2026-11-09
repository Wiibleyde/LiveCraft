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
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;

/// What is known of each file asked about: `None` while ffprobe is reading it.
/// A file's weight only changes if the file does, so once per run is enough.
static KNOWN: Mutex<BTreeMap<String, Option<bool>>> = Mutex::new(BTreeMap::new());

/// The ffmpeg input options that move decoding to the graphics card.
pub type Hardware = &'static [&'static str];

/// D3D11 is offered by every vendor's driver on Windows. For a codec the card
/// does not know, ffmpeg quietly decodes in software instead.
pub const CARD: Hardware = &["-hwaccel", "d3d11va"];

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

fn probe(path: &str) -> Option<Picture> {
    let mut cmd = Command::new("ffprobe");
    cmd.args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=codec_name,width,height,avg_frame_rate", "-of", "csv=p=0"])
        .arg(path)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.output().ok()?;
    String::from_utf8_lossy(&out.stdout).lines().find_map(Picture::parse)
}

/// Whether this file is heavy enough for the card, once known. The first
/// question starts finding out on the side: a picture never waits for it.
pub fn verdict(path: &str) -> Option<bool> {
    if let Some(known) = KNOWN.lock().unwrap().get(path) {
        return *known;
    }
    ask_ahead(vec![path.to_string()]);
    None
}

/// Finds out about files that will play later, one after the other.
pub fn ask_ahead(paths: Vec<String>) {
    let fresh: Vec<String> = {
        let mut known = KNOWN.lock().unwrap();
        let mut fresh = Vec::new();
        for path in paths {
            if !known.contains_key(&path) {
                known.insert(path.clone(), None);
                fresh.push(path);
            }
        }
        fresh
    };
    if fresh.is_empty() {
        return;
    }
    thread::spawn(move || {
        for path in fresh {
            let heavy = probe(&path).is_some_and(|p| p.worth_the_card());
            KNOWN.lock().unwrap().insert(path, Some(heavy));
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
        KNOWN.lock().unwrap().insert(path.into(), Some(true));
        ask_ahead(vec![path.into(), path.into()]);
        assert_eq!(verdict(path), Some(true), "must not be asked again and forgotten meanwhile");
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
