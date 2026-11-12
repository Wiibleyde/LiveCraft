//! Open Media Transport, through libomt (MIT, openmediatransport on GitHub),
//! written from its public `libomt.h`. The library and its codec, libvmx, are
//! loaded at run time from beside the program or from the PATH.

use crate::frame::Frame;
use crate::netvideo::{self, Alpha, Got, Library, Order, Picture, Sound};
use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr::null_mut;
use std::sync::LazyLock;
use std::time::Instant;

const DLL: &str = "libomt.dll";
pub const MISSING: &str = "Bibliothèque OMT introuvable : placez libomt.dll et libvmx.dll \
     (github.com/openmediatransport) à côté de LiveCraft ou dans le PATH, puis relancez LiveCraft";

#[repr(C)]
struct MediaFrame {
    kind: i32,
    timestamp: i64,
    codec: u32,
    width: i32,
    height: i32,
    stride: i32,
    flags: i32,
    rate_n: i32,
    rate_d: i32,
    aspect: f32,
    colour_space: i32,
    sample_rate: i32,
    channels: i32,
    samples: i32,
    data: *mut c_void,
    data_len: i32,
    compressed: *mut c_void,
    compressed_len: i32,
    frame_metadata: *mut c_void,
    frame_metadata_len: i32,
}

const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*code)
}

const BGRA: u32 = fourcc(b"BGRA");
const BGRX: u32 = fourcc(b"BGRX");
const FPA1: u32 = fourcc(b"FPA1");
const FRAME_VIDEO: i32 = 2;
const FRAME_AUDIO: i32 = 4;
const FLAG_ALPHA: i32 = 2;
const FLAG_PREMULTIPLIED: i32 = 4;
const PREFER_BGRA: i32 = 2;
const QUALITY_DEFAULT: i32 = 0;

type Instance = *mut c_void;

struct Api {
    addresses: unsafe extern "C" fn(*mut i32) -> *mut *mut c_char,
    receive_create: unsafe extern "C" fn(*const c_char, i32, i32, i32) -> Instance,
    receive_destroy: unsafe extern "C" fn(Instance),
    receive: unsafe extern "C" fn(Instance, i32, i32) -> *const MediaFrame,
    send_create: unsafe extern "C" fn(*const c_char, i32) -> Instance,
    send_destroy: unsafe extern "C" fn(Instance),
    send: unsafe extern "C" fn(Instance, *mut MediaFrame) -> i32,
    send_connections: unsafe extern "C" fn(Instance) -> i32,
}

static API: LazyLock<Result<Api, String>> = LazyLock::new(load);

fn api() -> Result<&'static Api, String> {
    API.as_ref().map_err(Clone::clone)
}

fn load() -> Result<Api, String> {
    let candidates = netvideo::runtime_candidates(DLL, &[]);
    let lib = Library::open(&candidates).ok_or(MISSING)?;
    // SAFETY: each type is the signature `libomt.h` declares for that name, and
    // the library stays loaded for the life of the program.
    unsafe {
        Ok(Api {
            addresses: lib.symbol("omt_discovery_getaddresses")?,
            receive_create: lib.symbol("omt_receive_create")?,
            receive_destroy: lib.symbol("omt_receive_destroy")?,
            receive: lib.symbol("omt_receive")?,
            send_create: lib.symbol("omt_send_create")?,
            send_destroy: lib.symbol("omt_send_destroy")?,
            send: lib.symbol("omt_send")?,
            send_connections: lib.symbol("omt_send_connections")?,
        })
    }
}

pub fn available() -> Result<(), String> {
    api().map(|_| ())
}

/// Names of the senders seen on the network, this computer's included.
pub fn sources() -> Result<Vec<String>, String> {
    let api = api()?;
    static FIRST: std::sync::Once = std::sync::Once::new();
    let read = || {
        let mut count = 0i32;
        // SAFETY: the list stays valid until the next call; it is copied at once.
        let list = unsafe { (api.addresses)(&mut count) };
        if list.is_null() || count <= 0 {
            return Vec::new();
        }
        // SAFETY: the library returned `count` string pointers at `list`.
        let names = unsafe { std::slice::from_raw_parts(list, count as usize) };
        names
            .iter()
            .filter(|p| !p.is_null())
            // SAFETY: each name is NUL-terminated UTF-8.
            .map(|p| unsafe { CStr::from_ptr(*p) }.to_string_lossy().into_owned())
            .collect()
    };
    // Asking starts the search; the first answers take a moment to come.
    let mut found = read();
    FIRST.call_once(|| {
        if found.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            found = read();
        }
    });
    Ok(found)
}

pub struct Receiver {
    api: &'static Api,
    instance: Instance,
}

// SAFETY: an OMT receiver may be used from any thread; each has one owner.
unsafe impl Send for Receiver {}

impl Receiver {
    pub fn new(source: &str) -> Result<Self, String> {
        let api = api()?;
        let address = CString::new(source).map_err(|_| "Nom de source OMT invalide")?;
        // SAFETY: the address outlives the call.
        let instance =
            unsafe { (api.receive_create)(address.as_ptr(), FRAME_VIDEO | FRAME_AUDIO, PREFER_BGRA, 0) };
        if instance.is_null() {
            return Err("Le récepteur OMT n'a pas pu être créé".into());
        }
        Ok(Self { api, instance })
    }
}

