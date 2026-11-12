//! Which ffmpeg LiveCraft runs, and what it can do. The program is looked for
//! once, in this order: the `LIVECRAFT_FFMPEG` variable (the path of the
//! ffmpeg executable), the folder of LiveCraft itself, then `PATH`. ffprobe is
//! taken from beside the ffmpeg that was found. Every child that is ffmpeg or
//! ffprobe starts from the path resolved here.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

/// The variable naming the ffmpeg executable.
pub const VARIABLE: &str = "LIVECRAFT_FFMPEG";

/// How long one question to ffmpeg may take, a hardware encoder test included.
const ASK_LIMIT: Duration = Duration::from_secs(15);

/// Encoders that need particular hardware: listed does not mean working.
const HARDWARE: [&str; 7] =
    ["h264_nvenc", "hevc_nvenc", "av1_nvenc", "h264_qsv", "hevc_qsv", "h264_amf", "hevc_amf"];

/// Software encoders offered besides libx264, when the build has them.
const SOFTWARE: [&str; 2] = ["libx265", "libsvtav1"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum How {
    /// Named by `LIVECRAFT_FFMPEG`.
    Variable,
    /// In the folder of the LiveCraft executable.
    Beside,
    /// Found on `PATH`.
    Path,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Located {
    pub path: PathBuf,
    pub how: How,
}

/// The lookup itself, from its inputs: `variable` is the value of
/// `LIVECRAFT_FFMPEG`, `own_dir` the folder of LiveCraft, `path` the value of `PATH`.
pub fn locate_in(variable: Option<&OsStr>, own_dir: Option<&Path>, path: Option<&OsStr>) -> Option<Located> {
    let name = crate::platform::exe_name("ffmpeg");
    if let Some(v) = variable.filter(|v| !v.is_empty()) {
        let p = PathBuf::from(v);
        if p.is_file() {
            return Some(Located { path: p, how: How::Variable });
        }
        // A folder holding ffmpeg is a natural thing to give.
        let inside = p.join(&name);
        if inside.is_file() {
            return Some(Located { path: inside, how: How::Variable });
        }
    }
    if let Some(dir) = own_dir {
        let p = dir.join(&name);
        if p.is_file() {
            return Some(Located { path: p, how: How::Beside });
        }
    }
    let path = path?;
    std::env::split_paths(path)
        .map(|dir| dir.join(&name))
        .find(|p| p.is_file())
        .map(|path| Located { path, how: How::Path })
}

/// Where ffmpeg is, resolved on the first question and kept.
pub fn located() -> Option<&'static Located> {
    static FOUND: OnceLock<Option<Located>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            let own = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf));
            locate_in(
                std::env::var_os(VARIABLE).as_deref(),
                own.as_deref(),
                std::env::var_os("PATH").as_deref(),
            )
        })
        .as_ref()
}

/// The program to start for `tool` (`ffmpeg`, `ffprobe`): the one beside the
/// ffmpeg that was found, else the bare name, left to the system.
pub fn program(tool: &str) -> OsString {
    let name = crate::platform::exe_name(tool);
    match located() {
        Some(found) if tool == "ffmpeg" => found.path.clone().into_os_string(),
        Some(found) => match found.path.parent().map(|d| d.join(&name)) {
            Some(beside) if beside.is_file() => beside.into_os_string(),
            _ => OsString::from(name),
        },
        None => OsString::from(name),
    }
}

/// What the ffmpeg found can do.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub version: Option<String>,
    /// `srt` is among its protocols.
    pub srt: bool,
    /// Encoders that start, software ones first, libx264 leading.
    pub encoders: Vec<String>,
}

impl Capabilities {
    /// The encoders tied to hardware that really started.
    pub fn hardware(&self) -> Vec<String> {
        self.encoders.iter().filter(|e| HARDWARE.contains(&e.as_str())).cloned().collect()
    }
}

