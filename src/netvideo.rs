//! NDI and OMT: pictures and sound over the local network, in and out. Both
//! runtimes are loaded the first time someone uses them, so LiveCraft starts
//! and works without them, and says what is missing when one is asked for.

use crate::frame::{Frame, ROWS};
use crate::sources::{AUDIO_RATE, InputRt};
use crate::{ndi, omt};
use rayon::prelude::*;
use std::ffi::CString;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Net {
    Ndi,
    Omt,
}

impl Net {
    pub const ALL: [Net; 2] = [Net::Ndi, Net::Omt];

    pub fn label(self) -> &'static str {
        match self {
            Net::Ndi => "NDI",
            Net::Omt => "OMT",
        }
    }

    pub fn input(self, source: String) -> crate::model::Kind {
        match self {
            Net::Ndi => crate::model::Kind::Ndi { source },
            Net::Omt => crate::model::Kind::Omt { source },
        }
    }

    /// Whether the runtime loads. The first call loads it, which takes a moment.
    pub fn available(self) -> Result<(), String> {
        match self {
            Net::Ndi => ndi::available(),
            Net::Omt => omt::available(),
        }
    }

    pub fn sources(self) -> Result<Vec<String>, String> {
        match self {
            Net::Ndi => ndi::sources(),
            Net::Omt => omt::sources(),
        }
    }

    fn receiver(self, source: &str) -> Result<Box<dyn Receiver>, String> {
        Ok(match self {
            Net::Ndi => Box::new(ndi::Receiver::new(source)?),
            Net::Omt => Box::new(omt::Receiver::new(source)?),
        })
    }

    fn sender(self, name: &str) -> Result<Box<dyn Sender>, String> {
        Ok(match self {
            Net::Ndi => Box::new(ndi::Sender::new(name)?),
            Net::Omt => Box::new(omt::Sender::new(name)?),
        })
    }
}

/// A runtime library, loaded for good: its functions are kept for the life of
/// the program, so it is never unloaded.
pub(crate) struct Library(windows_sys::Win32::Foundation::HMODULE);

impl Library {
    /// The first existing candidate that loads. Only a full path is ever
    /// loaded: a bare name would also be searched for in the current folder,
    /// where anyone could have planted a library. The library's own folder is
    /// searched for what it needs (libvmx beside libomt).
    pub(crate) fn open(candidates: &[PathBuf]) -> Option<Self> {
        use windows_sys::Win32::System::LibraryLoader::*;
        candidates.iter().filter(|p| p.is_absolute() && p.is_file()).find_map(|path| {
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
            let flags = LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS;
            // SAFETY: the name is a NUL-terminated wide string that outlives the call.
            let module = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), flags) };
            (!module.is_null()).then_some(Self(module))
        })
    }

    /// # Safety
    /// `T` must be a function pointer type matching what the library exports
    /// under `name`.
    pub(crate) unsafe fn symbol<T: Copy>(&self, name: &str) -> Result<T, String> {
        let wanted = CString::new(name).map_err(|e| e.to_string())?;
        // SAFETY: the module is loaded and the name is NUL-terminated.
        let found = unsafe {
            windows_sys::Win32::System::LibraryLoader::GetProcAddress(self.0, wanted.as_ptr().cast())
        };
        let Some(found) = found else {
            return Err(format!("{name} manque : version trop ancienne de la bibliothèque"));
        };
        assert_eq!(size_of::<T>(), size_of_val(&found));
        // SAFETY: same size, and the caller vouches for the signature.
        Ok(unsafe { std::mem::transmute_copy(&found) })
    }
}

/// Where a runtime library may be, as full paths only, in order: the given
/// folders (a variable the installer sets), the folder of the program, then
/// each folder of the `PATH`. Relative and empty entries are left out, so the
/// current folder is never searched.
pub(crate) fn runtime_candidates(file: &str, env_vars: &[&str]) -> Vec<PathBuf> {
    let dirs: Vec<std::ffi::OsString> = env_vars.iter().filter_map(std::env::var_os).collect();
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(PathBuf::from));
    candidates_from(file, dirs, exe_dir, std::env::var_os("PATH"))
}