impl netvideo::Receiver for Receiver {
    fn capture(&mut self, timeout_ms: u32, on: &mut dyn FnMut(Got<'_>)) {
        let wait = timeout_ms.min(i32::MAX as u32) as i32;
        // SAFETY: the instance is live; the frame stays valid until the next call.
        let got = unsafe { (self.api.receive)(self.instance, FRAME_VIDEO | FRAME_AUDIO, wait) };
        // SAFETY: a non-null result points at a frame owned by the receiver.
        let Some(f) = (unsafe { got.as_ref() }) else { return };
        if f.data.is_null() {
            return;
        }
        match f.kind {
            FRAME_VIDEO if f.codec == BGRA || f.codec == BGRX => {
                let (w, h) = (f.width.max(0) as usize, f.height.max(0) as usize);
                let stride = f.stride.max(0) as usize;
                if w == 0 || h == 0 || stride < w * 4 || (f.data_len.max(0) as usize) < stride * h {
                    return;
                }
                let alpha = if f.codec == BGRX || f.flags & FLAG_ALPHA == 0 {
                    Alpha::Opaque
                } else if f.flags & FLAG_PREMULTIPLIED != 0 {
                    Alpha::Premultiplied
                } else {
                    Alpha::Straight
                };
                // SAFETY: `data_len` bytes, checked above to hold `h` rows.
                let data = unsafe { std::slice::from_raw_parts(f.data as *const u8, stride * h) };
                on(Got::Picture(Picture { data, w, h, stride, order: Order::Bgra, alpha }));
            }
            FRAME_AUDIO if f.codec == FPA1 => {
                let (channels, samples) = (f.channels.max(0) as usize, f.samples.max(0) as usize);
                if channels == 0 || (f.data_len.max(0) as usize) < channels * samples * 4 {
                    return;
                }
                // SAFETY: planar floats, `samples` per channel, checked above.
                let data = unsafe { std::slice::from_raw_parts(f.data as *const f32, channels * samples) };
                let rate = f.sample_rate.max(1) as u32;
                on(Got::Sound(Sound { data, channels, samples, plane: samples, rate }));
            }
            _ => {}
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        // SAFETY: the instance is live and nothing else uses it any more.
        unsafe { (self.api.receive_destroy)(self.instance) };
    }
}

pub struct Sender {
    api: &'static Api,
    instance: Instance,
    began: Instant,
    bgra: Vec<u8>,
    planar: Vec<f32>,
}

// SAFETY: an OMT sender may be used from any thread; each has one owner.
unsafe impl Send for Sender {}

impl Sender {
    pub fn new(name: &str) -> Result<Self, String> {
        let api = api()?;
        let name = CString::new(name).map_err(|_| "Nom de sortie OMT invalide")?;
        // SAFETY: the name outlives the call.
        let instance = unsafe { (api.send_create)(name.as_ptr(), QUALITY_DEFAULT) };
        if instance.is_null() {
            return Err("La sortie OMT n'a pas pu être créée (nom déjà pris ?)".into());
        }
        Ok(Self { api, instance, began: Instant::now(), bgra: Vec::new(), planar: Vec::new() })
    }

    /// In the protocol's units of 100 ns. Receivers line pictures and sound up
    /// with it, so both come from the same clock.
    fn now(&self) -> i64 {
        (self.began.elapsed().as_nanos() / 100) as i64
    }

    fn frame(&self, kind: i32, codec: u32) -> MediaFrame {
        MediaFrame {
            kind,
            timestamp: self.now(),
            codec,
            width: 0,
            height: 0,
            stride: 0,
            flags: 0,
            rate_n: 0,
            rate_d: 0,
            aspect: 0.0,
            colour_space: 0,
            sample_rate: 0,
            channels: 0,
            samples: 0,
            data: null_mut(),
            data_len: 0,
            compressed: null_mut(),
            compressed_len: 0,
            frame_metadata: null_mut(),
            frame_metadata_len: 0,
        }
    }
}

impl netvideo::Sender for Sender {
    fn video(&mut self, f: &Frame, fps: u32) {
        // OMT takes blue first; without the alpha flag it reads BGRX.
        netvideo::swap_red_blue(&f.data, &mut self.bgra);
        let mut frame = self.frame(FRAME_VIDEO, BGRA);
        frame.width = f.w as i32;
        frame.height = f.h as i32;
        frame.stride = (f.w * 4) as i32;
        frame.rate_n = fps as i32;
        frame.rate_d = 1;
        frame.aspect = f.w as f32 / f.h.max(1) as f32;
        frame.data = self.bgra.as_mut_ptr() as *mut c_void;
        frame.data_len = self.bgra.len() as i32;
        // SAFETY: the pixels stay alive and untouched until the call returns.
        unsafe { (self.api.send)(self.instance, &mut frame) };
    }

    fn audio(&mut self, stereo: &[f32]) {
        let samples = stereo.len() / 2;
        netvideo::to_planar(stereo, &mut self.planar);
        let mut frame = self.frame(FRAME_AUDIO, FPA1);
        frame.sample_rate = crate::sources::AUDIO_RATE as i32;
        frame.channels = 2;
        frame.samples = samples as i32;
        frame.data = self.planar.as_mut_ptr() as *mut c_void;
        frame.data_len = (self.planar.len() * 4) as i32;
        // SAFETY: the samples stay alive and untouched until the call returns.
        unsafe { (self.api.send)(self.instance, &mut frame) };
    }

    fn connections(&self) -> u32 {
        // SAFETY: the instance is live.
        unsafe { (self.api.send_connections)(self.instance) }.max(0) as u32
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        // SAFETY: the instance is live and nothing else uses it any more.
        unsafe { (self.api.send_destroy)(self.instance) };
    }
}
