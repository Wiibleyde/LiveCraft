//! DVD-Video: recognising a disc, a VIDEO_TS folder or an .iso image, listing
//! its titles, and asking ffmpeg's `dvdvideo` demuxer to play one of them.
//!
//! The titles come from the disc's own tables (VIDEO_TS.IFO and the title set
//! files), read here directly: ffmpeg only opens one title at a time, and
//! trying titles one by one would spin a real drive for seconds. The layout
//! read is the published DVD-Video IFO structure.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const SECTOR: u64 = 2048;
/// IFO files are a few hundred kilobytes; anything far larger is not one.
const MAX_IFO: u64 = 16 << 20;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Title {
    /// As players and ffmpeg count them, from 1.
    pub number: u32,
    pub chapters: u32,
    pub angles: u32,
    pub duration_ms: u64,
}

/// What to hand ffmpeg for a dropped or picked path, when it is a DVD.
pub fn recognise(p: &Path) -> Option<String> {
    let is_iso = p.extension().is_some_and(|e| e.eq_ignore_ascii_case("iso"));
    if p.is_file() {
        // A Linux install image is an .iso too: only one holding VIDEO_TS is a DVD.
        return (is_iso && Disc::open(p).is_ok()).then(|| p.to_string_lossy().into_owned());
    }
    // ffmpeg wants the folder that holds VIDEO_TS, or the drive itself.
    let holder = if is_video_ts(p) { p.parent()? } else { p };
    holder.join("VIDEO_TS").join("VIDEO_TS.IFO").is_file().then(|| holder.to_string_lossy().into_owned())
}

fn is_video_ts(p: &Path) -> bool {
    p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("VIDEO_TS"))
}

/// A name for the card: the image or folder name, or the drive letter.
pub fn display_name(path: &str) -> String {
    let p = Path::new(path);
    match p.file_stem() {
        Some(stem) => stem.to_string_lossy().into_owned(),
        None => format!("DVD {}", path.trim_end_matches(['\\', '/'])),
    }
}

/// How ffmpeg opens one title. Title 0 would let ffmpeg guess, and it says
/// itself that its guess is often not the main feature.
pub fn input_args(path: &str, title: u32) -> Vec<String> {
    ["-f", "dvdvideo", "-title", &title.max(1).to_string(), "-i", path].map(String::from).to_vec()
}

/// `optical_drives`, asked again at most every few seconds: the interface draws
/// the menu that lists them every frame, and a stale network drive can make the
/// query slow.
pub fn optical_drives_cached() -> Vec<String> {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static CACHE: Mutex<Option<(Instant, Vec<String>)>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    match &*cache {
        Some((at, drives)) if at.elapsed() < Duration::from_secs(5) => drives.clone(),
        _ => {
            let drives = optical_drives();
            *cache = Some((Instant::now(), drives.clone()));
            drives
        }
    }
}

/// Elsewhere than Windows there are no drive letters to list: a disc is
/// opened from an image or a folder.
#[cfg(not(windows))]
fn optical_drives() -> Vec<String> {
    Vec::new()
}

/// Drive letters of the computer's optical drives, such as `D:\`.
#[cfg(windows)]
fn optical_drives() -> Vec<String> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    // DRIVE_CDROM, as documented for GetDriveTypeW; DVD drives report it too.
    const DRIVE_CDROM: u32 = 5;
    // SAFETY: no arguments; returns a bit mask of the drive letters in use.
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|n| mask & (1 << n) != 0)
        .map(|n| format!("{}:\\", (b'A' + n) as char))
        .filter(|root| {
            let wide: Vec<u16> = root.encode_utf16().chain([0]).collect();
            // SAFETY: `wide` is a NUL-terminated UTF-16 root path that outlives the call.
            unsafe { GetDriveTypeW(wide.as_ptr()) == DRIVE_CDROM }
        })
        .collect()
}

