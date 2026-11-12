//! PowerPoint inputs. A presentation becomes one picture per slide, once, on
//! the input's own thread, through whichever presentation program the machine
//! has; the slides are then shown like photos and moved on command. Neither
//! program is shipped: they are started from where they are installed, as
//! ffmpeg is.

use crate::sources::{InputRt, end_process_tree};
use crate::titles;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

fn no_converter() -> &'static str {
    t!("slides.no_converter")
}

/// Files offered and recognised as presentations.
pub const EXTENSIONS: [&str; 6] = ["pptx", "ppt", "pptm", "ppsx", "pps", "odp"];

const POWERPOINT_SCRIPT: &str = include_str!("slides_powerpoint.ps1");
const LIBREOFFICE_SCRIPT: &str = include_str!("slides_libreoffice.py");

/// Slides made more than this long ago are thrown away on the next conversion.
const KEEP_CACHE: Duration = Duration::from_secs(30 * 24 * 3600);

/// A program asked to stop gets this long to close the presentation itself:
/// killed halfway, PowerPoint would stay behind with the file open.
const STOP_GRACE: Duration = Duration::from_secs(20);

/// Nothing heard for this long, the program is taken as stuck. The first
/// LibreOffice start builds its profile, which takes half a minute.
const SILENCE: Duration = Duration::from_secs(120);

/// One conversion at a time: PowerPoint is a single shared process, and two
/// presentations opening at once would fight over it.
static TURN: Mutex<()> = Mutex::new(());

