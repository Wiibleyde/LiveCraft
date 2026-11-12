//! `--livecheck <folder>`: the path of a real show, with devices and a server
//! that are real to Windows. A virtual camera (Softcam) shows a counting
//! pattern and a virtual cable (VB-CABLE) carries a tone; LiveCraft takes them
//! through its own camera and microphone inputs, streams program over RTMP and
//! SRT to a local MediaMTX, and each stream is pulled back to read the count
//! and hear the tone. `--break freeze|silence|drop` spoils one link on purpose,
//! to show that the check notices.

use crate::engine::Engine;
use crate::functions::Args;
use crate::model::*;
use crate::sources::{self, ffmpeg};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::io::Read;
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle, sleep};
use std::time::{Duration, Instant};

/// Softcam's own filter identity and name, as its DLL registers them.
const CAM_CLSID: &str = "{AEF3B972-5FA5-4647-9571-358EB472BC9E}";
const CAM_NAME: &str = "DirectShow Softcam";
/// DirectShow's "video capture sources" category.
const VIDEO_INPUTS: &str = "{860BB310-5D01-11D0-BD3B-00A0C911CE86}";
const CABLE_IN: &str = "CABLE Input";
const CABLE_OUT: &str = "CABLE Output";

const CAM_W: usize = 640;
const CAM_H: usize = 360;
const CAM_FPS: f32 = 30.0;
const TONE_HZ: f32 = 1000.0;
const TONE_LEVEL: f32 = 0.25;

/// Bottom band of the pattern: a white cell, 13 bits of frame count, an even
/// parity bit, a black cell. Wide cells survive scaling and compression.
const CELLS: usize = 16;
const BITS: u32 = 13;

/// Pulled pictures are small: reading cells needs no detail.
const PULL_W: usize = 320;
const PULL_H: usize = 180;
const PULL_SECONDS: u32 = 4;
/// Different pictures a second the stream must show to count as moving.
const MIN_CHANGES_PER_SECOND: usize = 5;
const SAMPLE_RATE: usize = 48_000;

#[derive(Clone, Copy, PartialEq)]
enum Sabotage {
    None,
    /// The camera keeps showing the same picture.
    Freeze,
    /// The cable carries silence.
    Silence,
    /// LiveCraft stops streaming before anything is pulled.
    Drop,
}

pub fn run(dir: &str, sabotage: Option<&str>) -> i32 {
    let sabotage = match sabotage {
        None => Sabotage::None,
        Some("freeze") => Sabotage::Freeze,
        Some("silence") => Sabotage::Silence,
        Some("drop") => Sabotage::Drop,
        Some(other) => {
            println!("ECHEC: --break attend freeze, silence ou drop, pas {other}");
            return 2;
        }
    };
    match check(dir, sabotage) {
        Ok(msg) => {
            println!("OK: {msg}");
            0
        }
        Err(e) => {
            println!("ECHEC: {e}");
            1
        }
    }
}

fn check(dir: &str, sabotage: Sabotage) -> Result<String, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    sources::tie_children_to_this_process();
    let dll = std::env::var("LIVECRAFT_SOFTCAM")
        .map_err(|_| "LIVECRAFT_SOFTCAM doit donner le chemin de softcam.dll (voir README)".to_string())?;
    let mediamtx = std::env::var("LIVECRAFT_MEDIAMTX").unwrap_or_else(|_| "mediamtx".into());
    let (_, mics) = sources::list_devices();
    let mic = mics
        .into_iter()
        .find(|m| m.starts_with(CABLE_OUT))
        .ok_or("micro virtuel absent : installez VB-CABLE (voir README)")?;

    let _registered = Registration::add(&dll)?;
    let camera = Camera::start(&dll, sabotage == Sabotage::Freeze)?;
    let _tone = play_tone(if sabotage == Sabotage::Silence { 0.0 } else { TONE_LEVEL })?;
    let server = Server::start(&mediamtx, dir)?;

    let mut project = Project::blank();
    project.out_w = 1280;
    project.out_h = 720;
    project.fps = 30;
    // The tone must reach the stream, not the speakers of whoever runs this.
    project.headphones_level = 0.0;
    project.streams[0].enabled = true;
    project.streams[0].url = format!("rtmp://127.0.0.1:{}/live", server.rtmp);
    project.streams[0].key = "livecheck".into();
    project.srt[0].enabled = true;
    project.srt[0].url =
        format!("srt://127.0.0.1:{}?streamid=publish:livecheck-srt&pkt_size=1316", server.srt);
    let engine = Engine::start(project);
    let outcome = on_air(&engine, &server, &camera, &mic, dir, sabotage);
    // How the desk kept up explains a stream short of pictures.
    let dropped: u64 =
        engine.streamers.lock().unwrap().iter().map(|e| e.dropped.load(Ordering::Relaxed)).sum::<u64>()
            + engine.srt.lock().unwrap().iter().map(|(_, e)| e.dropped.load(Ordering::Relaxed)).sum::<u64>();
    let pace = format!(
        "cadence {}/s, rendu {} µs, images perdues {dropped}",
        engine.stats.ticks_per_sec.load(Ordering::Relaxed),
        engine.stats.mix_us.load(Ordering::Relaxed)
    );
    engine.shutdown();
    let mut lines = Vec::new();
    for (name, pull) in outcome.map_err(|e| format!("{e} ({pace})"))? {
        lines.push(format!("{name} {}", pull.map_err(|e| format!("{name} : {e} ({pace})"))?));
    }
    lines.push(pace);
    Ok(lines.join(" | "))
}

