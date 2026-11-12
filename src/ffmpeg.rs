//! Which ffmpeg LiveCraft runs, and what it can do. The program is looked for
//! once, in this order: the `LIVECRAFT_FFMPEG` variable (the path of the
//! ffmpeg executable), the folder of LiveCraft itself, then `PATH`. ffprobe is
//! taken from beside the ffmpeg that was found. Every child that is ffmpeg or
//! ffprobe starts from the path resolved here.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// The variable naming the ffmpeg executable.
pub const VARIABLE: &str = "LIVECRAFT_FFMPEG";

/// How long one question to ffmpeg may take, an encoder test included.
const ASK_LIMIT: Duration = Duration::from_secs(15);

/// How long a child that was told to end, or that closed its output, may take
/// to be gone before it is given up on: one stuck in a driver call cannot exit.
const GRACE: Duration = Duration::from_secs(3);

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

/// What the lookup came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup {
    Found(Located),
    /// The variable is set (this is its cleaned value) and names nothing usable:
    /// the search stops there, so a wrong setting is never hidden by `PATH`.
    BadVariable(String),
    NotFound,
}

/// A file that can be run: on Unix it must carry an execute bit, as `execvp`
/// requires, so a plain file of that name does not stop the search.
fn usable(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else { return false };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    true
}

fn absolute(p: PathBuf) -> PathBuf {
    std::path::absolute(&p).unwrap_or(p)
}

/// The variable without the quotes and spaces people paste around a path.
fn clean(raw: &OsStr) -> String {
    raw.to_string_lossy().trim().trim_matches('"').trim().to_string()
}

/// The lookup itself, from its inputs: `variable` is the value of
/// `LIVECRAFT_FFMPEG`, `own_dir` the folder of LiveCraft, `path` the value of `PATH`.
/// Only absolute places count: an empty or relative `PATH` entry (a trailing
/// separator, `.`) would otherwise make the current folder a source of programs.
pub fn locate_in(variable: Option<&OsStr>, own_dir: Option<&Path>, path: Option<&OsStr>) -> Lookup {
    let name = crate::platform::exe_name("ffmpeg");
    if let Some(v) = variable.map(clean).filter(|v| !v.is_empty()) {
        let p = PathBuf::from(&v);
        // A folder holding ffmpeg is a natural thing to give; a bare name is relative.
        let found =
            if p.is_absolute() { [p.clone(), p.join(&name)].into_iter().find(|c| usable(c)) } else { None };
        return match found {
            Some(f) => Lookup::Found(Located { path: absolute(f), how: How::Variable }),
            None => Lookup::BadVariable(v),
        };
    }
    if let Some(dir) = own_dir.filter(|d| d.is_absolute()) {
        let p = dir.join(&name);
        if usable(&p) {
            return Lookup::Found(Located { path: absolute(p), how: How::Beside });
        }
    }
    let Some(path) = path else { return Lookup::NotFound };
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(&name))
        .find(|p| usable(p))
        .map_or(Lookup::NotFound, |p| Lookup::Found(Located { path: absolute(p), how: How::Path }))
}

/// Where ffmpeg is, resolved on the first question and kept.
pub fn lookup() -> &'static Lookup {
    static FOUND: OnceLock<Lookup> = OnceLock::new();
    FOUND.get_or_init(|| {
        let own = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf));
        locate_in(std::env::var_os(VARIABLE).as_deref(), own.as_deref(), std::env::var_os("PATH").as_deref())
    })
}

/// The ffmpeg that was found, if one was.
pub fn located() -> Option<&'static Located> {
    match lookup() {
        Lookup::Found(f) => Some(f),
        _ => None,
    }
}

/// The program to start for `tool` (`ffmpeg`, `ffprobe`): the one beside the
/// ffmpeg that was found, else the bare name, left to the system. A variable
/// that names nothing usable is passed on as it is, so starting fails there
/// and ffmpeg is not quietly taken from elsewhere.
pub fn program(tool: &str) -> OsString {
    let name = crate::platform::exe_name(tool);
    match lookup() {
        Lookup::Found(found) if tool == "ffmpeg" => found.path.clone().into_os_string(),
        Lookup::Found(found) => match found.path.parent().map(|d| d.join(&name)) {
            Some(beside) if usable(&beside) => beside.into_os_string(),
            _ => OsString::from(name),
        },
        Lookup::BadVariable(v) if tool == "ffmpeg" => OsString::from(v),
        _ => OsString::from(name),
    }
}