/// The titles of a disc, folder or image, in order.
pub fn titles(path: &str) -> Result<Vec<Title>, String> {
    let mut disc = Disc::open(Path::new(path))?;
    let vmg = disc.read("VIDEO_TS.IFO")?;
    if !vmg.starts_with(b"DVDVIDEO-VMG") {
        return Err(t!("dvd.not_video_ifo").into());
    }
    let srpt = table(&vmg, 0xC4)?;
    let count = be16(&vmg, srpt)? as usize;
    // Read the title entries first, then the title sets one at a time in set order:
    // a title set file can be large, and an image can name many of them.
    let mut entries = Vec::with_capacity(count);
    for n in 0..count {
        let e = srpt + 8 + n * 12;
        let angles = *vmg.get(e + 1).ok_or_else(short)? as u32;
        let chapters = be16(&vmg, e + 2)? as u32;
        let (set, set_title) = (*vmg.get(e + 6).ok_or_else(short)?, *vmg.get(e + 7).ok_or_else(short)?);
        entries.push((n as u32 + 1, angles, chapters, set, set_title));
    }
    let mut by_set: Vec<usize> = (0..count).collect();
    by_set.sort_by_key(|&i| entries[i].3);
    let mut out: Vec<Option<Title>> = vec![None; count];
    let mut current: Option<(u8, Vec<u8>)> = None;
    for i in by_set {
        let (number, angles, chapters, set, set_title) = entries[i];
        if current.as_ref().is_none_or(|(s, _)| *s != set) {
            current = Some((set, disc.read(&format!("VTS_{set:02}_0.IFO"))?));
        }
        let vts = &current.as_ref().ok_or_else(short)?.1;
        let duration_ms = title_duration(vts, set_title, chapters)?;
        out[i] = Some(Title { number, chapters, angles, duration_ms });
    }
    let out: Vec<Title> = out.into_iter().flatten().collect();
    Ok(out)
}

fn short() -> String {
    t!("dvd.truncated").to_string()
}

fn be16(b: &[u8], at: usize) -> Result<u16, String> {
    b.get(at..at + 2).map(|s| u16::from_be_bytes([s[0], s[1]])).ok_or_else(short)
}

fn be32(b: &[u8], at: usize) -> Result<u32, String> {
    b.get(at..at + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]])).ok_or_else(short)
}

/// IFO headers point to their tables by sector.
fn table(ifo: &[u8], pointer_at: usize) -> Result<usize, String> {
    let at = be32(ifo, pointer_at)? as usize * SECTOR as usize;
    if at == 0 || at >= ifo.len() { Err(short()) } else { Ok(at) }
}

/// Adds up the program chains the title's chapters run through: usually one,
/// sometimes one per chapter.
fn title_duration(vts: &[u8], set_title: u8, chapters: u32) -> Result<u64, String> {
    if !vts.starts_with(b"DVDVIDEO-VTS") {
        return Err(t!("dvd.title_table").into());
    }
    let ptt = table(vts, 0xC8)?;
    let pgci = table(vts, 0xCC)?;
    let first = ptt + be32(vts, ptt + 8 + (set_title.max(1) as usize - 1) * 4)? as usize;
    let mut chains: Vec<u16> = Vec::new();
    for c in 0..chapters as usize {
        let pgcn = be16(vts, first + c * 4)?;
        if !chains.contains(&pgcn) {
            chains.push(pgcn);
        }
    }
    let mut total = 0;
    for pgcn in chains {
        let pgc = pgci + be32(vts, pgci + 8 + (pgcn.max(1) as usize - 1) * 8 + 4)? as usize;
        total += playback_ms(vts.get(pgc + 4..pgc + 8).ok_or_else(short)?);
    }
    Ok(total)
}

/// hh mm ss ff in BCD; the two top bits of ff give the frame rate.
fn playback_ms(t: &[u8]) -> u64 {
    let bcd = |b: u8| ((b >> 4) * 10 + (b & 0x0F)) as u64;
    let fps = if t[3] >> 6 == 0b01 { 25.0 } else { 30_000.0 / 1001.0 };
    let seconds = bcd(t[0]) * 3600 + bcd(t[1]) * 60 + bcd(t[2]);
    seconds * 1000 + (bcd(t[3] & 0x3F) as f64 * 1000.0 / fps) as u64
}