/// Everything that needs the engine, so that it is always shut down after.
fn on_air(
    engine: &Arc<Engine>,
    server: &Server,
    camera: &Camera,
    mic: &str,
    dir: &str,
    sabotage: Sabotage,
) -> Result<Vec<(&'static str, Result<String, String>)>, String> {
    let cam_id = engine.new_id();
    let kind = Kind::Camera { device: CAM_NAME.into(), ptz_host: String::new() };
    engine.add_input(Input::new(cam_id, "Caméra virtuelle", kind));
    let mic_id = engine.new_id();
    let mut input = Input::new(mic_id, "Micro virtuel", Kind::Microphone { device: mic.into() });
    input.audio.follow = false;
    engine.add_input(input);
    let cam = engine.rt(cam_id).ok_or("entrée caméra absente")?;
    let mic_rt = engine.rt(mic_id).ok_or("entrée micro absente")?;
    engine.call("CutDirect", &Args::new().input("Caméra virtuelle"))?;

    let end = Instant::now() + Duration::from_secs(20);
    while cam.seq() < 30 {
        if Instant::now() > end {
            let why = cam.status().unwrap_or_else(|| format!("{} images", cam.seq()));
            return Err(format!("la caméra virtuelle ne donne pas d'images : {why}"));
        }
        sleep(Duration::from_millis(50));
    }
    if !camera.connected() {
        return Err("Softcam n'a vu aucun lecteur : la caméra lue n'est pas la virtuelle".into());
    }

    engine.start_streaming()?;
    engine.start_srt()?;
    // Publishers connect, then the server holds a keyframe for readers.
    sleep(Duration::from_secs(3));
    if let Some(e) = engine.output_error.lock().unwrap().clone() {
        return Err(e);
    }
    if sabotage == Sabotage::Drop {
        engine.stop_streaming();
        engine.stop_srt();
        sleep(Duration::from_secs(1));
    }
    let rtmp = format!("rtmp://127.0.0.1:{}/live/livecheck", server.rtmp);
    let srt = format!("srt://127.0.0.1:{}?streamid=read:livecheck-srt", server.srt);
    let jobs: Vec<(&'static str, JoinHandle<Result<String, String>>)> = vec![
        ("RTMP", spawn_pull(dir.to_string(), "rtmp", rtmp)),
        ("SRT", spawn_pull(dir.to_string(), "srt", srt)),
    ];
    let results = jobs
        .into_iter()
        .map(|(name, job)| (name, job.join().unwrap_or_else(|_| Err("lecture interrompue".into()))))
        .collect();
    // A microphone that never opened explains a silent stream better.
    if let Some(e) = mic_rt.status() {
        return Err(format!("micro virtuel : {e}"));
    }
    if sabotage != Sabotage::Drop
        && let Some(e) = engine.output_error.lock().unwrap().clone()
    {
        return Err(e);
    }
    Ok(results)
}