/// What the ffmpeg found says it can do, from questions that cost nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// `None` when ffmpeg did not answer `-version`: it did not start, or
    /// crashed (missing libraries), or hung.
    pub version: Option<String>,
    /// `srt` is among its protocols.
    pub srt: bool,
    /// Encoders it lists, software ones first, libx264 leading. Listed is not
    /// proven to work: hardware ones are only tried when asked.
    pub encoders: Vec<String>,
}

impl Capabilities {
    pub fn answered(&self) -> bool {
        self.version.is_some()
    }

    /// The listed encoders that are tied to hardware.
    pub fn hardware(&self) -> Vec<String> {
        self.encoders.iter().filter(|e| HARDWARE.contains(&e.as_str())).cloned().collect()
    }

    fn silent() -> Self {
        Self { encoders: vec!["libx264".into()], ..Self::default() }
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

/// Waits for a child that has been told to end, or has closed its output, but
/// only so long: `None` means it is abandoned, and nothing waits on it again.
pub(crate) fn finish(child: &mut Child, grace: Duration) -> Option<ExitStatus> {
    let end = Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < end => thread::sleep(Duration::from_millis(20)),
            _ => return None,
        }
    }
}

fn ask(args: &[&str]) -> Option<String> {
    let mut cmd = crate::sources::ffmpeg();
    cmd.args(args);
    crate::decode::run_limited(cmd, ASK_LIMIT)
}

fn probe() -> Capabilities {
    if located().is_none() {
        return Capabilities::silent();
    }
    let Some(version) = ask(&["-version"]).and_then(|t| parse_version(&t)) else {
        return Capabilities::silent();
    };
    let srt = ask(&["-protocols"]).is_some_and(|t| parse_srt(&t));
    let listed = ask(&["-encoders"]).map(|t| parse_encoders(&t)).unwrap_or_default();
    let has = |n: &&&str| listed.iter().any(|l| l == **n);
    let mut encoders = vec!["libx264".to_string()];
    encoders.extend(SOFTWARE.iter().filter(has).chain(HARDWARE.iter().filter(has)).map(|n| n.to_string()));
    Capabilities { version: Some(version), srt, encoders }
}

static CAPABILITIES: OnceLock<Capabilities> = OnceLock::new();

/// Asks ffmpeg once (`-version`, `-protocols`, `-encoders`) and keeps the
/// answer; the first caller waits for it, so call this off the interface
/// thread. Whatever happens it ends, and the answer is then known.
pub fn capabilities() -> &'static Capabilities {
    CAPABILITIES.get_or_init(|| std::panic::catch_unwind(probe).unwrap_or_else(|_| Capabilities::silent()))
}

/// The answer if it is in already; never waits.
pub fn capabilities_if_known() -> Option<&'static Capabilities> {
    CAPABILITIES.get()
}

/// How one hardware encoder did when it was really tried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Test {
    Works,
    /// With the first line ffmpeg said (empty when it said nothing).
    Failed(String),
}

struct Tests {
    started: bool,
    running: bool,
    results: Vec<(String, Test)>,
}

static TESTS: Mutex<Tests> = Mutex::new(Tests { started: false, running: false, results: Vec::new() });

#[derive(Clone, Debug, Default)]
pub struct TestView {
    /// Tried at least once since the start.
    pub started: bool,
    pub running: bool,
    pub results: Vec<(String, Test)>,
}

pub fn test_view() -> TestView {
    let t = TESTS.lock().unwrap_or_else(|e| e.into_inner());
    TestView { started: t.started, running: t.running, results: t.results.clone() }
}

/// First line of what ffmpeg said, shortened.
pub fn first_line(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    line.chars().take(160).collect()
}

/// Opens each listed hardware encoder on a small picture, on a thread of its
/// own, and keeps what came of it. Real encoders are heavy and may fail once
/// (a session limit while another program encodes), so this runs only when
/// asked, and a failure only annotates: the encoder stays on offer. Does
/// nothing while a run is going or before `capabilities()` is in.
pub fn start_tests() {
    let Some(caps) = capabilities_if_known() else { return };
    let names = caps.hardware();
    {
        let mut t = TESTS.lock().unwrap_or_else(|e| e.into_inner());
        if t.running {
            return;
        }
        t.started = true;
        t.running = true;
    }
    thread::spawn(move || {
        let done = std::panic::catch_unwind(|| {
            names.iter().map(|n| (n.clone(), test_encoder(n))).collect::<Vec<_>>()
        });
        let mut t = TESTS.lock().unwrap_or_else(|e| e.into_inner());
        t.results = done.unwrap_or_default();
        t.running = false;
    });
}

