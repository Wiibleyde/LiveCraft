//! NDI, written from the public SDK documentation at docs.ndi.video. The
//! runtime is proprietary: it is never shipped with LiveCraft, only loaded
//! from where the NDI installer put it, the first time someone needs it.

use crate::frame::Frame;
use crate::netvideo::{self, Alpha, Got, Library, Order, Picture, Sound};
use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr::{null, null_mut};
use std::sync::{LazyLock, Mutex};

const DLL: &str = "Processing.NDI.Lib.x64.dll";

#[repr(C)]
struct Source {
    name: *const c_char,
    url: *const c_char,
}

#[repr(C)]
struct FindCreate {
    show_local_sources: bool,
    groups: *const c_char,
    extra_ips: *const c_char,
}

#[repr(C)]
struct RecvCreate {
    source: Source,
    color_format: i32,
    bandwidth: i32,
    allow_video_fields: bool,
    name: *const c_char,
}

#[repr(C)]
struct SendCreate {
    name: *const c_char,
    groups: *const c_char,
    clock_video: bool,
    clock_audio: bool,
}

#[repr(C)]
struct VideoFrame {
    xres: i32,
    yres: i32,
    fourcc: u32,
    rate_n: i32,
    rate_d: i32,
    aspect: f32,
    format: i32,
    timecode: i64,
    data: *mut u8,
    stride: i32,
    metadata: *const c_char,
    timestamp: i64,
}

#[repr(C)]
struct AudioFrame {
    rate: i32,
    channels: i32,
    samples: i32,
    timecode: i64,
    fourcc: u32,
    data: *mut u8,
    stride: i32,
    metadata: *const c_char,
    timestamp: i64,
}

const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*code)
}

const RGBA: u32 = fourcc(b"RGBA");
const RGBX: u32 = fourcc(b"RGBX");
const FLTP: u32 = fourcc(b"FLTp");
const COLOR_RGBX_RGBA: i32 = 2;
const BANDWIDTH_HIGHEST: i32 = 100;
const PROGRESSIVE: i32 = 1;
const FRAME_VIDEO: i32 = 1;
const FRAME_AUDIO: i32 = 2;
const TIMECODE_SYNTHESIZE: i64 = i64::MAX;

type Instance = *mut c_void;

struct Api {
    find_create: unsafe extern "C" fn(*const FindCreate) -> Instance,
    find_wait: unsafe extern "C" fn(Instance, u32) -> bool,
    find_sources: unsafe extern "C" fn(Instance, *mut u32) -> *const Source,
    recv_create: unsafe extern "C" fn(*const RecvCreate) -> Instance,
    recv_destroy: unsafe extern "C" fn(Instance),
    recv_capture: unsafe extern "C" fn(Instance, *mut VideoFrame, *mut AudioFrame, *mut c_void, u32) -> i32,
    free_video: unsafe extern "C" fn(Instance, *const VideoFrame),
    free_audio: unsafe extern "C" fn(Instance, *const AudioFrame),
    send_create: unsafe extern "C" fn(*const SendCreate) -> Instance,
    send_destroy: unsafe extern "C" fn(Instance),
    send_video: unsafe extern "C" fn(Instance, *const VideoFrame),
    send_audio: unsafe extern "C" fn(Instance, *const AudioFrame),
    send_connections: unsafe extern "C" fn(Instance, u32) -> i32,
}

static API: LazyLock<Result<Api, String>> = LazyLock::new(load);

fn api() -> Result<&'static Api, String> {
    API.as_ref().map_err(Clone::clone)
}

/// The NDI installer names its folder in these variables; a copy beside the
/// program or on the PATH is found too.
fn load() -> Result<Api, String> {
    let candidates = netvideo::runtime_candidates(DLL, &["NDI_RUNTIME_DIR_V6", "NDI_RUNTIME_DIR_V5"]);
    let lib = Library::open(&candidates).ok_or(t!("ndi.missing"))?;
    // SAFETY: each type is the signature the SDK documents for that name, and
    // the library stays loaded for the life of the program.
    unsafe {
        let initialize: unsafe extern "C" fn() -> bool = lib.symbol("NDIlib_initialize")?;
        if !initialize() {
            return Err(t!("ndi.cpu").into());
        }
        Ok(Api {
            find_create: lib.symbol("NDIlib_find_create_v2")?,
            find_wait: lib.symbol("NDIlib_find_wait_for_sources")?,
            find_sources: lib.symbol("NDIlib_find_get_current_sources")?,
            recv_create: lib.symbol("NDIlib_recv_create_v3")?,
            recv_destroy: lib.symbol("NDIlib_recv_destroy")?,
            recv_capture: lib.symbol("NDIlib_recv_capture_v3")?,
            free_video: lib.symbol("NDIlib_recv_free_video_v2")?,
            free_audio: lib.symbol("NDIlib_recv_free_audio_v3")?,
            send_create: lib.symbol("NDIlib_send_create")?,
            send_destroy: lib.symbol("NDIlib_send_destroy")?,
            send_video: lib.symbol("NDIlib_send_send_video_v2")?,
            send_audio: lib.symbol("NDIlib_send_send_audio_v3")?,
            send_connections: lib.symbol("NDIlib_send_get_no_connections")?,
        })
    }
}

