//! macOS: AVFoundation capture, Core Graphics screens and the Keychain.

use super::unix::{Keystore, find_on_path, home, is_program};
use std::path::PathBuf;
use std::process::Stdio;

/// AVFoundation's devices by number, as it lists them: (video, audio). The
/// video list includes the screens, named "Capture screen N".
fn devices() -> (Vec<(u32, String)>, Vec<(u32, String)>) {
    let listed = crate::sources::ffmpeg()
        .args(["-f", "avfoundation", "-list_devices", "true", "-i", ""])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    match listed {
        Ok(out) => parse_devices(&String::from_utf8_lossy(&out.stderr)),
        Err(_) => (Vec::new(), Vec::new()),
    }
}

fn parse_devices(log: &str) -> (Vec<(u32, String)>, Vec<(u32, String)>) {
    let (mut video, mut audio) = (Vec::new(), Vec::new());
    let mut in_audio = false;
    for line in log.lines() {
        if line.contains("AVFoundation video devices") {
            in_audio = false;
        } else if line.contains("AVFoundation audio devices") {
            in_audio = true;
        } else if let Some((_, rest)) = line.split_once("] [")
            && let Some((number, name)) = rest.split_once("] ")
            && let Ok(number) = number.parse()
        {
            let list = if in_audio { &mut audio } else { &mut video };
            list.push((number, name.trim().to_string()));
        }
    }
    (video, audio)
}

fn is_screen(name: &str) -> bool {
    name.starts_with("Capture screen")
}

pub fn list_devices() -> (Vec<String>, Vec<String>) {
    let (video, audio) = devices();
    let cams = video.into_iter().map(|(_, n)| n).filter(|n| !is_screen(n)).collect();
    (cams, audio.into_iter().map(|(_, n)| n).collect())
}

/// Devices open by number, looked up just before: a name holding a colon
/// would be read as a video and audio pair.
fn number_of(list: &[(u32, String)], name: &str) -> String {
    list.iter().find(|(_, n)| n == name).map_or_else(|| name.to_string(), |(i, _)| i.to_string())
}

pub fn camera_input(device: &str) -> Vec<String> {
    let input = format!("{}:none", number_of(&devices().0, device));
    // Built-in cameras refuse ffmpeg's default of 29.97 pictures a second.
    ["-f", "avfoundation", "-framerate", "30", "-i", &input].map(String::from).to_vec()
}

pub fn microphone_input(device: &str) -> Vec<String> {
    let input = format!("none:{}", number_of(&devices().1, device));
    ["-f", "avfoundation", "-i", &input].map(String::from).to_vec()
}

/// None: Core Audio's own loopback, opened through cpal, is used here.
pub fn system_audio_input() -> Option<Vec<String>> {
    None
}

/// The main screen, which macOS lists first.
pub fn screen_input(fps: u32) -> Result<Vec<String>, String> {
    let screen = devices().0.into_iter().find(|(_, n)| is_screen(n));
    let (number, _) = screen.ok_or("Aucun écran à capturer : autorisez l'enregistrement de l'écran")?;
    let input = format!("{number}:none");
    let rate = fps.to_string();
    Ok(["-f", "avfoundation", "-capture_cursor", "1", "-framerate", &rate, "-i", &input]
        .map(String::from)
        .to_vec())
}

#[repr(C)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGMainDisplayID() -> u32;
}

/// Display rectangles, main first: left, top, width, height, in points.
pub fn list_screens() -> Vec<(i32, i32, i32, i32)> {
    let mut ids = [0u32; 16];
    let mut count = 0u32;
    // SAFETY: the list has room for the number given, and the count is
    // written before the call returns.
    let failed = unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) } != 0;
    if failed {
        return Vec::new();
    }
    // SAFETY: these only read the state of the displays named.
    let main = unsafe { CGMainDisplayID() };
    let mut list: Vec<_> = ids[..(count as usize).min(ids.len())]
        .iter()
        .map(|&id| {
            // SAFETY: as above.
            let r = unsafe { CGDisplayBounds(id) };
            let rect = (r.origin.x as i32, r.origin.y as i32, r.size.width as i32, r.size.height as i32);
            (id != main, rect)
        })
        .collect();
    list.sort_by_key(|&(other, (x, y, _, _))| (other, y, x));
    list.into_iter().map(|(_, r)| r).collect()
}

pub fn find_browser() -> Option<PathBuf> {
    let apps = [
        "Google Chrome.app/Contents/MacOS/Google Chrome",
        "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "Chromium.app/Contents/MacOS/Chromium",
    ];
    let roots = [PathBuf::from("/Applications"), home().join("Applications")];
    let installed = apps.iter().flat_map(|app| roots.iter().map(move |root| root.join(app)));
    installed.into_iter().find(|p| is_program(p)).or_else(|| find_on_path(&["chromium", "google-chrome"]))
}

const SERVICE: &str = "LiveCraft";
const ACCOUNT: &str = "session-key";
/// What the Keychain answers for an item it does not hold.
const ITEM_NOT_FOUND: i32 = -25300;

fn keychain_load() -> Result<Option<Vec<u8>>, ()> {
    match security_framework::passwords::get_generic_password(SERVICE, ACCOUNT) {
        Ok(key) => Ok(Some(key)),
        Err(e) if e.code() == ITEM_NOT_FOUND => Ok(None),
        Err(_) => Err(()),
    }
}

fn keychain_save(key: &[u8]) -> bool {
    security_framework::passwords::set_generic_password(SERVICE, ACCOUNT, key).is_ok()
}

/// The login Keychain, which opens with the account.
pub(super) const KEYSTORE: Keystore = Keystore { load: keychain_load, save: keychain_save };

/// Settings, the session and imported LUTs live here, under LiveCraft.
pub fn config_dir() -> PathBuf {
    home().join("Library").join("Application Support")
}

pub fn videos_dir() -> PathBuf {
    home().join("Movies")
}

pub fn pictures_dir() -> PathBuf {
    home().join("Pictures")
}

pub fn font_dirs() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/System/Library/Fonts"),
        PathBuf::from("/Library/Fonts"),
        home().join("Library/Fonts"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_avfoundation_device_list() {
        let log = "[AVFoundation indev @ 0x7f9] AVFoundation video devices:\n\
                   [AVFoundation indev @ 0x7f9] [0] FaceTime HD Camera\n\
                   [AVFoundation indev @ 0x7f9] [1] Capture screen 0\n\
                   [AVFoundation indev @ 0x7f9] AVFoundation audio devices:\n\
                   [AVFoundation indev @ 0x7f9] [0] MacBook Pro Microphone\n\
                   [AVFoundation indev @ 0x7f9] [1] Micro: USB\n\
                   [in#0 @ 0x600] Error opening input: Input/output error\n";
        let (video, audio) = parse_devices(log);
        assert_eq!(video, [(0, "FaceTime HD Camera".to_string()), (1, "Capture screen 0".to_string())]);
        assert_eq!(audio.len(), 2);
        assert_eq!(number_of(&audio, "Micro: USB"), "1", "a colon in a name is safe");
        assert_eq!(number_of(&audio, "Débranché"), "Débranché");
        assert!(is_screen(&video[1].1) && !is_screen(&video[0].1));
    }
}