fn test_encoder(encoder: &str) -> Test {
    let mut cmd = crate::sources::ffmpeg();
    cmd.args(["-v", "error", "-f", "lavfi", "-i", "color=black:s=256x256:d=0.1", "-frames:v", "1", "-c:v"])
        .arg(encoder)
        .args(["-f", "null", "-"]);
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    let Ok(mut child) = cmd.spawn() else { return Test::Failed(String::new()) };
    let Some(mut err) = child.stderr.take() else { return Test::Failed(String::new()) };
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = err.read_to_end(&mut bytes);
        let _ = tx.send(String::from_utf8_lossy(&bytes).into_owned());
    });
    let Ok(said) = rx.recv_timeout(ASK_LIMIT) else {
        crate::platform::end_process_tree(&mut child);
        let _ = finish(&mut child, GRACE);
        return Test::Failed(String::new());
    };
    match finish(&mut child, GRACE) {
        Some(status) if status.success() => Test::Works,
        _ => Test::Failed(first_line(&said)),
    }
}

/// What the person should be told about, when something is wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// No ffmpeg anywhere it is looked for.
    Missing,
    /// The variable is set and names nothing usable (its value).
    BadVariable(String),
    /// Found, but it did not answer: broken file, missing libraries, hung.
    NoAnswer,
    /// An SRT input or output is set up and this ffmpeg cannot do SRT.
    NoSrt,
}

