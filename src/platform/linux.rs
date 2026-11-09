//! Linux: Video4Linux cameras, PulseAudio (or PipeWire's stand-in) and ALSA
//! sound, X11 screens, and the Secret Service keyring.

use super::unix::{Keystore, find_on_path, home, is_program};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// One device as ffmpeg's `-sources` names it.
#[derive(Debug, PartialEq)]
struct Source {
    format: &'static str,
    id: String,
    label: String,
}

fn sources(format: &'static str) -> Vec<Source> {
    let listed = crate::sources::ffmpeg()
        .args(["-sources", format])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    match listed {
        Ok(out) if out.status.success() => parse_sources(format, &String::from_utf8_lossy(&out.stdout)),
        _ => Vec::new(),
    }
}

/// Lines read `* id [label] (kinds)`, the star marking the default.
fn parse_sources(format: &'static str, text: &str) -> Vec<Source> {
    let mut list: Vec<Source> = text
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("* ").or_else(|| line.strip_prefix("  "))?;
            let (id, tail) = rest.split_once(" [")?;
            let label = tail.rsplit_once(']')?.0.trim();
            let label = if label.is_empty() { id } else { label };
            Some(Source { format, id: id.to_string(), label: label.to_string() })
        })
        // ALSA's "null" device only ever gives silence.
        .filter(|s| !(format == "alsa" && s.id == "null"))
        .collect();
    // Two identical webcams share a name; the device tells them apart.
    let twins: Vec<bool> =
        list.iter().map(|s| list.iter().filter(|o| o.label == s.label).count() > 1).collect();
    for (s, twin) in list.iter_mut().zip(twins) {
        if twin {
            s.label = format!("{} ({})", s.label, s.id);
        }
    }
    list
}

/// Sound comes through PulseAudio, which PipeWire also answers to, and
/// straight from ALSA on systems without either.
fn audio_sources() -> Vec<Source> {
    let pulse = sources("pulse");
    if pulse.is_empty() { sources("alsa") } else { pulse }
}

/// Inputs are saved by the name people see; a device or ALSA name typed by
/// hand works too.
fn resolve(list: Vec<Source>, device: &str) -> Option<Source> {
    let mut by_id = None;
    for s in list {
        if s.label == device {
            return Some(s);
        }
        if s.id == device {
            by_id = Some(s);
        }
    }
    by_id
}

pub fn list_devices() -> (Vec<String>, Vec<String>) {
    let labels = |list: Vec<Source>| list.into_iter().map(|s| s.label).collect();
    (labels(sources("v4l2")), labels(audio_sources()))
}

pub fn camera_input(device: &str) -> Vec<String> {
    let path = resolve(sources("v4l2"), device).map_or_else(|| device.to_string(), |s| s.id);
    vec!["-f".into(), "v4l2".into(), "-i".into(), path]
}

pub fn microphone_input(device: &str) -> Vec<String> {
    let (format, id) = match resolve(audio_sources(), device) {
        Some(s) => (s.format, s.id),
        None => (guess_audio_format(device), device.to_string()),
    };
    vec!["-f".into(), format.into(), "-i".into(), id]
}

fn guess_audio_format(device: &str) -> &'static str {
    let alsa = ["hw:", "plughw:", "sysdefault", "dsnoop", "front:"];
    if alsa.iter().any(|p| device.starts_with(p)) { "alsa" } else { "pulse" }
}

/// What the default output plays, from its monitor.
pub fn system_audio_input() -> Option<Vec<String>> {
    Some(["-f", "pulse", "-i", "@DEFAULT_MONITOR@"].map(String::from).to_vec())
}

/// Every screen of the X display at once. Under Wayland this goes through
/// XWayland, which only sees windows of X programs.
pub fn screen_input(fps: u32) -> Result<Vec<String>, String> {
    let display = std::env::var("DISPLAY")
        .ok()
        .filter(|d| !d.is_empty())
        .ok_or("La capture d'écran demande une session X11 ou XWayland")?;
    Ok(["-f", "x11grab", "-framerate", &fps.to_string(), "-draw_mouse", "1", "-i", &display]
        .map(String::from)
        .to_vec())
}

/// Display rectangles, primary first: left, top, width, height.
pub fn list_screens() -> Vec<(i32, i32, i32, i32)> {
    use x11rb::connection::Connection;
    use x11rb::protocol::randr::ConnectionExt;
    let Ok((conn, number)) = x11rb::connect(None) else { return Vec::new() };
    let Some(screen) = conn.setup().roots.get(number) else { return Vec::new() };
    let monitors = conn.randr_get_monitors(screen.root, true).ok().and_then(|c| c.reply().ok());
    let Some(reply) = monitors.filter(|r| !r.monitors.is_empty()) else {
        return vec![(0, 0, screen.width_in_pixels as i32, screen.height_in_pixels as i32)];
    };
    let mut list: Vec<_> = reply
        .monitors
        .iter()
        .map(|m| (m.primary, (m.x as i32, m.y as i32, m.width as i32, m.height as i32)))
        .collect();
    list.sort_by_key(|&(primary, (x, y, _, _))| (!primary, y, x));
    list.into_iter().map(|(_, r)| r).collect()
}

pub fn find_browser() -> Option<PathBuf> {
    let names = [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "microsoft-edge",
        "microsoft-edge-stable",
    ];
    let fixed = ["/opt/google/chrome/chrome", "/opt/microsoft/msedge/msedge", "/snap/bin/chromium"];
    find_on_path(&names).or_else(|| fixed.iter().map(PathBuf::from).find(|p| is_program(p)))
}

const KEY_ATTRIBUTES: [(&str, &str); 2] = [("application", "livecraft"), ("purpose", "session-key")];