/// `ffmpeg version 9.0.2-full_build Copyright …` gives `9.0.2-full_build`.
pub fn parse_version(text: &str) -> Option<String> {
    let line = text.lines().find(|l| l.starts_with("ffmpeg version"))?;
    line.split_whitespace().nth(2).map(str::to_string)
}

/// Whether `-protocols` lists `srt`, as input or output.
pub fn parse_srt(text: &str) -> bool {
    text.lines().any(|l| l.trim() == "srt")
}

/// The encoder names `-encoders` lists, after its header.
pub fn parse_encoders(text: &str) -> Vec<String> {
    let mut lines = text.lines().skip_while(|l| !l.trim_start().starts_with("------"));
    lines.next();
    lines.filter_map(|l| l.split_whitespace().nth(1)).map(str::to_string).collect()
}

fn ask(args: &[&str]) -> Option<String> {
    let mut cmd = crate::sources::ffmpeg();
    cmd.args(args);
    crate::decode::run_limited(cmd, ASK_LIMIT)
}

/// Whether an encoder produces a picture at all: a card that is absent or a
/// driver that is too old lists the encoder and fails here.
fn starts(encoder: &str) -> bool {
    let args = [
        "-f",
        "lavfi",
        "-i",
        "color=black:s=256x256:d=0.1",
        "-frames:v",
        "1",
        "-c:v",
        encoder,
        "-f",
        "null",
        "-",
    ];
    ask(&args).is_some()
}

fn probe() -> Capabilities {
    if located().is_none() {
        return Capabilities { encoders: vec!["libx264".into()], ..Capabilities::default() };
    }
    let version = ask(&["-version"]).and_then(|t| parse_version(&t));
    let srt = ask(&["-protocols"]).is_some_and(|t| parse_srt(&t));
    let listed = ask(&["-encoders"]).map(|t| parse_encoders(&t)).unwrap_or_default();
    let mut encoders = vec!["libx264".to_string()];
    encoders.extend(SOFTWARE.iter().filter(|n| listed.iter().any(|l| l == *n)).map(|n| n.to_string()));
    for name in HARDWARE.iter().filter(|n| listed.iter().any(|l| l == *n)) {
        if starts(name) {
            encoders.push(name.to_string());
        }
    }
    Capabilities { version, srt, encoders }
}

static CAPABILITIES: OnceLock<Capabilities> = OnceLock::new();

/// Asks ffmpeg once and keeps the answer; the first caller waits for it, so
/// call this off the interface thread.
pub fn capabilities() -> &'static Capabilities {
    CAPABILITIES.get_or_init(probe)
}

/// The answer if it is in already; never waits.
pub fn capabilities_if_known() -> Option<&'static Capabilities> {
    CAPABILITIES.get()
}

/// What the person should be told about, when something is wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    /// No ffmpeg anywhere it is looked for.
    Missing,
    /// An SRT input or output is set up and this ffmpeg cannot do SRT.
    NoSrt,
}

