//! Windows: DirectShow and GDI capture, job objects, DPAPI.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
use windows_sys::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows_sys::Win32::System::JobObjects::*;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A console program started from a window program would flash a console.
pub fn prepare_child(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// Whether the child has ended.
pub fn has_exited(child: &mut Child) -> bool {
    !matches!(child.try_wait(), Ok(None))
}

/// A child can have children of its own: ffmpeg is often started through a
/// launcher, and a browser is dozens of processes. Stopping only the first
/// would leave the real work running.
pub fn end_process_tree(child: &mut Child) {
    let mut kill = Command::new("taskkill");
    kill.args(["/T", "/F", "/PID", &child.id().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    prepare_child(&mut kill);
    let _ = kill.status();
    let _ = child.kill();
}

/// A job that ends everything in it when its last handle closes, including
/// when this program crashes.
fn kill_on_close_job() -> Option<HANDLE> {
    // SAFETY: the job handle is checked, and the information block is sized
    // for the call.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of_val(&info) as u32,
        );
        Some(job)
    }
}

/// Everything started from now on ends with this program, however it stops,
/// so nothing is left running in the background after a crash.
pub fn tie_children_to_this_process() {
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // The handle stays open on purpose until this process ends.
        if let Some(job) = kill_on_close_job() {
            // SAFETY: both handles are valid; the current one needs no closing.
            unsafe {
                AssignProcessToJobObject(job, GetCurrentProcess());
            }
        }
    });
}

/// A browser runs as dozens of processes. A Windows job holds them together,
/// so closing the job ends them all, even if this program crashes.
pub struct ProcessTree(HANDLE);

impl ProcessTree {
    pub fn hold(child: &Child) -> Option<Self> {
        use std::os::windows::io::AsRawHandle;
        let job = kill_on_close_job()?;
        // SAFETY: the child's handle is valid while child is borrowed.
        unsafe {
            AssignProcessToJobObject(job, child.as_raw_handle() as _);
        }
        Some(Self(job))
    }
}

// SAFETY: a job handle is not tied to a thread and is only closed once, on drop.
unsafe impl Send for ProcessTree {}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        // SAFETY: this handle came from CreateJobObjectW and is closed once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

pub fn find_browser() -> Option<PathBuf> {
    let roots = ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"];
    let tails = ["Microsoft/Edge/Application/msedge.exe", "Google/Chrome/Application/chrome.exe"];
    for tail in tails {
        for root in roots {
            if let Ok(base) = std::env::var(root) {
                let p = PathBuf::from(base).join(tail);
                if p.exists() {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// Display rectangles, primary first: left, top, width, height.
pub fn list_screens() -> Vec<(i32, i32, i32, i32)> {
    use windows_sys::Win32::Foundation::{LPARAM, RECT};
    use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};
    unsafe extern "system" fn add(_: HMONITOR, _: HDC, r: *mut RECT, data: LPARAM) -> i32 {
        // SAFETY: Windows hands back the list pointer given below, and a valid rectangle.
        unsafe {
            let list = &mut *(data as *mut Vec<(i32, i32, i32, i32)>);
            let r = &*r;
            list.push((r.left, r.top, r.right - r.left, r.bottom - r.top));
        }
        1
    }
    let mut list: Vec<(i32, i32, i32, i32)> = Vec::new();
    // SAFETY: the callback only runs during this call, while `list` is alive.
    unsafe {
        EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(add), &mut list as *mut _ as LPARAM);
    }
    // The primary display sits at the origin; the rest keep reading order.
    list.sort_by_key(|&(x, y, _, _)| ((x, y) != (0, 0), y, x));
    list
}

/// Capture devices ffmpeg can open: (cameras, microphones).
pub fn list_devices() -> (Vec<String>, Vec<String>) {
    let Ok(out) = crate::sources::ffmpeg()
        .args(["-list_devices", "true", "-f", "dshow", "-i", "dummy"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
    else {
        return (Vec::new(), Vec::new());
    };
    parse_devices(&String::from_utf8_lossy(&out.stderr))
}

fn parse_devices(log: &str) -> (Vec<String>, Vec<String>) {
    let (mut video, mut audio) = (Vec::new(), Vec::new());
    for line in log.lines() {
        let mut parts = line.split('"');
        let (Some(_), Some(name), Some(tail)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if tail.contains("(video)") {
            video.push(name.to_string());
        } else if tail.contains("(audio)") {
            audio.push(name.to_string());
        }
    }
    (video, audio)
}

/// ffmpeg arguments opening a camera by the name `list_devices` gave.
pub fn camera_input(device: &str) -> Vec<String> {
    let input = format!("video={device}");
    ["-f", "dshow", "-rtbufsize", "128M", "-i", &input].map(String::from).to_vec()
}

/// ffmpeg arguments capturing every screen at once.
pub fn screen_input(fps: u32) -> Result<Vec<String>, String> {
    Ok(["-f", "gdigrab", "-framerate", &fps.to_string(), "-i", "desktop"].map(String::from).to_vec())
}

pub fn microphone_input(device: &str) -> Vec<String> {
    let input = format!("audio={device}");
    ["-f", "dshow", "-audio_buffer_size", "40", "-i", &input].map(String::from).to_vec()
}

/// None: the sound card's own loopback, opened through cpal, is better here.
pub fn system_audio_input() -> Option<Vec<String>> {
    None
}

fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 }
}

/// Copies out what Windows allocated, then gives it back.
fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    // SAFETY: on success Windows filled `out` with a buffer of that length,
    // which is read once here and released with the matching call.
    unsafe {
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData as *mut _);
        bytes
    }
}

/// Sealed with the Windows account: another account, or a copy of the file
/// on another computer, cannot open it.
pub fn seal(plain: &[u8]) -> Option<Vec<u8>> {
    let input = blob(plain);
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    // SAFETY: `input` points at `plain`, alive for the call; the optional
    // arguments are allowed to be null.
    let done = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    (done != 0).then(|| take(out))
}

pub fn open_sealed(sealed: &[u8]) -> Option<Vec<u8>> {
    let input = blob(sealed);
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    // SAFETY: `input` points at `sealed`, alive for the call; the optional
    // arguments are allowed to be null.
    let done = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    (done != 0).then(|| take(out))
}

fn env_dir(name: &str) -> PathBuf {
    PathBuf::from(std::env::var(name).unwrap_or_else(|_| ".".into()))
}

/// Settings, the session and imported LUTs live here, under LiveCraft.
pub fn config_dir() -> PathBuf {
    env_dir("APPDATA")
}

pub fn videos_dir() -> PathBuf {
    env_dir("USERPROFILE").join("Videos")
}

pub fn pictures_dir() -> PathBuf {
    env_dir("USERPROFILE").join("Pictures")
}

pub fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(w) = std::env::var("WINDIR") {
        dirs.push(PathBuf::from(w).join("Fonts"));
    }
    if let Ok(l) = std::env::var("LOCALAPPDATA") {
        dirs.push(PathBuf::from(l).join("Microsoft").join("Windows").join("Fonts"));
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_device_list() {
        let log = "[dshow @ 01] \"HD Webcam\" (video)\n\
                   [dshow @ 01]   Alternative name \"@device_pnp_x\"\n\
                   [dshow @ 01] \"Micro (USB)\" (audio)\n";
        let (v, a) = parse_devices(log);
        assert_eq!(v, ["HD Webcam"]);
        assert_eq!(a, ["Micro (USB)"]);
    }
}