pub fn available() -> Result<(), String> {
    api().map(|_| ())
}

struct Finder(Instance);
// SAFETY: an NDI finder may be used from any thread; this one is behind a lock.
unsafe impl Send for Finder {}

/// Names of the sources seen on the network, this computer's included. The
/// finder stays up once made: sources announce themselves over time.
pub fn sources() -> Result<Vec<String>, String> {
    let api = api()?;
    static FINDER: Mutex<Option<Finder>> = Mutex::new(None);
    let mut finder = FINDER.lock().unwrap();
    if finder.is_none() {
        let settings = FindCreate { show_local_sources: true, groups: null(), extra_ips: null() };
        // SAFETY: the settings outlive the call.
        let made = unsafe { (api.find_create)(&settings) };
        if made.is_null() {
            return Err(t!("ndi.find_failed").into());
        }
        // SAFETY: `made` is a live finder. The first answers take a moment.
        unsafe { (api.find_wait)(made, 1500) };
        *finder = Some(Finder(made));
    }
    let Some(Finder(instance)) = *finder else { return Ok(Vec::new()) };
    let mut count = 0u32;
    // SAFETY: the list stays valid until the next call on this finder, which
    // the lock holds back until the names are copied.
    let list = unsafe { (api.find_sources)(instance, &mut count) };
    if list.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: the runtime returned `count` sources at `list`.
    let found = unsafe { std::slice::from_raw_parts(list, count as usize) };
    Ok(found
        .iter()
        .filter(|s| !s.name.is_null())
        // SAFETY: names are NUL-terminated UTF-8 owned by the finder.
        .map(|s| unsafe { CStr::from_ptr(s.name) }.to_string_lossy().into_owned())
        .collect())
}

pub struct Receiver {
    api: &'static Api,
    instance: Instance,
}

// SAFETY: NDI receivers may be used from any thread; each has one owner.
unsafe impl Send for Receiver {}

impl Receiver {
    pub fn new(source: &str) -> Result<Self, String> {
        let api = api()?;
        let name = CString::new(source).map_err(|_| t!("ndi.bad_source_name"))?;
        let own = CString::new("LiveCraft").unwrap();
        let settings = RecvCreate {
            source: Source { name: name.as_ptr(), url: null() },
            color_format: COLOR_RGBX_RGBA,
            bandwidth: BANDWIDTH_HIGHEST,
            allow_video_fields: false,
            name: own.as_ptr(),
        };
        // SAFETY: the settings and their strings outlive the call.
        let instance = unsafe { (api.recv_create)(&settings) };
        if instance.is_null() {
            return Err(t!("ndi.receiver_failed").into());
        }
        Ok(Self { api, instance })
    }
}

/// Row length and total size of a picture, or `None` for one that makes no
/// sense (empty, negative or too short a stride, or a size that overflows).
fn frame_size(w: usize, h: usize, stride: i32) -> Option<(usize, usize)> {
    let stride = usize::try_from(stride).ok()?;
    (w > 0 && h > 0 && stride >= w.checked_mul(4)?).then_some(())?;
    Some((stride, stride.checked_mul(h)?))
}