fn spawn_pull(dir: String, name: &'static str, url: String) -> JoinHandle<Result<String, String>> {
    thread::spawn(move || {
        let pictures = format!("{dir}/{name}.gray");
        let sound = format!("{dir}/{name}.pcm");
        pull(&url, &pictures, &sound)?;
        let frames = std::fs::read(&pictures).map_err(|e| e.to_string())?;
        let pcm = std::fs::read(&sound).map_err(|e| e.to_string())?;
        let samples: Vec<f32> =
            pcm.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b) as f32 / 32768.0).collect();
        let picture = judge_pictures(&frames, PULL_W, PULL_H)?;
        let tone = judge_tone(&samples, SAMPLE_RATE)?;
        Ok(format!("{picture}, {tone}"))
    })
}

/// A publisher shows up at the server only once its encoder has a first
/// picture, which can take seconds; until then opening fails at once. A
/// stream that breaks once it has given pictures is not tried again.
fn pull(url: &str, pictures: &str, sound: &str) -> Result<(), String> {
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        // A file left by an earlier run must not pass for pictures received.
        let _ = std::fs::remove_file(pictures);
        let _ = std::fs::remove_file(sound);
        let tried = pull_once(url, pictures, sound);
        let nothing = std::fs::metadata(pictures).map_or(true, |m| m.len() == 0);
        if tried.is_ok() || !nothing || Instant::now() > end {
            return tried;
        }
        sleep(Duration::from_millis(500));
    }
}

/// Reads a few seconds of a stream as small grey pictures and mono sound.
fn pull_once(url: &str, pictures: &str, sound: &str) -> Result<(), String> {
    let seconds = PULL_SECONDS.to_string();
    let mut child = ffmpeg()
        .args(["-y", "-loglevel", "error", "-i", url])
        .args([
            "-map",
            "0:v:0",
            "-fps_mode",
            "passthrough",
            "-t",
            &seconds,
            "-s",
            &format!("{PULL_W}x{PULL_H}"),
        ])
        .args(["-pix_fmt", "gray", "-f", "rawvideo", pictures])
        .args(["-map", "0:a:0?", "-t", &seconds, "-ac", "1", "-ar", &SAMPLE_RATE.to_string()])
        .args(["-f", "s16le", sound])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| "ffmpeg est introuvable".to_string())?;
    let mut err = child.stderr.take().unwrap();
    let log = thread::spawn(move || {
        let mut text = String::new();
        let _ = err.read_to_string(&mut text);
        text
    });
    let end = Instant::now() + Duration::from_secs(25);
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
            break Some(s);
        }
        if Instant::now() > end {
            sources::end_process_tree(&mut child);
            break None;
        }
        sleep(Duration::from_millis(50));
    };
    let text = log.join().unwrap_or_default();
    let why = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("sans message").trim().to_string();
    match status {
        Some(s) if s.success() => Ok(()),
        Some(_) => Err(format!("flux illisible : {why}")),
        None => Err(format!("flux muet pendant 25 s : {why}")),
    }
}

/// The count shown in frame `n`, drawn into a top-down BGR picture.
fn draw_pattern(bgr: &mut [u8], w: usize, h: usize, count: u32) {
    bgr.fill(48);
    let band = h * 3 / 4;
    let cell = w / CELLS;
    let value = count & ((1 << BITS) - 1);
    let parity = value.count_ones() & 1;
    for c in 0..CELLS {
        let white = match c {
            0 => true,
            15 => false,
            14 => parity == 1,
            bit => value >> (BITS as usize - bit) & 1 == 1,
        };
        let shade = if white { 255 } else { 0 };
        for y in band..h {
            let row = y * w * 3;
            bgr[row + c * cell * 3..row + (c + 1) * cell * 3].fill(shade);
        }
    }
}

/// Reads the count back from a grey picture, or nothing if the cells are not
/// the pattern's.
fn read_pattern(gray: &[u8], w: usize, h: usize) -> Option<u32> {
    let cell = w / CELLS;
    let (top, bottom) = (h * 3 / 4 + h / 16, h - h / 16);
    let level = |c: usize| {
        let (left, right) = (c * cell + cell / 4, (c + 1) * cell - cell / 4);
        let mut sum = 0u32;
        for y in top..bottom {
            sum += gray[y * w + left..y * w + right].iter().map(|&v| v as u32).sum::<u32>();
        }
        sum / ((bottom - top) * (right - left)) as u32
    };
    let (white, black) = (level(0), level(15));
    if white < black + 100 {
        return None;
    }
    let middle = (white + black) / 2;
    let bits: Vec<bool> = (1..15).map(|c| level(c) > middle).collect();
    if bits.iter().filter(|&&b| b).count() % 2 == 1 {
        return None;
    }
    Some(bits[..BITS as usize].iter().fold(0, |v, &b| v << 1 | b as u32))
}