/// Where the VIDEO_TS files are: a folder on disk, or inside an image.
enum Disc {
    Folder(PathBuf),
    Image { file: File, entries: Vec<IsoEntry> },
}

struct IsoEntry {
    name: String,
    lba: u64,
    len: u64,
    dir: bool,
}

impl Disc {
    fn open(p: &Path) -> Result<Self, String> {
        if p.is_file() {
            let mut file = File::open(p).map_err(|e| t!("dvd.image_unreadable", error = e))?;
            let root = iso_root(&mut file)?;
            let video_ts = root
                .into_iter()
                .find(|e| e.dir && e.name.eq_ignore_ascii_case("VIDEO_TS"))
                .ok_or_else(|| t!("dvd.no_video_ts_in_image").to_string())?;
            let entries = iso_dir(&mut file, video_ts.lba, video_ts.len)?;
            return Ok(Disc::Image { file, entries });
        }
        let dir = if is_video_ts(p) { p.to_path_buf() } else { p.join("VIDEO_TS") };
        if dir.is_dir() { Ok(Disc::Folder(dir)) } else { Err(t!("dvd.no_video_ts").into()) }
    }

    fn read(&mut self, name: &str) -> Result<Vec<u8>, String> {
        match self {
            Disc::Folder(dir) => {
                let path = dir.join(name);
                let len = std::fs::metadata(&path).map_err(|e| format!("{name} : {e}"))?.len();
                if len > MAX_IFO {
                    return Err(t!("dvd.too_big", name = name));
                }
                std::fs::read(&path).map_err(|e| format!("{name} : {e}"))
            }
            Disc::Image { file, entries } => {
                let e = entries
                    .iter()
                    .find(|e| !e.dir && e.name.eq_ignore_ascii_case(name))
                    .ok_or_else(|| t!("dvd.missing_in_image", name = name))?;
                if e.len > MAX_IFO {
                    return Err(t!("dvd.too_big", name = name));
                }
                read_at(file, e.lba * SECTOR, e.len)
            }
        }
    }
}

fn read_at(file: &mut File, at: u64, len: u64) -> Result<Vec<u8>, String> {
    let mut buf = vec![0; len as usize];
    file.seek(SeekFrom::Start(at)).and_then(|_| file.read_exact(&mut buf)).map_err(|e| e.to_string())?;
    Ok(buf)
}

/// DVD-Video images carry an ISO 9660 file system beside UDF, and its
/// primary volume descriptor always sits in sector 16.
fn iso_root(file: &mut File) -> Result<Vec<IsoEntry>, String> {
    let pvd = read_at(file, 16 * SECTOR, SECTOR)?;
    if pvd[0] != 1 || &pvd[1..6] != b"CD001" {
        return Err(t!("dvd.no_iso").into());
    }
    let root = record(&pvd[156..190]).ok_or_else(|| t!("dvd.no_root").to_string())?;
    iso_dir(file, root.lba, root.len)
}

fn iso_dir(file: &mut File, lba: u64, len: u64) -> Result<Vec<IsoEntry>, String> {
    // VIDEO_TS holds at most a hundred or so files.
    let data = read_at(file, lba * SECTOR, len.min(1 << 20))?;
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let size = data[i] as usize;
        if size == 0 {
            // Records never straddle sectors; the rest of this one is padding.
            i = (i / SECTOR as usize + 1) * SECTOR as usize;
            continue;
        }
        if let Some(e) = data.get(i..i + size).and_then(record) {
            out.push(e);
        }
        i += size;
    }
    Ok(out)
}