fn keyring_load() -> Result<Option<Vec<u8>>, ()> {
    use secret_service::{EncryptionType, blocking::SecretService};
    let service = SecretService::connect(EncryptionType::Dh).map_err(drop)?;
    let collection = service.get_default_collection().map_err(drop)?;
    // A locked keyring asks for its password, as for any program using it.
    collection.ensure_unlocked().map_err(drop)?;
    let items = collection.search_items(HashMap::from(KEY_ATTRIBUTES)).map_err(drop)?;
    match items.first() {
        Some(item) => item.get_secret().map(Some).map_err(drop),
        None => Ok(None),
    }
}

fn keyring_save(key: &[u8]) -> bool {
    use secret_service::{EncryptionType, blocking::SecretService};
    let Ok(service) = SecretService::connect(EncryptionType::Dh) else { return false };
    let Ok(collection) = service.get_default_collection() else { return false };
    let attributes = HashMap::from(KEY_ATTRIBUTES);
    collection.create_item("LiveCraft", attributes, key, true, "application/octet-stream").is_ok()
}

/// The desktop's keyring (GNOME Keyring, KWallet, KeePassXC…) through the
/// Secret Service.
pub(super) const KEYSTORE: Keystore = Keystore { load: keyring_load, save: keyring_save };

fn xdg_dir(variable: &str, fallback: &str) -> PathBuf {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(fallback))
}

/// Settings, the session and imported LUTs live here, under LiveCraft.
pub fn config_dir() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config")
}

/// A folder from `user-dirs.dirs`, which names it in the user's language.
fn user_dir(key: &str, fallback: &str) -> PathBuf {
    let home = home();
    let listed = std::fs::read_to_string(config_dir().join("user-dirs.dirs")).ok();
    listed.and_then(|text| parse_user_dir(&text, key, &home)).unwrap_or_else(|| home.join(fallback))
}

fn parse_user_dir(text: &str, key: &str, home: &Path) -> Option<PathBuf> {
    let value = text.lines().find_map(|l| l.trim().strip_prefix(key)?.strip_prefix('='))?;
    let value = value.trim().trim_matches('"');
    let path = match value.strip_prefix("$HOME") {
        Some(rest) => home.join(rest.trim_start_matches('/')),
        None => PathBuf::from(value),
    };
    // Set to the home folder itself means the folder is turned off.
    (path.is_absolute() && path != home).then_some(path)
}

pub fn videos_dir() -> PathBuf {
    user_dir("XDG_VIDEOS_DIR", "Videos")
}

pub fn pictures_dir() -> PathBuf {
    user_dir("XDG_PICTURES_DIR", "Pictures")
}

pub fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![xdg_dir("XDG_DATA_HOME", ".local/share").join("fonts"), home().join(".fonts")];
    let shared = std::env::var("XDG_DATA_DIRS").ok().filter(|d| !d.is_empty());
    let shared = shared.unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    dirs.extend(shared.split(':').filter(|d| !d.is_empty()).map(|d| Path::new(d).join("fonts")));
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ffmpeg_source_lists() {
        let cams = "Auto-detected sources for video4linux2,v4l2:\n  \
                    /dev/video0 [Integrated Camera: Integrated C] (video)\n  \
                    /dev/video2 [USB Camera] (video)\n  \
                    /dev/video4 [USB Camera]\n";
        let list = parse_sources("v4l2", cams);
        let labels: Vec<_> = list.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(
            labels,
            ["Integrated Camera: Integrated C", "USB Camera (/dev/video2)", "USB Camera (/dev/video4)"]
        );
        assert_eq!(list[0].id, "/dev/video0");

        let mics = "Auto-detected sources for pulse:\n  \
                    RDPSink.monitor [Monitor of RDP Sink] (none)\n\
                    * RDPSource [RDP Source] (none)\n";
        let list = parse_sources("pulse", mics);
        assert_eq!(list[1], Source { format: "pulse", id: "RDPSource".into(), label: "RDP Source".into() });
        assert_eq!(list.len(), 2);

        let alsa = "Auto-detected sources for alsa:\n  null [Discard all samples] (none)\n\
                    * default [Default ALSA Output] (none)\n";
        let ids: Vec<_> = parse_sources("alsa", alsa).into_iter().map(|s| s.id).collect();
        assert_eq!(ids, ["default"], "the silent device is left out");
    }

    #[test]
    fn devices_open_by_name_or_by_device() {
        let list = || parse_sources("v4l2", "  /dev/video0 [Cam] (video)\n  /dev/video2 [Other] (video)\n");
        assert_eq!(resolve(list(), "Other").unwrap().id, "/dev/video2");
        assert_eq!(resolve(list(), "/dev/video0").unwrap().label, "Cam");
        assert!(resolve(list(), "Gone").is_none());
        assert_eq!(guess_audio_format("hw:CARD=PCH,DEV=0"), "alsa");
        assert_eq!(guess_audio_format("alsa_input.usb-mic"), "pulse");
    }

    #[test]
    fn user_folders_follow_their_translated_names() {
        let home = Path::new("/home/ana");
        let text = "# written by xdg-user-dirs-update\nXDG_VIDEOS_DIR=\"$HOME/Vidéos\"\n\
                    XDG_PICTURES_DIR=\"$HOME/\"\nXDG_MUSIC_DIR=\"/srv/musique\"\n";
        assert_eq!(parse_user_dir(text, "XDG_VIDEOS_DIR", home), Some(home.join("Vidéos")));
        assert_eq!(parse_user_dir(text, "XDG_PICTURES_DIR", home), None, "turned off");
        assert_eq!(parse_user_dir(text, "XDG_MUSIC_DIR", home), Some(PathBuf::from("/srv/musique")));
        assert_eq!(parse_user_dir(text, "XDG_DESKTOP_DIR", home), None);
    }
}