/// `found` is whether ffmpeg was located, `caps` what it said it can do (when
/// it has said), `srt_used` whether the project has an SRT input or output.
pub fn problem(found: bool, caps: Option<&Capabilities>, srt_used: bool) -> Option<Problem> {
    if !found {
        Some(Problem::Missing)
    } else if srt_used && caps.is_some_and(|c| !c.srt) {
        Some(Problem::NoSrt)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn exe(dir: &Path) -> PathBuf {
        let p = dir.join(crate::platform::exe_name("ffmpeg"));
        fs::write(&p, b"x").unwrap();
        p
    }

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("livecraft-ffmpeg-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_variable_beats_the_own_folder_and_path() {
        let (v, own, on_path) = (dir("v1"), dir("o1"), dir("p1"));
        let (want, _, _) = (exe(&v), exe(&own), exe(&on_path));
        let found = locate_in(Some(want.as_os_str()), Some(&own), Some(on_path.as_os_str())).unwrap();
        assert_eq!(found, Located { path: want, how: How::Variable });
    }

    #[test]
    fn a_folder_in_the_variable_is_searched() {
        let v = dir("v2");
        let want = exe(&v);
        assert_eq!(locate_in(Some(v.as_os_str()), None, None).unwrap().path, want);
    }

    #[test]
    fn a_wrong_variable_falls_through_to_the_own_folder() {
        let own = dir("o3");
        let want = exe(&own);
        let nothing = dir("v3").join("missing");
        let found = locate_in(Some(nothing.as_os_str()), Some(&own), None).unwrap();
        assert_eq!(found, Located { path: want, how: How::Beside });
    }

    #[test]
    fn the_own_folder_beats_path() {
        let (own, on_path) = (dir("o4"), dir("p4"));
        let (want, _) = (exe(&own), exe(&on_path));
        let joined = std::env::join_paths([&on_path]).unwrap();
        let found = locate_in(None, Some(&own), Some(&joined)).unwrap();
        assert_eq!(found, Located { path: want, how: How::Beside });
    }

    #[test]
    fn path_is_the_last_resort_and_nothing_is_none() {
        let (empty, on_path) = (dir("e5"), dir("p5"));
        let want = exe(&on_path);
        let joined = std::env::join_paths([&empty, &on_path]).unwrap();
        let found = locate_in(None, Some(&empty), Some(&joined)).unwrap();
        assert_eq!(found, Located { path: want, how: How::Path });
        assert_eq!(locate_in(None, Some(&empty), Some(empty.as_os_str())), None);
        assert_eq!(locate_in(Some(OsStr::new("")), None, None), None);
    }

    // Captured from ffmpeg 9.0.2 (gyan.dev full build), shortened.
    const VERSION: &str = "ffmpeg version 9.0.2-full_build-www.gyan.dev Copyright (c) 2000-2026 the FFmpeg developers\nbuilt with gcc 16.2.0\n";
    const PROTOCOLS: &str = "Supported file protocols:\nInput:\n  async\n  file\n  http\n  rist\n  srt\n  sftp\nOutput:\n  file\n  http\n  srt\n";
    const PROTOCOLS_OLD: &str =
        "Supported file protocols:\nInput:\n  file\n  http\n  srtp\nOutput:\n  file\n";
    const ENCODERS: &str = "Encoders:\n V..... = Video\n ------\n V..... libsvtav1            SVT-AV1 encoder (codec av1)\n V....D libx264              libx264 H.264 (codec h264)\n V....D h264_nvenc           NVIDIA NVENC H.264 encoder (codec h264)\n V..... h264_qsv             H.264 (Intel Quick Sync Video acceleration) (codec h264)\n";

    #[test]
    fn reads_the_version() {
        assert_eq!(parse_version(VERSION).as_deref(), Some("9.0.2-full_build-www.gyan.dev"));
        assert_eq!(parse_version("nothing here"), None);
    }

    #[test]
    fn finds_srt_among_the_protocols_but_not_srtp() {
        assert!(parse_srt(PROTOCOLS));
        assert!(!parse_srt(PROTOCOLS_OLD));
    }

    #[test]
    fn reads_encoder_names_after_the_header() {
        assert_eq!(parse_encoders(ENCODERS), ["libsvtav1", "libx264", "h264_nvenc", "h264_qsv"]);
    }

    #[test]
    fn only_working_hardware_counts_as_hardware() {
        let c = Capabilities {
            version: None,
            srt: true,
            encoders: vec!["libx264".into(), "libx265".into(), "h264_nvenc".into()],
        };
        assert_eq!(c.hardware(), ["h264_nvenc"]);
    }

    #[test]
    fn says_what_is_wrong_only_when_it_matters() {
        let with = Capabilities { srt: true, ..Capabilities::default() };
        let without = Capabilities::default();
        assert_eq!(problem(false, None, false), Some(Problem::Missing));
        assert_eq!(problem(true, Some(&without), true), Some(Problem::NoSrt));
        assert_eq!(problem(true, Some(&without), false), None);
        assert_eq!(problem(true, Some(&with), true), None);
        assert_eq!(problem(true, None, true), None, "not asked yet is not a fault");
    }
}