/// The camera's count must be readable and keep moving forward.
fn judge_pictures(frames: &[u8], w: usize, h: usize) -> Result<String, String> {
    let counts: Vec<Option<u32>> = frames.chunks_exact(w * h).map(|f| read_pattern(f, w, h)).collect();
    let total = counts.len();
    let wanted = PULL_SECONDS as usize * MIN_CHANGES_PER_SECOND;
    if total < wanted {
        return Err(format!("{total} images reçues en {PULL_SECONDS} s, au moins {wanted} attendues"));
    }
    let read: Vec<u32> = counts.iter().flatten().copied().collect();
    if read.len() * 10 < total * 9 {
        return Err(format!("mire illisible sur {} images sur {total}", total - read.len()));
    }
    if let Some(pair) = read.windows(2).find(|p| p[1] < p[0]) {
        return Err(format!("images dans le désordre : {} puis {}", pair[0], pair[1]));
    }
    let (first, last) = (read[0], read[read.len() - 1]);
    let mut distinct = read.clone();
    distinct.dedup();
    // A loaded machine makes a camera stutter, which is not what this looks
    // for; a picture that stops or barely changes is.
    let changes = PULL_SECONDS as usize * MIN_CHANGES_PER_SECOND;
    if distinct.len() < changes {
        return Err(format!(
            "image figée : {} images différentes sur {total}, au moins {changes} attendues, compteur de {first} à {last}",
            distinct.len()
        ));
    }
    Ok(format!("{total} images, mire {first} à {last} ({} différentes)", distinct.len()))
}

/// The tone must be there, loud enough and mostly alone.
pub(crate) fn judge_tone(samples: &[f32], rate: usize) -> Result<String, String> {
    // The first half second may still hold what the encoder started with.
    let samples = samples.get(rate / 2..).unwrap_or_default();
    if samples.len() < rate {
        return Err(format!("{} ms de son reçus", samples.len() * 1000 / rate));
    }
    let power = samples.iter().map(|v| v * v).sum::<f32>() / samples.len() as f32;
    // Goertzel: the strength of one frequency, without a full spectrum.
    let k = 2.0 * (std::f32::consts::TAU * TONE_HZ / rate as f32).cos();
    let (mut s1, mut s2) = (0f32, 0f32);
    for &v in samples {
        let s0 = v + k * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let n = samples.len() as f32;
    let tone_power = 2.0 * (s1 * s1 + s2 * s2 - k * s1 * s2) / (n * n);
    let db = |p: f32| 10.0 * p.max(1e-12).log10();
    if tone_power < 1e-4 {
        return Err(format!("pas de son à {TONE_HZ} Hz : {:.0} dB au total", db(power)));
    }
    if tone_power < power * 0.8 {
        return Err(format!(
            "son brouillé : la tonalité ne fait que {:.0} % du son",
            tone_power / power * 100.0
        ));
    }
    Ok(format!("tonalité {TONE_HZ} Hz à {:.0} dB", db(tone_power)))
}

/// Softcam registered for this user only, so no administrator is needed;
/// removed afterwards. It refuses to run over keys that already exist, so that
/// nothing of the user's is overwritten and nothing needs restoring.
struct Registration {
    added: Vec<String>,
}

impl Registration {
    fn add(dll: &str) -> Result<Self, String> {
        if !std::path::Path::new(dll).is_file() {
            return Err(format!("softcam.dll introuvable : {dll}"));
        }
        let server = format!("Software\\Classes\\CLSID\\{CAM_CLSID}");
        let instance = format!("Software\\Classes\\CLSID\\{VIDEO_INPUTS}\\Instance\\{CAM_CLSID}");
        for key in [&server, &instance] {
            if registry::exists(key) {
                return Err(format!(
                    "HKCU\\{key} existe déjà (une caméra Softcam est déjà inscrite) : retirez-la ou fermez ce qui l'utilise avant de lancer le test"
                ));
            }
        }
        let me = Self { added: vec![server.clone(), instance.clone()] };
        let server_dll = format!("{server}\\InprocServer32");
        registry::set(&server_dll, None, dll)?;
        registry::set(&server_dll, Some("ThreadingModel"), "Both")?;
        registry::set(&instance, Some("FriendlyName"), CAM_NAME)?;
        registry::set(&instance, Some("CLSID"), CAM_CLSID)?;
        Ok(me)
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        for key in &self.added {
            registry::remove(key);
        }
    }
}

mod registry {
    use windows_sys::Win32::System::Registry::*;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn exists(key: &str) -> bool {
        let mut handle = std::ptr::null_mut();
        // SAFETY: the name is NUL-terminated and the handle is closed at once.
        unsafe {
            let found = RegOpenKeyExW(HKEY_CURRENT_USER, wide(key).as_ptr(), 0, KEY_READ, &mut handle) == 0;
            if found {
                RegCloseKey(handle);
            }
            found
        }
    }

    pub fn set(key: &str, name: Option<&str>, value: &str) -> Result<(), String> {
        let data = wide(value);
        let name = name.map(wide);
        // SAFETY: strings are NUL-terminated and the size covers `data`.
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                wide(key).as_ptr(),
                name.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()),
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        };
        if status == 0 { Ok(()) } else { Err(format!("registre {key} : erreur {status}")) }
    }

    pub fn remove(key: &str) {
        // SAFETY: the name is NUL-terminated.
        unsafe {
            RegDeleteTreeW(HKEY_CURRENT_USER, wide(key).as_ptr());
            RegDeleteKeyW(HKEY_CURRENT_USER, wide(key).as_ptr());
        }
    }
}