/// `caps` is what ffmpeg said it can do (when it has said), `srt_used`
/// whether the project has an SRT input or output.
pub fn problem(lookup: &Lookup, caps: Option<&Capabilities>, srt_used: bool) -> Option<Problem> {
    match lookup {
        Lookup::NotFound => Some(Problem::Missing),
        Lookup::BadVariable(v) => Some(Problem::BadVariable(v.clone())),
        Lookup::Found(_) => match caps {
            Some(c) if !c.answered() => Some(Problem::NoAnswer),
            Some(c) if srt_used && !c.srt => Some(Problem::NoSrt),
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn exe(dir: &Path) -> PathBuf {
        let p = dir.join(crate::platform::exe_name("ffmpeg"));
        fs::write(&p, b"x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("livecraft-ffmpeg-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn found(path: PathBuf, how: How) -> Lookup {
        Lookup::Found(Located { path, how })
    }

    #[test]
    fn the_variable_beats_the_own_folder_and_path() {
        let (v, own, on_path) = (dir("v1"), dir("o1"), dir("p1"));
        let (want, _, _) = (exe(&v), exe(&own), exe(&on_path));
        let got = locate_in(Some(want.as_os_str()), Some(&own), Some(on_path.as_os_str()));
        assert_eq!(got, found(want, How::Variable));
    }

    #[test]
    fn a_folder_in_the_variable_is_searched() {
        let v = dir("v2");
        let want = exe(&v);
        assert_eq!(locate_in(Some(v.as_os_str()), None, None), found(want, How::Variable));
    }

    #[test]
    fn quotes_and_spaces_around_the_variable_are_dropped() {
        let v = dir("v7");
        let want = exe(&v);
        let pasted = format!("  \"{}\" ", want.display());
        assert_eq!(locate_in(Some(OsStr::new(&pasted)), None, None), found(want, How::Variable));
    }

    #[test]
    fn a_wrong_variable_stops_the_search_instead_of_falling_through() {
        let (own, on_path) = (dir("o3"), dir("p3"));
        exe(&own);
        exe(&on_path);
        let nothing = dir("v3").join("missing");
        let joined = std::env::join_paths([&on_path]).unwrap();
        let got = locate_in(Some(nothing.as_os_str()), Some(&own), Some(&joined));
        assert_eq!(got, Lookup::BadVariable(nothing.to_string_lossy().into_owned()));
    }

    #[test]
    fn a_bare_name_in_the_variable_is_relative_and_rejected() {
        let on_path = dir("p8");
        exe(&on_path);
        let joined = std::env::join_paths([&on_path]).unwrap();
        let got = locate_in(Some(OsStr::new("ffmpeg")), None, Some(&joined));
        assert_eq!(got, Lookup::BadVariable("ffmpeg".into()));
    }

    #[test]
    fn a_blank_variable_counts_as_unset() {
        let own = dir("o9");
        let want = exe(&own);
        assert_eq!(locate_in(Some(OsStr::new("  \"\" ")), Some(&own), None), found(want, How::Beside));
    }

    #[test]
    fn the_own_folder_beats_path() {
        let (own, on_path) = (dir("o4"), dir("p4"));
        let (want, _) = (exe(&own), exe(&on_path));
        let joined = std::env::join_paths([&on_path]).unwrap();
        assert_eq!(locate_in(None, Some(&own), Some(&joined)), found(want, How::Beside));
    }

    #[test]
    fn path_is_the_last_resort_and_nothing_is_not_found() {
        let (empty, on_path) = (dir("e5"), dir("p5"));
        let want = exe(&on_path);
        let joined = std::env::join_paths([&empty, &on_path]).unwrap();
        assert_eq!(locate_in(None, Some(&empty), Some(&joined)), found(want, How::Path));
        assert_eq!(locate_in(None, Some(&empty), Some(empty.as_os_str())), Lookup::NotFound);
        assert_eq!(locate_in(Some(OsStr::new("")), None, None), Lookup::NotFound);
    }

    #[test]
    fn empty_and_relative_path_entries_are_skipped() {
        // The current folder holds an ffmpeg: a trailing separator or a dot must not find it.
        let here = std::env::current_dir().unwrap();
        let name = crate::platform::exe_name("ffmpeg");
        let planted = here.join(&name);
        let made = !planted.exists() && fs::write(&planted, b"x").is_ok();
        let sep = if cfg!(windows) { ";" } else { ":" };
        let value = format!("{sep}.{sep}relative{sep}");
        let got = locate_in(None, None, Some(OsStr::new(&value)));
        if made {
            let _ = fs::remove_file(&planted);
        }
        assert_eq!(got, Lookup::NotFound);
    }

    #[cfg(unix)]
    #[test]
    fn a_file_without_the_execute_bit_does_not_count() {
        use std::os::unix::fs::PermissionsExt;
        let (own, on_path) = (dir("o10"), dir("p10"));
        let plain = exe(&own);
        fs::set_permissions(&plain, fs::Permissions::from_mode(0o644)).unwrap();
        let want = exe(&on_path);
        let joined = std::env::join_paths([&on_path]).unwrap();
        assert_eq!(locate_in(None, Some(&own), Some(&joined)), found(want, How::Path));
        assert_eq!(
            locate_in(Some(plain.as_os_str()), None, None),
            Lookup::BadVariable(plain.to_string_lossy().into())
        );
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
    fn hardware_means_the_listed_hardware_encoders() {
        let c = Capabilities {
            version: None,
            srt: true,
            encoders: vec!["libx264".into(), "libx265".into(), "h264_nvenc".into()],
        };
        assert_eq!(c.hardware(), ["h264_nvenc"]);
    }

    #[test]
    fn keeps_the_first_line_of_an_error() {
        let said = "\n[h264_nvenc @ 0x1] OpenEncodeSessionEx failed: out of memory (10)\n[vost#0:0] Error\n";
        assert_eq!(first_line(said), "[h264_nvenc @ 0x1] OpenEncodeSessionEx failed: out of memory (10)");
        assert_eq!(first_line(""), "");
        assert_eq!(first_line(&"x".repeat(500)).len(), 160);
    }

    #[test]
    fn says_what_is_wrong_only_when_it_matters() {
        let here = found(PathBuf::from("/x/ffmpeg"), How::Path);
        let with = Capabilities { version: Some("9".into()), srt: true, ..Capabilities::default() };
        let without = Capabilities { version: Some("9".into()), ..Capabilities::default() };
        let silent = Capabilities::default();
        assert_eq!(problem(&Lookup::NotFound, None, false), Some(Problem::Missing));
        assert_eq!(
            problem(&Lookup::BadVariable("v".into()), None, false),
            Some(Problem::BadVariable("v".into()))
        );
        assert_eq!(problem(&here, Some(&without), true), Some(Problem::NoSrt));
        assert_eq!(problem(&here, Some(&without), false), None);
        assert_eq!(problem(&here, Some(&with), true), None);
        assert_eq!(problem(&here, None, true), None, "not asked yet is not a fault");
        assert_eq!(
            problem(&here, Some(&silent), false),
            Some(Problem::NoAnswer),
            "no answer is its own fault"
        );
        assert_eq!(problem(&here, Some(&silent), true), Some(Problem::NoAnswer), "and not a missing SRT");
    }
}