fn candidates_from(
    file: &str,
    dirs: Vec<std::ffi::OsString>,
    exe_dir: Option<PathBuf>,
    path: Option<std::ffi::OsString>,
) -> Vec<PathBuf> {
    let from_path = path.iter().flat_map(std::env::split_paths);
    dirs.into_iter()
        .map(PathBuf::from)
        .chain(exe_dir)
        .chain(from_path)
        .filter(|d| d.is_absolute())
        .map(|d| d.join(file))
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Order {
    Rgba,
    Bgra,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Alpha {
    Opaque,
    Straight,
    Premultiplied,
}

/// A received picture, still in the runtime's memory.
pub struct Picture<'a> {
    pub data: &'a [u8],
    pub w: usize,
    pub h: usize,
    pub stride: usize,
    pub order: Order,
    pub alpha: Alpha,
}

/// Received sound: `channels` planes of `samples` floats, `plane` floats apart.
pub struct Sound<'a> {
    pub data: &'a [f32],
    pub channels: usize,
    pub samples: usize,
    pub plane: usize,
    pub rate: u32,
}

pub enum Got<'a> {
    Picture(Picture<'a>),
    Sound(Sound<'a>),
}

pub(crate) trait Receiver: Send {
    /// Waits up to `timeout_ms` for a picture or some sound, and hands it to `on`.
    fn capture(&mut self, timeout_ms: u32, on: &mut dyn FnMut(Got<'_>));
}

pub(crate) trait Sender: Send {
    fn video(&mut self, f: &Frame, fps: u32);
    /// Interleaved stereo at `AUDIO_RATE`.
    fn audio(&mut self, stereo: &[f32]);
    fn connections(&self) -> u32;
}

/// Sound waiting beyond this is late already: dropping it keeps a source
/// whose clock runs a little fast from drifting behind its picture.
const MOST_BUFFERED: usize = AUDIO_RATE * 2 / 5;

/// An NDI or OMT input: the latest picture fitted to the output size, and
/// the sound in stereo at the mixer's rate. The runtime reconnects by itself.
pub fn run_input(rt: &Arc<InputRt>, net: Net, source: &str, w: usize, h: usize) {
    let label = net.label();
    let source = source.trim();
    if source.is_empty() {
        rt.set_status(format!("Choisissez une source {label} dans les réglages de l'entrée"));
        return;
    }
    let mut receiver = match net.receiver(source) {
        Ok(r) => r,
        Err(e) => {
            rt.set_status(e);
            return;
        }
    };
    rt.set_status(format!("Recherche de la source {label} « {source} »…"));
    let mut spare: Option<Frame> = None;
    let mut scratch: Option<Frame> = None;
    let mut resampler = Resampler::default();
    let mut stereo = Vec::new();
    let mut heard: Option<Instant> = None;
    let mut lost = false;
    while !rt.stopped() {
        let mut got_any = false;
        receiver.capture(100, &mut |got| {
            got_any = true;
            match got {
                Got::Picture(p) => {
                    let mut out =
                        spare.take().filter(|f| (f.w, f.h) == (w, h)).unwrap_or_else(|| Frame::new(w, h));
                    fit(&p, &mut out, &mut scratch);
                    spare = rt.publish(out);
                }
                Got::Sound(s) => {
                    if let Some(ring) = &rt.audio {
                        to_stereo(&s, &mut resampler, &mut stereo);
                        if ring.len() + stereo.len() <= MOST_BUFFERED {
                            ring.push(&stereo);
                        }
                    }
                }
            }
        });
        if got_any {
            if heard.is_none() || lost {
                rt.clear_status();
                lost = false;
            }
            heard = Some(Instant::now());
        } else if !lost && heard.is_some_and(|t| t.elapsed() > Duration::from_secs(3)) {
            lost = true;
            rt.set_status(format!("Source {label} « {source} » perdue, en attente de son retour…"));
        }
    }
}

/// Fills `out` with the picture, letterboxed when its shape differs.
pub(crate) fn fit(p: &Picture, out: &mut Frame, scratch: &mut Option<Frame>) {
    if (p.w, p.h) == (out.w, out.h) {
        convert(p, out);
        return;
    }
    let mut src = scratch.take().filter(|f| (f.w, f.h) == (p.w, p.h)).unwrap_or_else(|| Frame::new(p.w, p.h));
    convert(p, &mut src);
    let scale = (out.w as f32 / p.w as f32).min(out.h as f32 / p.h as f32);
    let dw = ((p.w as f32 * scale).round() as usize).clamp(1, out.w);
    let dh = ((p.h as f32 * scale).round() as usize).clamp(1, out.h);
    out.data.fill(0);
    out.blit_scaled(&src, ((out.w - dw) / 2) as i32, ((out.h - dh) / 2) as i32, dw, dh, 255);
    *scratch = Some(src);
}

/// Into RGBA with premultiplied alpha, as every picture here is.
fn convert(p: &Picture, out: &mut Frame) {
    let row_bytes = p.w * 4;
    out.data.par_chunks_mut(row_bytes).with_min_len(ROWS).enumerate().for_each(|(y, row)| {
        let src = &p.data[y * p.stride..y * p.stride + row_bytes];
        for (d, s) in row.as_chunks_mut::<4>().0.iter_mut().zip(src.as_chunks::<4>().0) {
            let [r, g, b] = match p.order {
                Order::Rgba => [s[0], s[1], s[2]],
                Order::Bgra => [s[2], s[1], s[0]],
            };
            *d = match p.alpha {
                Alpha::Opaque => [r, g, b, 255],
                Alpha::Premultiplied => [r, g, b, s[3]],
                Alpha::Straight => {
                    let a = s[3] as u32;
                    let times = |c: u8| ((c as u32 * a + 127) / 255) as u8;
                    [times(r), times(g), times(b), s[3]]
                }
            };
        }
    });
}

pub(crate) fn swap_red_blue(src: &[u8], out: &mut Vec<u8>) {
    out.resize(src.len(), 0);
    out.par_chunks_mut(1 << 16).zip(src.par_chunks(1 << 16)).for_each(|(d, s)| {
        for (d, s) in d.as_chunks_mut::<4>().0.iter_mut().zip(s.as_chunks::<4>().0) {
            *d = [s[2], s[1], s[0], s[3]];
        }
    });
}

/// Interleaved stereo to one plane per channel.
pub(crate) fn to_planar(stereo: &[f32], out: &mut Vec<f32>) {
    out.clear();
    out.extend(stereo.iter().step_by(2));
    out.extend(stereo.iter().skip(1).step_by(2));
}

/// Carries a rate change across blocks, so their joins make no click.
#[derive(Default)]
pub(crate) struct Resampler {
    /// Where the next output sample falls in the next block; -1 is the last
    /// sample of the previous block.
    pos: f64,
    last: [f32; 2],
}

/// Interleaved stereo at the mixer's rate; mono goes to both sides, channels
/// past the second are left out.
pub(crate) fn to_stereo(s: &Sound, rs: &mut Resampler, out: &mut Vec<f32>) {
    out.clear();
    let n = s.samples;
    if n == 0 {
        return;
    }
    let left = &s.data[..n];
    let right = if s.channels > 1 { &s.data[s.plane..s.plane + n] } else { left };
    if s.rate as usize == AUDIO_RATE {
        out.extend(left.iter().zip(right).flat_map(|(l, r)| [*l, *r]));
        *rs = Resampler { pos: 0.0, last: [left[n - 1], right[n - 1]] };
        return;
    }
    let step = s.rate as f64 / AUDIO_RATE as f64;
    let planes = [left, right];
    let last = rs.last;
    let at = |i: isize, c: usize| if i < 0 { last[c] } else { planes[c][i as usize] };
    let mut pos = rs.pos;
    while pos < (n - 1) as f64 {
        let i = pos.floor();
        let t = (pos - i) as f32;
        let i = i as isize;
        for c in 0..2 {
            out.push(at(i, c) * (1.0 - t) + at(i + 1, c) * t);
        }
        pos += step;
    }
    *rs = Resampler { pos: pos - n as f64, last: [left[n - 1], right[n - 1]] };
}

enum Feed {
    Picture(Arc<Frame>),
    Sound(Vec<f32>),
}

/// The program sent as an NDI or OMT source, with the Master sound. Sending
/// happens on its own thread: the mixer and the sound never wait for it.
pub struct NetOut {
    pub net: Net,
    pub name: String,
    feed: Option<SyncSender<Feed>>,
    pictures_queued: Arc<AtomicUsize>,
    connections: Arc<AtomicU32>,
    worker: Option<thread::JoinHandle<()>>,
}

impl NetOut {
    pub fn start(net: Net, name: &str, fps: u32) -> Result<Self, String> {
        let mut sender = net.sender(name)?;
        let (tx, rx) = sync_channel::<Feed>(64);
        let pictures_queued = Arc::new(AtomicUsize::new(0));
        let connections = Arc::new(AtomicU32::new(0));
        let (queued, count) = (pictures_queued.clone(), connections.clone());
        let worker = thread::spawn(move || {
            let mut counted = Instant::now();
            while let Ok(feed) = rx.recv() {
                match feed {
                    Feed::Picture(f) => {
                        queued.fetch_sub(1, Ordering::Relaxed);
                        sender.video(&f, fps);
                    }
                    Feed::Sound(s) => sender.audio(&s),
                }
                if counted.elapsed() > Duration::from_millis(500) {
                    counted = Instant::now();
                    count.store(sender.connections(), Ordering::Relaxed);
                }
            }
        });
        Ok(Self {
            net,
            name: name.to_string(),
            feed: Some(tx),
            pictures_queued,
            connections,
            worker: Some(worker),
        })
    }

    pub fn push_video(&self, f: Arc<Frame>) {
        // A slow network loses pictures, never sound: a picture only queues
        // when few are already waiting.
        if self.pictures_queued.fetch_add(1, Ordering::Relaxed) >= 2 {
            self.pictures_queued.fetch_sub(1, Ordering::Relaxed);
            return;
        }
        if self.feed.as_ref().is_none_or(|tx| tx.try_send(Feed::Picture(f)).is_err()) {
            self.pictures_queued.fetch_sub(1, Ordering::Relaxed);
        }
    }

    pub fn push_audio(&self, stereo: &[f32]) {
        if let Some(tx) = &self.feed {
            let _ = tx.try_send(Feed::Sound(stereo.to_vec()));
        }
    }

    /// Connections from receivers as last counted; one receiver may open several.
    pub fn connections(&self) -> u32 {
        self.connections.load(Ordering::Relaxed)
    }
}

impl Drop for NetOut {
    fn drop(&mut self) {
        self.feed = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn runtime_candidates_are_full_paths_only() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        let path = format!("rel{sep}{sep}C:\\bin{sep}.{sep}C:\\tools");
        let got = candidates_from(
            "x.dll",
            vec!["relatif".into(), "C:\\ndi".into(), "".into()],
            Some(PathBuf::from("C:\\app")),
            Some(path.into()),
        );
        assert!(got.iter().all(|p| p.is_absolute()), "{got:?}");
        assert!(got.iter().all(|p| p.file_name().unwrap() == "x.dll"));
        assert!(!got.contains(&PathBuf::from("x.dll")), "never the bare name");
        let order: Vec<String> =
            got.iter().map(|p| p.parent().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(order, [r"C:\ndi", r"C:\app", r"C:\bin", r"C:\tools"]);
    }

    #[test]
    fn a_bare_name_is_never_loaded() {
        assert!(Library::open(&[PathBuf::from("kernel32.dll")]).is_none());
    }

    use super::*;

    fn picture(data: &[u8], w: usize, h: usize, stride: usize, order: Order, alpha: Alpha) -> Frame {
        let mut out = Frame::new(w, h);
        fit(&Picture { data, w, h, stride, order, alpha }, &mut out, &mut None);
        out
    }

    #[test]
    fn received_pixels_come_out_rgba_premultiplied() {
        // One pixel per row and padding after it, which must be skipped.
        let rows = [10, 20, 30, 128, 9, 9, 9, 9, 200, 100, 50, 255, 9, 9, 9, 9];
        let straight = picture(&rows, 1, 2, 8, Order::Rgba, Alpha::Straight);
        assert_eq!(straight.data, [5, 10, 15, 128, 200, 100, 50, 255]);
        let bgr = picture(&rows, 1, 2, 8, Order::Bgra, Alpha::Opaque);
        assert_eq!(bgr.data, [30, 20, 10, 255, 50, 100, 200, 255]);
        let kept = picture(&rows, 1, 2, 8, Order::Rgba, Alpha::Premultiplied);
        assert_eq!(kept.data, [10, 20, 30, 128, 200, 100, 50, 255]);
    }

    #[test]
    fn another_shape_is_letterboxed_in_the_middle() {
        let red = [255u8, 0, 0, 255].repeat(4 * 4);
        let mut out = Frame::new(8, 4);
        fit(
            &Picture { data: &red, w: 4, h: 4, stride: 16, order: Order::Rgba, alpha: Alpha::Opaque },
            &mut out,
            &mut None,
        );
        let px = |x: usize, y: usize| &out.data[(y * 8 + x) * 4..(y * 8 + x) * 4 + 4];
        assert_eq!(px(0, 2), [0, 0, 0, 0], "the sides stay clear");
        assert_eq!(px(7, 2), [0, 0, 0, 0]);
        assert_eq!(px(2, 0), [255, 0, 0, 255]);
        assert_eq!(px(5, 3), [255, 0, 0, 255]);
    }

    #[test]
    fn mono_sound_reaches_both_sides() {
        let plane = [0.1f32, 0.2, 0.3];
        let mut out = Vec::new();
        let s = Sound { data: &plane, channels: 1, samples: 3, plane: 3, rate: 48_000 };
        to_stereo(&s, &mut Resampler::default(), &mut out);
        assert_eq!(out, [0.1, 0.1, 0.2, 0.2, 0.3, 0.3]);
    }

    #[test]
    fn another_rate_keeps_its_length_and_joins_blocks_smoothly() {
        // A rising ramp at 44.1 kHz, cut into blocks: the result must rise
        // all along, block joins included, and last as long.
        let mut rs = Resampler::default();
        let (mut all, mut out) = (Vec::new(), Vec::new());
        for block in 0..100 {
            let planes: Vec<f32> =
                (0..2).flat_map(|_| (0..441).map(move |i| (block * 441 + i) as f32 / 44_100.0)).collect();
            let s = Sound { data: &planes, channels: 2, samples: 441, plane: 441, rate: 44_100 };
            to_stereo(&s, &mut rs, &mut out);
            all.extend_from_slice(&out);
        }
        let left: Vec<f32> = all.iter().step_by(2).copied().collect();
        assert!((left.len() as i64 - 48_000).abs() <= 2, "{} échantillons", left.len());
        assert!(left.windows(2).all(|w| w[1] > w[0]), "la rampe doit monter sans saut");
    }

    #[test]
    fn sent_sound_and_pictures_are_laid_out_as_the_protocols_want() {
        let mut planar = Vec::new();
        to_planar(&[1.0, -1.0, 2.0, -2.0], &mut planar);
        assert_eq!(planar, [1.0, 2.0, -1.0, -2.0]);
        let mut bgra = Vec::new();
        swap_red_blue(&[1, 2, 3, 4, 5, 6, 7, 8], &mut bgra);
        assert_eq!(bgra, [3, 2, 1, 4, 7, 6, 5, 8]);
    }
}