type ScCreate = unsafe extern "C" fn(i32, i32, f32) -> *mut std::ffi::c_void;
type ScSend = unsafe extern "C" fn(*mut std::ffi::c_void, *const std::ffi::c_void);
type ScDelete = unsafe extern "C" fn(*mut std::ffi::c_void);
type ScConnected = unsafe extern "C" fn(*mut std::ffi::c_void) -> bool;

/// Feeds Softcam from this process: the DLL holds both the filter that
/// ffmpeg opens and the sender side used here.
struct Camera {
    stop: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Camera {
    fn start(dll: &str, frozen: bool) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let connected = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (dll, halt, seen) = (dll.to_string(), stop.clone(), connected.clone());
        let worker = thread::spawn(move || {
            let api = match Softcam::load(&dll) {
                Ok(api) => api,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            // SAFETY: the functions come from the DLL with the signatures of
            // softcam.h; the camera handle stays on this thread until deleted.
            unsafe {
                let cam = (api.create)(CAM_W as i32, CAM_H as i32, CAM_FPS);
                if cam.is_null() {
                    let _ =
                        ready_tx.send(Err("Softcam refuse : une autre caméra Softcam tourne déjà".into()));
                    return;
                }
                let _ = ready_tx.send(Ok(()));
                let mut picture = vec![0u8; CAM_W * CAM_H * 3];
                let mut count = 0;
                while !halt.load(Ordering::Relaxed) {
                    draw_pattern(&mut picture, CAM_W, CAM_H, count);
                    // Paced by Softcam itself at the frame rate given above.
                    (api.send)(cam, picture.as_ptr().cast());
                    if (api.connected)(cam) {
                        seen.store(true, Ordering::Relaxed);
                    }
                    if !frozen {
                        count += 1;
                    }
                }
                (api.delete)(cam);
            }
        });
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self { stop, connected, worker: Some(worker) }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("la caméra virtuelle s'est arrêtée".into()),
        }
    }

    fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

struct Softcam {
    create: ScCreate,
    send: ScSend,
    delete: ScDelete,
    connected: ScConnected,
}

impl Softcam {
    fn load(dll: &str) -> Result<Self, String> {
        use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
        let path: Vec<u16> = dll.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: the path is NUL-terminated; the library stays loaded for the
        // rest of the process, so the function pointers never dangle.
        unsafe {
            let lib = LoadLibraryW(path.as_ptr());
            if lib.is_null() {
                return Err(format!("impossible de charger {dll}"));
            }
            let find = |name: &[u8]| {
                GetProcAddress(lib, name.as_ptr()).ok_or_else(|| {
                    format!("{dll} ne contient pas {}", String::from_utf8_lossy(&name[..name.len() - 1]))
                })
            };
            Ok(Self {
                create: std::mem::transmute::<unsafe extern "system" fn() -> isize, ScCreate>(find(
                    b"scCreateCamera\0",
                )?),
                send: std::mem::transmute::<unsafe extern "system" fn() -> isize, ScSend>(find(
                    b"scSendFrame\0",
                )?),
                delete: std::mem::transmute::<unsafe extern "system" fn() -> isize, ScDelete>(find(
                    b"scDeleteCamera\0",
                )?),
                connected: std::mem::transmute::<unsafe extern "system" fn() -> isize, ScConnected>(find(
                    b"scIsConnected\0",
                )?),
            })
        }
    }
}

/// A steady tone into the cable's playback side; it comes out of its
/// recording side, which LiveCraft opens as a microphone.
fn play_tone(level: f32) -> Result<cpal::Stream, String> {
    let host = cpal::default_host();
    let device = host
        .output_devices()
        .map_err(|e| e.to_string())?
        .find(|d| d.description().is_ok_and(|x| x.name().starts_with(CABLE_IN)))
        .ok_or("sortie CABLE Input absente : installez VB-CABLE (voir README)")?;
    let config = device.default_output_config().map_err(|e| e.to_string())?;
    if config.sample_format() != cpal::SampleFormat::F32 {
        return Err(format!("CABLE Input en {:?}, F32 attendu", config.sample_format()));
    }
    let config: cpal::StreamConfig = config.into();
    let channels = config.channels as usize;
    let step = std::f32::consts::TAU * TONE_HZ / config.sample_rate as f32;
    let mut phase = 0f32;
    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                for frame in data.chunks_mut(channels) {
                    frame.fill(phase.sin() * level);
                    phase = (phase + step) % std::f32::consts::TAU;
                }
            },
            |_| {},
            None,
        )
        .map_err(|e| format!("CABLE Input : {e}"))?;
    stream.play().map_err(|e| format!("CABLE Input : {e}"))?;
    Ok(stream)
}

/// A MediaMTX of our own on free ports, ended with the check.
struct Server {
    child: Child,
    rtmp: u16,
    srt: u16,
}