impl netvideo::Receiver for Receiver {
    fn capture(&mut self, timeout_ms: u32, on: &mut dyn FnMut(Got<'_>)) {
        // SAFETY: zeroed frames are valid empty frames for the runtime to fill.
        let mut video: VideoFrame = unsafe { std::mem::zeroed() };
        let mut audio: AudioFrame = unsafe { std::mem::zeroed() };
        // SAFETY: the instance is live and the frames are ours to fill.
        let kind =
            unsafe { (self.api.recv_capture)(self.instance, &mut video, &mut audio, null_mut(), timeout_ms) };
        match kind {
            FRAME_VIDEO => {
                let (w, h) = (video.xres.max(0) as usize, video.yres.max(0) as usize);
                let size = frame_size(w, h, video.stride);
                if let (false, Some((stride, size))) = (video.data.is_null(), size) {
                    let alpha = if video.fourcc == RGBA { Alpha::Straight } else { Alpha::Opaque };
                    if video.fourcc == RGBA || video.fourcc == RGBX {
                        // SAFETY: the runtime hands `h` rows of `stride` bytes.
                        let data = unsafe { std::slice::from_raw_parts(video.data, size) };
                        on(Got::Picture(Picture { data, w, h, stride, order: Order::Rgba, alpha }));
                    }
                }
                // SAFETY: the frame came from this receiver and is freed once.
                unsafe { (self.api.free_video)(self.instance, &video) };
            }
            FRAME_AUDIO => {
                let (channels, samples) = (audio.channels.max(0) as usize, audio.samples.max(0) as usize);
                let plane = audio.stride.max(0) as usize / 4;
                if audio.fourcc == FLTP && !audio.data.is_null() && channels > 0 && plane >= samples {
                    let len = plane * (channels - 1) + samples;
                    // SAFETY: planar floats, one plane of `stride` bytes per channel.
                    let data = unsafe { std::slice::from_raw_parts(audio.data as *const f32, len) };
                    on(Got::Sound(Sound { data, channels, samples, plane, rate: audio.rate.max(1) as u32 }));
                }
                // SAFETY: the frame came from this receiver and is freed once.
                unsafe { (self.api.free_audio)(self.instance, &audio) };
            }
            _ => {}
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        // SAFETY: the instance is live and nothing else uses it any more.
        unsafe { (self.api.recv_destroy)(self.instance) };
    }
}

pub struct Sender {
    api: &'static Api,
    instance: Instance,
    planar: Vec<f32>,
}

// SAFETY: NDI senders may be used from any thread; each has one owner.
unsafe impl Send for Sender {}

impl Sender {
    pub fn new(name: &str) -> Result<Self, String> {
        let api = api()?;
        let name = CString::new(name).map_err(|_| t!("ndi.bad_name"))?;
        // The program is already paced by the mixer: a second clock here would
        // only make the mixer wait.
        let settings =
            SendCreate { name: name.as_ptr(), groups: null(), clock_video: false, clock_audio: false };
        // SAFETY: the settings and their strings outlive the call.
        let instance = unsafe { (api.send_create)(&settings) };
        if instance.is_null() {
            return Err(t!("ndi.sender_failed").into());
        }
        Ok(Self { api, instance, planar: Vec::new() })
    }
}

impl netvideo::Sender for Sender {
    fn video(&mut self, f: &Frame, fps: u32) {
        let frame = VideoFrame {
            xres: f.w as i32,
            yres: f.h as i32,
            // The program is opaque: saying so spares the receivers the alpha.
            fourcc: RGBX,
            rate_n: fps as i32,
            rate_d: 1,
            aspect: 0.0,
            format: PROGRESSIVE,
            timecode: TIMECODE_SYNTHESIZE,
            data: f.data.as_ptr() as *mut u8,
            stride: (f.w * 4) as i32,
            metadata: null(),
            timestamp: 0,
        };
        // SAFETY: the runtime only reads the pixels, and is done before returning.
        unsafe { (self.api.send_video)(self.instance, &frame) };
    }

    fn audio(&mut self, stereo: &[f32]) {
        let samples = stereo.len() / 2;
        netvideo::to_planar(stereo, &mut self.planar);
        let frame = AudioFrame {
            rate: crate::sources::AUDIO_RATE as i32,
            channels: 2,
            samples: samples as i32,
            timecode: TIMECODE_SYNTHESIZE,
            fourcc: FLTP,
            data: self.planar.as_mut_ptr() as *mut u8,
            stride: (samples * 4) as i32,
            metadata: null(),
            timestamp: 0,
        };
        // SAFETY: the runtime only reads the samples, and is done before returning.
        unsafe { (self.api.send_audio)(self.instance, &frame) };
    }

    fn connections(&self) -> u32 {
        // SAFETY: the instance is live; a zero timeout only reads the count.
        unsafe { (self.api.send_connections)(self.instance, 0) }.max(0) as u32
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        // SAFETY: the instance is live and nothing else uses it any more.
        unsafe { (self.api.send_destroy)(self.instance) };
    }
}

#[cfg(test)]
mod tests {
    use super::frame_size;

    #[test]
    fn a_picture_with_a_wrong_stride_is_refused() {
        assert_eq!(frame_size(4, 2, 16), Some((16, 32)));
        assert_eq!(frame_size(4, 2, 20), Some((20, 40)), "padded rows");
        assert_eq!(frame_size(4, 2, -16), None, "negative stride");
        assert_eq!(frame_size(4, 2, i32::MIN), None);
        assert_eq!(frame_size(4, 2, 12), None, "shorter than a row");
        assert_eq!(frame_size(0, 2, 16), None);
        assert_eq!(frame_size(usize::MAX, 2, 16), None, "width overflow");
        if usize::BITS == 32 {
            assert_eq!(frame_size(4, usize::MAX, i32::MAX), None, "size overflow");
        }
    }
}
