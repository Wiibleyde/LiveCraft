//! Windows: DirectShow and GDI capture, job objects, DPAPI, named pipes.

use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE,
    LocalFree,
};
use windows_sys::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
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

/// The account this process runs as, as a SID string such as `S-1-5-21-…`.
fn process_user(process: HANDLE) -> Option<String> {
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::OpenProcessToken;
    // Room for the user and its SID, aligned for TOKEN_USER.
    let mut info = [0u64; 64];
    let mut size = 0;
    // SAFETY: every out pointer is valid for the call, the token is closed once,
    // and the SID read lives inside `info`, which outlives its use.
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let read = GetTokenInformation(
            token,
            TokenUser,
            info.as_mut_ptr().cast(),
            size_of_val(&info) as u32,
            &mut size,
        );
        CloseHandle(token);
        if read == 0 {
            return None;
        }
        let user = &*(info.as_ptr() as *const TOKEN_USER);
        let mut text = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
            return None;
        }
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        LocalFree(text.cast());
        Some(sid)
    }
}

fn this_user() -> Option<&'static str> {
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    static USER: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    // SAFETY: the pseudo handle of this process needs no closing.
    USER.get_or_init(|| process_user(unsafe { GetCurrentProcess() })).as_deref()
}

/// Every account on the computer sees the same pipe names: the name carries
/// this account and this sign-in session, so each person, and each of their
/// sessions, has its own.
fn pipe_path(name: &str) -> io::Result<Vec<u16>> {
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    let user = this_user().ok_or_else(|| io::Error::other("no account"))?;
    let mut session = 0;
    // SAFETY: `session` is valid for the call.
    unsafe {
        ProcessIdToSessionId(std::process::id(), &mut session);
    }
    Ok(format!(r"\\.\pipe\{name}-{user}-{session}").encode_utf16().chain([0]).collect())
}

/// Only this account may open the pipe: not other people, not services.
struct OnlyThisUser(PSECURITY_DESCRIPTOR);

impl OnlyThisUser {
    fn new() -> io::Result<Self> {
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        let user = this_user().ok_or_else(|| io::Error::other("no account"))?;
        // A protected list with one entry: full access for this account.
        let sddl: Vec<u16> = format!("D:P(A;;GA;;;{user})").encode_utf16().chain([0]).collect();
        let mut sd = std::ptr::null_mut();
        // SAFETY: the text is NUL-terminated; Windows allocates the descriptor, freed on drop.
        let made = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            )
        };
        if made == 0 { Err(io::Error::last_os_error()) } else { Ok(Self(sd)) }
    }
}

impl Drop for OnlyThisUser {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW, freed once.
        unsafe {
            LocalFree(self.0);
        }
    }
}

// SAFETY: the descriptor is only read, and freed once, on drop.
unsafe impl Send for OnlyThisUser {}

fn new_pipe(path: &[u16], access: &OnlyThisUser, first: bool) -> io::Result<File> {
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
    use windows_sys::Win32::System::Pipes::*;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: access.0,
        bInheritHandle: 0,
    };
    let open = if first { PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE } else { PIPE_ACCESS_DUPLEX };
    // Never reachable from another computer.
    let mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
    // SAFETY: the name is NUL-terminated; the attributes and descriptor outlive the call.
    let pipe = unsafe {
        CreateNamedPipeW(path.as_ptr(), open, mode, PIPE_UNLIMITED_INSTANCES, 4096, 4096, 0, &attributes)
    };
    if pipe == INVALID_HANDLE_VALUE {
        let e = io::Error::last_os_error();
        // The first instance exists already: another LiveCraft holds the name.
        return Err(if first && e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
            io::Error::new(io::ErrorKind::AddrInUse, e)
        } else {
            e
        });
    }
    // SAFETY: a fresh handle that nothing else owns.
    Ok(unsafe { File::from_raw_handle(pipe) })
}

/// Where the LiveCraft already running for this account listens for later
/// starts: a named pipe.
pub struct LocalListener {
    path: Vec<u16>,
    access: OnlyThisUser,
    waiting: File,
}

pub type LocalStream = File;

impl LocalListener {
    /// Fails with `AddrInUse` while another LiveCraft of this account holds the name.
    pub fn bind(name: &str) -> io::Result<Self> {
        let path = pipe_path(name)?;
        let access = OnlyThisUser::new()?;
        let waiting = new_pipe(&path, &access, true)?;
        Ok(Self { path, access, waiting })
    }

    pub fn accept(&mut self) -> io::Result<LocalStream> {
        use windows_sys::Win32::System::Pipes::ConnectNamedPipe;
        loop {
            // SAFETY: a pipe this listener created and still owns; no overlapped I/O.
            let joined = unsafe { ConnectNamedPipe(self.waiting.as_raw_handle(), std::ptr::null_mut()) } != 0;
            let caller =
                joined || io::Error::last_os_error().raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32);
            // The next one waits before this one is handed out, so the name is
            // never free while this program runs.
            let next = new_pipe(&self.path, &self.access, false)?;
            let current = std::mem::replace(&mut self.waiting, next);
            if caller {
                return Ok(current);
            }
            // That caller left before being answered.
        }
    }
}

/// Opens the pipe of the LiveCraft already running for this account:
/// `NotFound` when there is none.
pub fn connect_local(name: &str) -> io::Result<LocalStream> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;
    use windows_sys::Win32::System::Pipes::{GetNamedPipeServerProcessId, WaitNamedPipeW};
    let path = pipe_path(name)?;
    let text = String::from_utf16_lossy(&path[..path.len() - 1]);
    let mut tries = 0;
    let pipe = loop {
        // The other side may learn who calls, never act as them.
        let opened = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .security_qos_flags(SECURITY_IDENTIFICATION)
            .open(&text);
        match opened {
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) && tries < 20 => {
                tries += 1;
                // SAFETY: the name is NUL-terminated.
                unsafe {
                    WaitNamedPipeW(path.as_ptr(), 100);
                }
            }
            other => break other?,
        }
    };
    let mut server = 0;
    // SAFETY: an open pipe handle and a valid out pointer.
    unsafe {
        GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut server);
    }
    if !runs_as_this_user(server) {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    // Windows lets a program come to the front only with leave from the one
    // the person just started, which is this one.
    // SAFETY: a plain call with a process number.
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(server);
    }
    Ok(pipe)
}

/// Whether that process belongs to this account, so that a pipe set up under
/// our name by someone else never receives a project.
fn runs_as_this_user(pid: u32) -> bool {
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: the handle is checked, and closed once.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return false;
        }
        let user = process_user(process);
        CloseHandle(process);
        user.is_some() && user.as_deref() == this_user()
    }
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