impl Server {
    fn start(program: &str, dir: &str) -> Result<Self, String> {
        let rtmp =
            TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).map_err(|e| e.to_string())?.port();
        let srt =
            UdpSocket::bind("127.0.0.1:0").and_then(|s| s.local_addr()).map_err(|e| e.to_string())?.port();
        let config = format!("{dir}/mediamtx.yml");
        let text = format!(
            "logLevel: info\napi: false\nmetrics: false\npprof: false\nplayback: false\nrtsp: false\n\
             hls: false\nwebrtc: false\nmoq: false\nrtmp: true\nrtmpAddress: 127.0.0.1:{rtmp}\n\
             srt: true\nsrtAddress: 127.0.0.1:{srt}\npaths:\n  all_others:\n"
        );
        std::fs::write(&config, text).map_err(|e| e.to_string())?;
        let log = std::fs::File::create(format!("{dir}/mediamtx.log")).map_err(|e| e.to_string())?;
        let mut child = Command::new(program)
            .arg(&config)
            .stdin(Stdio::null())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log)
            .spawn()
            .map_err(|_| {
                format!("{program} introuvable : installez MediaMTX ou indiquez LIVECRAFT_MEDIAMTX")
            })?;
        let end = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(("127.0.0.1", rtmp)).is_err() {
            let gone = child.try_wait().ok().flatten().is_some();
            if gone || Instant::now() > end {
                sources::end_process_tree(&mut child);
                let log = std::fs::read_to_string(format!("{dir}/mediamtx.log")).unwrap_or_default();
                let last = log.lines().last().unwrap_or("sans message").to_string();
                return Err(format!("MediaMTX ne démarre pas : {last}"));
            }
            sleep(Duration::from_millis(100));
        }
        Ok(Self { child, rtmp, srt })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        sources::end_process_tree(&mut self.child);
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_gray(bgr: &[u8]) -> Vec<u8> {
        bgr.as_chunks::<3>()
            .0
            .iter()
            .map(|[b, g, r]| ((*b as u32 + *g as u32 + *r as u32) / 3) as u8)
            .collect()
    }

    fn halve(gray: &[u8], w: usize, h: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(w * h / 4);
        for y in (0..h).step_by(2) {
            for x in (0..w).step_by(2) {
                out.push(gray[y * w + x]);
            }
        }
        out
    }

    fn frames(counts: impl Iterator<Item = u32>) -> Vec<u8> {
        let mut bgr = vec![0u8; CAM_W * CAM_H * 3];
        let mut all = Vec::new();
        for n in counts {
            draw_pattern(&mut bgr, CAM_W, CAM_H, n);
            all.extend(halve(&to_gray(&bgr), CAM_W, CAM_H));
        }
        all
    }

    #[test]
    fn the_count_survives_scaling() {
        for n in [0, 1, 2, 1000, 4095, 8191] {
            let gray = halve(
                &to_gray(&{
                    let mut bgr = vec![0u8; CAM_W * CAM_H * 3];
                    draw_pattern(&mut bgr, CAM_W, CAM_H, n);
                    bgr
                }),
                CAM_W,
                CAM_H,
            );
            assert_eq!(read_pattern(&gray, CAM_W / 2, CAM_H / 2), Some(n));
        }
    }

    #[test]
    fn a_picture_without_the_pattern_is_not_read() {
        let flat = vec![128u8; PULL_W * PULL_H];
        assert_eq!(read_pattern(&flat, PULL_W, PULL_H), None);
        let mut bgr = vec![0u8; CAM_W * CAM_H * 3];
        draw_pattern(&mut bgr, CAM_W, CAM_H, 5);
        let mut gray = halve(&to_gray(&bgr), CAM_W, CAM_H);
        // One cell flipped breaks the parity.
        let cell = PULL_W / CELLS;
        for y in PULL_H * 3 / 4..PULL_H {
            gray[y * PULL_W + 3 * cell..y * PULL_W + 4 * cell].iter_mut().for_each(|v| *v = 255 - *v);
        }
        assert_eq!(read_pattern(&gray, PULL_W, PULL_H), None);
    }

    #[test]
    fn moving_pictures_pass() {
        assert!(judge_pictures(&frames(100..220), PULL_W, PULL_H).is_ok());
    }

    #[test]
    fn a_frozen_camera_fails() {
        let e = judge_pictures(&frames(std::iter::repeat_n(7, 120)), PULL_W, PULL_H).unwrap_err();
        assert!(e.starts_with("image figée"), "{e}");
    }

    #[test]
    fn a_stuttering_camera_still_moves_but_one_stuck_most_of_the_time_does_not() {
        let stutter = (0..30).flat_map(|n| std::iter::repeat_n(n, 4));
        assert!(judge_pictures(&frames(stutter), PULL_W, PULL_H).is_ok());
        let mostly_stuck = std::iter::repeat_n(7, 110).chain(8..18);
        let e = judge_pictures(&frames(mostly_stuck), PULL_W, PULL_H).unwrap_err();
        assert!(e.starts_with("image figée"), "{e}");
    }

    #[test]
    fn pictures_going_back_fail() {
        let e = judge_pictures(&frames((0..60).chain(30..90)), PULL_W, PULL_H).unwrap_err();
        assert!(e.starts_with("images dans le désordre"), "{e}");
    }

    #[test]
    fn too_few_pictures_fail() {
        let e = judge_pictures(&frames(0..12), PULL_W, PULL_H).unwrap_err();
        assert!(e.starts_with("12 images reçues"), "{e}");
    }

    fn sine(hz: f32, level: f32, seconds: f32) -> Vec<f32> {
        let n = (SAMPLE_RATE as f32 * seconds) as usize;
        (0..n).map(|i| (std::f32::consts::TAU * hz * i as f32 / SAMPLE_RATE as f32).sin() * level).collect()
    }

    #[test]
    fn the_tone_is_heard() {
        assert!(judge_tone(&sine(TONE_HZ, TONE_LEVEL, 4.0), SAMPLE_RATE).is_ok());
    }

    #[test]
    fn silence_fails() {
        let e = judge_tone(&vec![0.0; SAMPLE_RATE * 4], SAMPLE_RATE).unwrap_err();
        assert!(e.starts_with("pas de son"), "{e}");
    }

    #[test]
    fn another_sound_fails() {
        assert!(judge_tone(&sine(440.0, TONE_LEVEL, 4.0), SAMPLE_RATE).is_err());
        let mut noisy = sine(TONE_HZ, 0.05, 4.0);
        let mut seed = 1u32;
        for v in noisy.iter_mut() {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *v += (seed >> 8) as f32 / (1 << 24) as f32 - 0.5;
        }
        let e = judge_tone(&noisy, SAMPLE_RATE).unwrap_err();
        assert!(e.starts_with("son brouillé"), "{e}");
    }
}