/// One directory record. A directory's first two records, itself and its
/// parent, are named by the bytes 0 and 1 and so never match a file name.
fn record(r: &[u8]) -> Option<IsoEntry> {
    let name_len = *r.get(32)? as usize;
    let name = String::from_utf8_lossy(r.get(33..33 + name_len)?);
    let name = name.split(';').next().unwrap_or_default().to_string();
    let le32 = |at: usize| r.get(at..at + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as u64);
    Some(IsoEntry { name, lba: le32(2)?, len: le32(10)?, dir: r.get(25)? & 2 != 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tables read here must agree with the disc ffmpeg plays, from a
    /// folder and from an image of it.
    #[test]
    fn titles_of_a_generated_disc_read_alike_from_its_folder_and_its_image() {
        let dir = std::env::temp_dir().join(format!("livecraft-dvd-titres-{}", std::process::id()));
        let Some(disc) = crate::playtest::dvd_fixture(&dir.to_string_lossy()).unwrap() else {
            eprintln!("ignoré : dvdauthor ou le démultiplexeur dvdvideo manque");
            return;
        };
        let found = titles(&disc).unwrap();
        let summary: Vec<(u32, u32, u32)> = found.iter().map(|t| (t.number, t.chapters, t.angles)).collect();
        assert_eq!(summary, [(1, 3, 1), (2, 1, 1)]);
        assert!(found[0].duration_ms.abs_diff(6000) < 100, "{:?}", found[0]);
        assert!(found[1].duration_ms.abs_diff(2000) < 100, "{:?}", found[1]);
        let iso = dir.join("disque.iso");
        let made = std::process::Command::new("mkisofs")
            .args(["-quiet", "-dvd-video", "-V", "ESSAI", "-o"])
            .arg(&iso)
            .arg(&disc)
            .status();
        if made.is_ok_and(|s| s.success()) {
            let iso = iso.to_string_lossy().into_owned();
            assert_eq!(titles(&iso).unwrap(), found);
            assert_eq!(recognise(Path::new(&iso)), Some(iso.clone()));
            // ffmpeg opens the same image and title with the same arguments.
            let played = crate::sources::ffmpeg()
                .args(input_args(&iso, 2))
                .args(["-frames:v", "1", "-f", "null", "-"])
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(played.success());
        } else {
            eprintln!("image non vérifiée : mkisofs manque");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_iso_without_video_ts_is_not_a_dvd() {
        let dir = std::env::temp_dir().join(format!("livecraft-dvd-iso-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let other = dir.join("installation.iso");
        std::fs::write(&other, vec![0u8; 40 * SECTOR as usize]).unwrap();
        assert_eq!(recognise(&other), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn playback_times_read_as_bcd_at_the_disc_rate() {
        assert_eq!(playback_ms(&[0x01, 0x30, 0x05, 0x40 | 0x12]), 5_405_480);
        assert_eq!(playback_ms(&[0x00, 0x00, 0x06, 0xC0 | 0x15]), 6_500);
    }

    #[test]
    fn a_folder_inside_or_holding_video_ts_is_a_dvd_but_a_photo_folder_is_not() {
        let dir = std::env::temp_dir().join(format!("livecraft-dvd-{}", std::process::id()));
        let video_ts = dir.join("Disque").join("VIDEO_TS");
        std::fs::create_dir_all(&video_ts).unwrap();
        std::fs::write(video_ts.join("VIDEO_TS.IFO"), b"DVDVIDEO-VMG").unwrap();
        let holder = dir.join("Disque").to_string_lossy().into_owned();
        assert_eq!(recognise(&dir.join("Disque")), Some(holder.clone()));
        assert_eq!(recognise(&video_ts), Some(holder), "picking VIDEO_TS itself opens the disc");
        assert_eq!(recognise(&dir), None, "a folder above the disc is not one");
        assert_eq!(display_name("D:\\"), "DVD D:");
        assert_eq!(display_name("C:/films/vacances.iso"), "vacances");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_broken_table_is_an_error_not_a_panic() {
        let mut vmg = b"DVDVIDEO-VMG".to_vec();
        vmg.resize(0x100, 0);
        vmg[0xC7] = 9;
        let dir = std::env::temp_dir().join(format!("livecraft-dvd-bad-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("VIDEO_TS")).unwrap();
        std::fs::write(dir.join("VIDEO_TS").join("VIDEO_TS.IFO"), &vmg).unwrap();
        assert!(titles(&dir.to_string_lossy()).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