pub fn is_presentation(path: &str) -> bool {
    Path::new(path).extension().is_some_and(|e| EXTENSIONS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

#[derive(Clone, Debug, PartialEq)]
pub enum Converter {
    PowerPoint,
    /// Path to `soffice.exe`.
    LibreOffice(PathBuf),
}

impl Converter {
    /// PowerPoint first: it draws its own files exactly as the speaker saw them.
    pub fn find() -> Option<Converter> {
        if powerpoint_installed() {
            return Some(Converter::PowerPoint);
        }
        libreoffice().map(Converter::LibreOffice)
    }

    pub fn name(&self) -> &'static str {
        match self {
            Converter::PowerPoint => "PowerPoint",
            Converter::LibreOffice(_) => "LibreOffice",
        }
    }
}

fn no_window(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

/// PowerPoint is driven through automation, which it registers on install.
fn powerpoint_installed() -> bool {
    let mut c = Command::new("reg");
    c.args(["query", r"HKCR\PowerPoint.Application\CLSID"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    no_window(&mut c).status().is_ok_and(|s| s.success())
}

/// LibreOffice is rarely on the `PATH` on Windows: its usual folders count too.
pub fn libreoffice() -> Option<PathBuf> {
    let on_path: Vec<PathBuf> =
        std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    let installed = ["ProgramFiles", "ProgramFiles(x86)"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|d| PathBuf::from(d).join("LibreOffice").join("program"));
    on_path.into_iter().chain(installed).map(|d| d.join("soffice.exe")).find(|p| p.is_file())
}

fn cache_root() -> PathBuf {
    std::env::temp_dir().join("livecraft-slides")
}

/// Where the slides of this version of the file, at this size, are kept.
fn cache_dir(file: &Path, w: usize, h: usize) -> Option<PathBuf> {
    let meta = std::fs::metadata(file).ok().filter(|m| m.is_file())?;
    let mut hasher = DefaultHasher::new();
    (std::path::absolute(file).ok()?, meta.len(), meta.modified().ok(), w, h).hash(&mut hasher);
    Some(cache_root().join(format!("{:016x}", hasher.finish())))
}

fn slide_file(dir: &Path, n: usize) -> PathBuf {
    dir.join(format!("{n}.png"))
}

/// Slides already made, when a conversion went all the way through.
fn cached(dir: &Path) -> Option<usize> {
    let n: usize = std::fs::read_to_string(dir.join("count")).ok()?.trim().parse().ok()?;
    (n > 0 && (1..=n).all(|k| slide_file(dir, k).is_file())).then_some(n)
}

/// Slide folders a running input is showing: never pruned, however old.
static IN_USE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

struct InUse(PathBuf);

impl InUse {
    fn new(dir: &Path) -> Self {
        IN_USE.lock().unwrap_or_else(|e| e.into_inner()).push(dir.to_path_buf());
        InUse(dir.to_path_buf())
    }
}

impl Drop for InUse {
    fn drop(&mut self) {
        let mut all = IN_USE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = all.iter().position(|d| *d == self.0) {
            all.swap_remove(i);
        }
    }
}

/// A folder's age is the time since it was last made or used, not made: a deck
/// used every week must outlive the others.
fn used_now(dir: &Path) {
    let now = SystemTime::now();
    if let Ok(f) = std::fs::File::options().write(true).open(dir.join("count")) {
        let _ = f.set_modified(now);
    }
}

fn last_used(dir: &Path) -> Option<SystemTime> {
    let of = |p: PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    of(dir.join("count")).or_else(|| of(dir.to_path_buf()))
}

fn prune_cache(keep: &Path) {
    prune_in(&cache_root(), keep, KEEP_CACHE);
}

fn prune_in(root: &Path, keep: &Path, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    let now = SystemTime::now();
    let busy = IN_USE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    for e in entries.flatten() {
        // Only the slide folders: the LibreOffice profile is kept, as building
        // it again makes the next conversion slow.
        let slides = e.file_name().len() == 16 && e.path().is_dir();
        let old = last_used(&e.path()).is_some_and(|t| now.duration_since(t).is_ok_and(|age| age > max_age));
        if slides && old && e.path() != keep && !busy.contains(&e.path()) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Turns every visible slide of `file` into `1.png`, `2.png`… in `out`, each
/// fitting `w`×`h`. `progress` hears (done, total) after each slide; `stop`
/// asked, the program finishes the slide in hand and closes the file.
pub fn convert(
    conv: &Converter,
    file: &Path,
    out: &Path,
    w: usize,
    h: usize,
    stop: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<usize, String> {
    let file = std::path::absolute(file).map_err(|e| e.to_string())?;
    let mut cmd = match conv {
        Converter::PowerPoint => {
            let mut c = Command::new("powershell");
            c.args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &utf16_base64(POWERPOINT_SCRIPT)]);
            c
        }
        Converter::LibreOffice(soffice) => {
            let python = soffice.with_file_name("python.exe");
            if !python.is_file() {
                return Err(t!("slides.no_python").into());
            }
            let script = cache_root().join("export-slides.py");
            std::fs::create_dir_all(cache_root()).map_err(|e| e.to_string())?;
            std::fs::write(&script, LIBREOFFICE_SCRIPT).map_err(|e| e.to_string())?;
            let mut c = Command::new(python);
            c.arg(script)
                .env("LC_SLIDES_OFFICE", soffice)
                .env("LC_SLIDES_PROFILE", cache_root().join("profile"));
            c
        }
    };
    // Paths travel in the environment, never inside the script text.
    cmd.env("LC_SLIDES_IN", &file)
        .env("LC_SLIDES_OUT", out)
        .env("LC_SLIDES_W", w.to_string())
        .env("LC_SLIDES_H", h.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child =
        no_window(&mut cmd).spawn().map_err(|e| t!("slides.wont_start", program = conv.name(), error = e))?;
    let (tx, rx) = mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    // Read so a chatty program never blocks on a full pipe.
    if let Some(mut stderr) = child.stderr.take() {
        thread::spawn(move || std::io::copy(&mut stderr, &mut std::io::sink()));
    }
    let (mut total, mut done, mut failure) = (0, 0, None);
    let mut asked_to_stop: Option<Instant> = None;
    let mut heard = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                heard = Instant::now();
                let line = line.trim();
                if let Some(n) = line.strip_prefix("total ") {
                    total = n.parse().unwrap_or(0);
                } else if let Some(n) = line.strip_prefix("slide ") {
                    done = n.parse().unwrap_or(done);
                    progress(done, total);
                } else if let Some(msg) = line.strip_prefix("error ") {
                    // Menu accelerators come along in PowerPoint's messages.
                    let msg = msg.trim().trim_start_matches("Presentations.Open : ").replace('&', "");
                    failure = Some(t!("slides.unreadable", message = msg));
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if stop() && asked_to_stop.is_none() {
            let _ = std::fs::write(out.join("stop"), b"");
            asked_to_stop = Some(Instant::now());
        }
        if asked_to_stop.is_some_and(|t| t.elapsed() > STOP_GRACE) {
            end_process_tree(&mut child);
            end_started_powerpoint(out, "POWERPNT.EXE");
            break;
        }
        if asked_to_stop.is_none() && heard.elapsed() > SILENCE {
            end_process_tree(&mut child);
            end_started_powerpoint(out, "POWERPNT.EXE");
            failure = Some(t!("slides.silent", program = conv.name()));
            break;
        }
    }
    let ended = child.wait().map_err(|e| e.to_string())?;
    if asked_to_stop.is_some() {
        return Err(t!("slides.stopped").into());
    }
    if let Some(msg) = failure {
        return Err(msg);
    }
    if total == 0 && ended.success() {
        return Err(t!("slides.no_visible").into());
    }
    if done == 0 || done < total || !ended.success() {
        return Err(t!("slides.ended_early", program = conv.name()));
    }
    Ok(done)
}

/// PowerPoint is a COM server, not a child of the script: ending the script
/// leaves it open and hidden. The script writes the id of a PowerPoint it
/// started itself (never one the user had open) to `powerpoint.pid`.
///
/// The process must still be PowerPoint: if it already ended and Windows gave
/// its id to another program, that program is left alone.
fn end_started_powerpoint(out: &Path, image: &str) {
    let Some(pid) = std::fs::read_to_string(out.join("powerpoint.pid"))
        .ok()
        .and_then(|t| t.trim().parse::<u32>().ok())
        .filter(|p| *p > 4)
    else {
        return;
    };
    let mut kill = Command::new("taskkill");
    kill.args(["/T", "/F", "/FI", &format!("PID eq {pid}"), "/FI", &format!("IMAGENAME eq {image}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let _ = no_window(&mut kill).status();
}

/// What `-EncodedCommand` takes: the script as UTF-16, in base64. It avoids
/// any quoting, and scripts given this way do not need to be signed.
fn utf16_base64(text: &str) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for k in 0..4 {
            if k <= chunk.len() {
                out.push(ABC[(n >> (18 - 6 * k)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The input's source thread: converts if needed, then shows the slide asked for.
pub(crate) fn run(rt: &Arc<InputRt>, path: &str, w: usize, h: usize) {
    run_with(rt, path, w, h, Converter::find);
}

fn run_with(rt: &Arc<InputRt>, path: &str, w: usize, h: usize, find: impl FnOnce() -> Option<Converter>) {
    let Some(dir) = cache_dir(Path::new(path), w, h) else {
        rt.set_status(t!("slides.not_found"));
        return;
    };
    let count = match cached(&dir) {
        Some(n) => {
            used_now(&dir);
            n
        }
        None => match prepare(rt, Path::new(path), &dir, w, h, find) {
            Ok(n) => n,
            Err(e) => {
                rt.set_status(e);
                return;
            }
        },
    };
    rt.clear_status();
    let _in_use = InUse::new(&dir);
    show(rt, &dir, count, w, h);
}

fn prepare(
    rt: &InputRt,
    file: &Path,
    dir: &Path,
    w: usize,
    h: usize,
    find: impl FnOnce() -> Option<Converter>,
) -> Result<usize, String> {
    let conv = find().ok_or(no_converter())?;
    rt.set_status(t!("slides.waiting"));
    let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    if rt.stopped() {
        return Err(t!("slides.stopped").into());
    }
    // Another input may have just converted the same file.
    if let Some(n) = cached(dir) {
        used_now(dir);
        return Ok(n);
    }
    prune_cache(dir);
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|e| t!("slides.folder", error = e))?;
    rt.set_status(t!("slides.opening", program = conv.name()));
    // The slide the show stands on goes up as soon as it exists: a format
    // change converts again, and the screen must not fall back to slide 1.
    let wanted = rt.item.load(Ordering::Relaxed) as usize + 1;
    let mut early = false;
    let n = convert(&conv, file, dir, w, h, &|| rt.stopped(), &mut |done, total| {
        rt.set_status(t!("slides.converting", done = done, total = total));
        if !early && done >= wanted {
            early = true;
            if let Ok(f) = titles::load_image(&slide_file(dir, wanted).to_string_lossy(), w, h) {
                rt.publish(f);
            }
        }
    })?;
    std::fs::write(dir.join("count"), n.to_string()).map_err(|e| e.to_string())?;
    Ok(n)
}

fn show(rt: &InputRt, dir: &Path, count: usize, w: usize, h: usize) {
    rt.items.store(count as u64, Ordering::Relaxed);
    let mut index = (rt.item.load(Ordering::Relaxed) as usize).min(count - 1);
    let mut shown = None;
    while !rt.stopped() {
        let jump = rt.select.swap(-1, Ordering::Relaxed);
        if jump >= 0 {
            // Past the end means the last slide, as a clicker held too long.
            index = (jump as usize).min(count - 1);
        }
        if shown != Some(index) {
            shown = Some(index);
            rt.item.store(index as u64, Ordering::Relaxed);
            match titles::load_image(&slide_file(dir, index + 1).to_string_lossy(), w, h) {
                Ok(f) => {
                    rt.publish(f);
                    rt.clear_status();
                }
                Err(e) => rt.set_status(t!("slides.slide_unreadable", error = e)),
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Builds a small presentation by hand: one blank slide per colour, the
/// background filled with it. Tests use it to see which slide is on screen.
#[cfg(test)]
pub(crate) fn write_test_deck(path: &Path, colours: &[[u8; 3]]) {
    const NS: &str = "xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
        xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
        xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\"";
    const HEAD: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n";
    const TREE: &str = "<p:spTree><p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/>\
        </p:nvGrpSpPr><p:grpSpPr/></p:spTree>";
    const CT: &str = "application/vnd.openxmlformats-officedocument.";
    let rels = |items: &[(&str, String)]| {
        let body: String = items
            .iter()
            .enumerate()
            .map(|(n, (kind, target))| {
                format!(
                    "<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/\
                     2006/relationships/{kind}\" Target=\"{target}\"/>",
                    n + 1
                )
            })
            .collect();
        format!(
            "{HEAD}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
             {body}</Relationships>"
        )
    };
    let n = colours.len();
    let mut parts: Vec<(String, String)> = Vec::new();
    let mut types = vec![
        ("/ppt/presentation.xml".to_string(), "presentationml.presentation.main+xml"),
        ("/ppt/slideMasters/slideMaster1.xml".to_string(), "presentationml.slideMaster+xml"),
        ("/ppt/slideLayouts/slideLayout1.xml".to_string(), "presentationml.slideLayout+xml"),
        ("/ppt/theme/theme1.xml".to_string(), "theme+xml"),
    ];
    types.extend((1..=n).map(|k| (format!("/ppt/slides/slide{k}.xml"), "presentationml.slide+xml")));
    let overrides: String =
        types.iter().map(|(p, t)| format!("<Override PartName=\"{p}\" ContentType=\"{CT}{t}\"/>")).collect();
    parts.push((
        "[Content_Types].xml".into(),
        format!(
            "{HEAD}<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
             <Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
             <Default Extension=\"xml\" ContentType=\"application/xml\"/>{overrides}</Types>"
        ),
    ));
    parts.push(("_rels/.rels".into(), rels(&[("officeDocument", "ppt/presentation.xml".into())])));
    let ids: String =
        (0..n).map(|k| format!("<p:sldId id=\"{}\" r:id=\"rId{}\"/>", 256 + k, k + 3)).collect();
    parts.push((
        "ppt/presentation.xml".into(),
        format!(
            "{HEAD}<p:presentation {NS}><p:sldMasterIdLst><p:sldMasterId id=\"2147483648\" r:id=\"rId1\"/>\
             </p:sldMasterIdLst><p:sldIdLst>{ids}</p:sldIdLst><p:sldSz cx=\"12192000\" cy=\"6858000\"/>\
             <p:notesSz cx=\"6858000\" cy=\"9144000\"/></p:presentation>"
        ),
    ));
    let mut deck_rels = vec![
        ("slideMaster", "slideMasters/slideMaster1.xml".to_string()),
        ("theme", "theme/theme1.xml".into()),
    ];
    deck_rels.extend((1..=n).map(|k| ("slide", format!("slides/slide{k}.xml"))));
    parts.push(("ppt/_rels/presentation.xml.rels".into(), rels(&deck_rels)));
    parts.push((
        "ppt/slideMasters/slideMaster1.xml".into(),
        format!(
            "{HEAD}<p:sldMaster {NS}><p:cSld>{TREE}</p:cSld><p:clrMap bg1=\"lt1\" tx1=\"dk1\" bg2=\"lt2\" \
             tx2=\"dk2\" accent1=\"accent1\" accent2=\"accent2\" accent3=\"accent3\" accent4=\"accent4\" \
             accent5=\"accent5\" accent6=\"accent6\" hlink=\"hlink\" folHlink=\"folHlink\"/><p:sldLayoutIdLst>\
             <p:sldLayoutId id=\"2147483649\" r:id=\"rId1\"/></p:sldLayoutIdLst></p:sldMaster>"
        ),
    ));
    parts.push((
        "ppt/slideMasters/_rels/slideMaster1.xml.rels".into(),
        rels(&[
            ("slideLayout", "../slideLayouts/slideLayout1.xml".into()),
            ("theme", "../theme/theme1.xml".into()),
        ]),
    ));
    parts.push((
        "ppt/slideLayouts/slideLayout1.xml".into(),
        format!(
            "{HEAD}<p:sldLayout {NS} type=\"blank\"><p:cSld name=\"Blank\">{TREE}</p:cSld><p:clrMapOvr>\
             <a:masterClrMapping/></p:clrMapOvr></p:sldLayout>"
        ),
    ));
    parts.push((
        "ppt/slideLayouts/_rels/slideLayout1.xml.rels".into(),
        rels(&[("slideMaster", "../slideMasters/slideMaster1.xml".into())]),
    ));
    let scheme: String = [
        ("dk1", "000000"),
        ("lt1", "FFFFFF"),
        ("dk2", "1F1F1F"),
        ("lt2", "EEEEEE"),
        ("accent1", "4472C4"),
        ("accent2", "ED7D31"),
        ("accent3", "A5A5A5"),
        ("accent4", "FFC000"),
        ("accent5", "5B9BD5"),
        ("accent6", "70AD47"),
        ("hlink", "0563C1"),
        ("folHlink", "954F72"),
    ]
    .iter()
    .map(|(name, v)| format!("<a:{name}><a:srgbClr val=\"{v}\"/></a:{name}>"))
    .collect();
    let font = "<a:latin typeface=\"Arial\"/><a:ea typeface=\"\"/><a:cs typeface=\"\"/>";
    let fill = "<a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill>".repeat(3);
    let line = "<a:ln w=\"9525\"><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill></a:ln>".repeat(3);
    let effect = "<a:effectStyle><a:effectLst/></a:effectStyle>".repeat(3);
    parts.push((
        "ppt/theme/theme1.xml".into(),
        format!(
            "{HEAD}<a:theme xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" name=\"T\">\
             <a:themeElements><a:clrScheme name=\"T\">{scheme}</a:clrScheme><a:fontScheme name=\"T\">\
             <a:majorFont>{font}</a:majorFont><a:minorFont>{font}</a:minorFont></a:fontScheme>\
             <a:fmtScheme name=\"T\"><a:fillStyleLst>{fill}</a:fillStyleLst><a:lnStyleLst>{line}</a:lnStyleLst>\
             <a:effectStyleLst>{effect}</a:effectStyleLst><a:bgFillStyleLst>{fill}</a:bgFillStyleLst>\
             </a:fmtScheme></a:themeElements></a:theme>"
        ),
    ));
    for (k, [r, g, b]) in colours.iter().enumerate() {
        parts.push((
            format!("ppt/slides/slide{}.xml", k + 1),
            format!(
                "{HEAD}<p:sld {NS}><p:cSld><p:bg><p:bgPr><a:solidFill><a:srgbClr val=\"{r:02X}{g:02X}{b:02X}\"/>\
                 </a:solidFill><a:effectLst/></p:bgPr></p:bg>{TREE}</p:cSld><p:clrMapOvr><a:masterClrMapping/>\
                 </p:clrMapOvr></p:sld>"
            ),
        ));
        parts.push((
            format!("ppt/slides/_rels/slide{}.xml.rels", k + 1),
            rels(&[("slideLayout", "../slideLayouts/slideLayout1.xml".into())]),
        ));
    }
    std::fs::write(path, stored_zip(&parts)).unwrap();
}

/// A zip archive without compression, which is all a presentation needs.
#[cfg(test)]
fn stored_zip(parts: &[(String, String)]) -> Vec<u8> {
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }
    let (mut out, mut central) = (Vec::new(), Vec::new());
    for (name, body) in parts {
        let (name, body) = (name.as_bytes(), body.as_bytes());
        let (crc, size, offset) = (crc32(body), body.len() as u32, out.len() as u32);
        // Version 2.0, no flags, stored, no date.
        let common = |v: &mut Vec<u8>| {
            v.extend(20u16.to_le_bytes());
            v.extend([0u8; 8]);
            v.extend(crc.to_le_bytes());
            v.extend(size.to_le_bytes());
            v.extend(size.to_le_bytes());
            v.extend((name.len() as u16).to_le_bytes());
            v.extend(0u16.to_le_bytes());
        };
        out.extend(0x0403_4b50u32.to_le_bytes());
        common(&mut out);
        out.extend(name);
        out.extend(body);
        central.extend(0x0201_4b50u32.to_le_bytes());
        central.extend(20u16.to_le_bytes());
        common(&mut central);
        // Comment length, disk, internal and external attributes.
        central.extend([0u8; 10]);
        central.extend(offset.to_le_bytes());
        central.extend(name);
    }
    let (at, len, count) = (out.len() as u32, central.len() as u32, parts.len() as u16);
    out.extend(central);
    out.extend(0x0605_4b50u32.to_le_bytes());
    out.extend([0u8; 4]);
    out.extend(count.to_le_bytes());
    out.extend(count.to_le_bytes());
    out.extend(len.to_le_bytes());
    out.extend(at.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLOURS: [[u8; 3]; 3] = [[230, 20, 20], [20, 200, 40], [30, 40, 220]];

    fn aged_folder(root: &Path, name: &str, days: u64) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("count"), "1").unwrap();
        let old = SystemTime::now() - Duration::from_secs(days * 24 * 3600);
        std::fs::File::options().write(true).open(dir.join("count")).unwrap().set_modified(old).unwrap();
        dir
    }

    #[test]
    fn pruning_keeps_what_was_used_lately_or_is_showing() {
        let root = std::env::temp_dir().join(format!("lc-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (stale, reused, showing, keep) = (
            aged_folder(&root, "0000000000000001", 40),
            aged_folder(&root, "0000000000000002", 40),
            aged_folder(&root, "0000000000000003", 40),
            aged_folder(&root, "0000000000000004", 40),
        );
        used_now(&reused);
        let _busy = InUse::new(&showing);
        prune_in(&root, &keep, KEEP_CACHE);
        assert!(!stale.exists(), "a deck unused for 40 days goes");
        assert!(reused.exists(), "a cache hit renews the folder");
        assert!(showing.exists(), "a folder on air is never pruned");
        assert!(keep.exists());
        drop(_busy);
        prune_in(&root, &keep, KEEP_CACHE);
        assert!(!showing.exists(), "once released it ages like the others");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_reused_process_id_under_another_name_is_left_alone() {
        let out = std::env::temp_dir().join(format!("lc-pid-reuse-{}", std::process::id()));
        std::fs::create_dir_all(&out).unwrap();
        let mut other = Command::new("ping");
        other.args(["-n", "30", "127.0.0.1"]).stdout(Stdio::null());
        let mut other = no_window(&mut other).spawn().unwrap();
        std::fs::write(out.join("powerpoint.pid"), other.id().to_string()).unwrap();
        end_started_powerpoint(&out, "POWERPNT.EXE");
        thread::sleep(Duration::from_millis(1500));
        let alive = other.try_wait().unwrap().is_none();
        let _ = other.kill();
        let _ = std::fs::remove_dir_all(&out);
        assert!(alive, "a process that is not PowerPoint must survive");
    }

    #[test]
    fn a_given_up_conversion_ends_the_powerpoint_it_started_and_only_that() {
        let out = std::env::temp_dir().join(format!("lc-pid-{}", std::process::id()));
        std::fs::create_dir_all(&out).unwrap();
        let mut ours = Command::new("ping");
        ours.args(["-n", "60", "127.0.0.1"]).stdout(Stdio::null());
        let mut ours = no_window(&mut ours).spawn().unwrap();
        let mut bystander = Command::new("ping");
        bystander.args(["-n", "60", "127.0.0.1"]).stdout(Stdio::null());
        let mut bystander = no_window(&mut bystander).spawn().unwrap();
        std::fs::write(
            out.join("powerpoint.pid"),
            format!(
                "{}
",
                ours.id()
            ),
        )
        .unwrap();
        end_started_powerpoint(&out, "PING.EXE");
        let ended = (0..50).any(|_| {
            thread::sleep(Duration::from_millis(100));
            ours.try_wait().unwrap().is_some()
        });
        let spared = bystander.try_wait().unwrap().is_none();
        let _ = bystander.kill();
        let _ = ours.kill();
        let _ = std::fs::remove_dir_all(&out);
        assert!(ended, "the recorded process must end");
        assert!(spared, "any other process must be left alone");
    }

    fn centre(rt: &InputRt) -> Option<[u8; 3]> {
        let f = rt.latest()?;
        let at = (f.h / 2 * f.w + f.w / 2) * 4;
        Some([f.data[at], f.data[at + 1], f.data[at + 2]])
    }

    fn near(a: [u8; 3], b: [u8; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| x.abs_diff(y) < 12)
    }

    fn wait_for(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while !ok() {
            assert!(Instant::now() < deadline, "{what}");
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("livecraft-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn base64_matches_what_powershell_expects() {
        // "ab" as UTF-16: 61 00 62 00.
        assert_eq!(utf16_base64("ab"), "YQBiAA==");
        assert_eq!(utf16_base64("a"), "YQA=");
        assert_eq!(utf16_base64("abc"), "YQBiAGMA");
    }

    #[test]
    fn presentations_are_known_by_their_extension() {
        assert!(is_presentation(r"C:\shows\Keynote.PPTX"));
        assert!(is_presentation("deck.odp"));
        assert!(!is_presentation("clip.mp4"));
        assert!(!is_presentation("pptx"));
    }

    #[test]
    fn without_a_converter_the_input_says_what_to_install() {
        let dir = test_dir("sans");
        let deck = dir.join("deck.pptx");
        write_test_deck(&deck, &COLOURS);
        let rt = InputRt::new(false, true);
        run_with(&rt, &deck.to_string_lossy(), 64, 36, || None);
        assert_eq!(rt.status().as_deref(), Some(no_converter()));
        assert!(rt.latest().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_file_is_said_plainly() {
        let rt = InputRt::new(false, true);
        run_with(&rt, r"C:\nulle\part\absent.pptx", 64, 36, || panic!("nothing to convert"));
        assert_eq!(rt.status().as_deref(), Some("Présentation introuvable"));
    }

    /// Converts a deck built by hand with each converter the machine has, and
    /// walks through the slides by name, as a remote control would.
    fn slides_change_on_command(conv: Converter) {
        let dir = test_dir(conv.name());
        let deck = dir.join("couleurs.pptx");
        write_test_deck(&deck, &COLOURS);
        let out = dir.join("slides");
        std::fs::create_dir_all(&out).unwrap();
        let mut seen = Vec::new();
        // Inputs converting in other tests take turns the same way.
        let turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        let n = convert(&conv, &deck, &out, 320, 180, &|| false, &mut |done, total| seen.push((done, total)))
            .unwrap_or_else(|e| panic!("{e}"));
        drop(turn);
        assert_eq!(n, 3);
        assert_eq!(seen, [(1, 3), (2, 3), (3, 3)]);
        // A damaged file must fail, not wait on a dialog nobody can see.
        let broken = dir.join("abime.pptx");
        std::fs::write(&broken, "pas une présentation").unwrap();
        let turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        let err = convert(&conv, &broken, &out, 320, 180, &|| false, &mut |_, _| {}).unwrap_err();
        drop(turn);
        assert!(err.starts_with("Présentation illisible"), "{err}");
        for (k, colour) in COLOURS.iter().enumerate() {
            let f = titles::load_image(&slide_file(&out, k + 1).to_string_lossy(), 32, 18).unwrap();
            let at = (9 * 32 + 16) * 4;
            assert!(near([f.data[at], f.data[at + 1], f.data[at + 2]], *colour), "slide {}", k + 1);
        }

        // The same deck as a running input, steered as the functions do.
        let rt = InputRt::new(false, true);
        let worker = rt.clone();
        let path = deck.to_string_lossy().into_owned();
        let conv2 = conv.clone();
        thread::spawn(move || run_with(&worker, &path, 320, 180, || Some(conv2)));
        // The first slide may go up before the others are made.
        wait_for("first slide", 180, || centre(&rt).is_some_and(|c| near(c, COLOURS[0])));
        wait_for("every slide", 60, || rt.items.load(Ordering::Relaxed) == 3);
        assert_eq!(rt.status(), None);
        rt.select.store(2, Ordering::Relaxed);
        wait_for("third slide", 5, || centre(&rt).is_some_and(|c| near(c, COLOURS[2])));
        assert_eq!(rt.item.load(Ordering::Relaxed), 2);
        rt.select.store(40, Ordering::Relaxed);
        thread::sleep(Duration::from_millis(200));
        assert_eq!(rt.item.load(Ordering::Relaxed), 2, "past the end stays on the last slide");
        rt.select.store(1, Ordering::Relaxed);
        wait_for("second slide", 5, || centre(&rt).is_some_and(|c| near(c, COLOURS[1])));
        rt.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn powerpoint_converts_and_slides_change_on_command() {
        if !powerpoint_installed() {
            eprintln!("PowerPoint absent : essai sauté");
            return;
        }
        slides_change_on_command(Converter::PowerPoint);
    }

    #[test]
    fn libreoffice_converts_and_slides_change_on_command() {
        let Some(soffice) = libreoffice() else {
            eprintln!("LibreOffice absent : essai sauté");
            return;
        };
        slides_change_on_command(Converter::LibreOffice(soffice));
    }
}
