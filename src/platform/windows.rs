//! Windows: DirectShow and GDI capture, job objects, DPAPI, named pipes.

use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
    ERROR_PIPE_NOT_CONNECTED, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES};
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

/// The text of a SID, such as `S-1-5-21-…`.
///
/// # Safety
/// `sid` must point at a valid SID.
unsafe fn sid_text(sid: PSID) -> Option<String> {
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    let mut text = std::ptr::null_mut();
    // SAFETY: the caller vouches for the SID; Windows allocates the text, read once and freed once.
    unsafe {
        if ConvertSidToStringSidW(sid, &mut text) == 0 {
            return None;
        }
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        LocalFree(text.cast());
        Some(sid)
    }
}

/// The last part of a SID: for an integrity label, its level (0x2000 for an
/// ordinary program, 0x3000 for one run as administrator).
///
/// # Safety
/// `sid` must point at a valid SID.
unsafe fn sid_level(sid: PSID) -> u32 {
    use windows_sys::Win32::Security::{GetSidSubAuthority, GetSidSubAuthorityCount};
    // SAFETY: the caller vouches for the SID; a label SID has one sub-authority.
    unsafe { *GetSidSubAuthority(sid, u32::from(*GetSidSubAuthorityCount(sid)).saturating_sub(1)) }
}

/// Who this program runs as: its account, and its integrity level.
struct Me {
    user: String,
    level: u32,
}

const MEDIUM_LEVEL: u32 = 0x2000;

fn me() -> Option<&'static Me> {
    use windows_sys::Win32::Security::{
        GetTokenInformation, SID_AND_ATTRIBUTES, TOKEN_QUERY, TokenIntegrityLevel, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    static ME: std::sync::OnceLock<Option<Me>> = std::sync::OnceLock::new();
    ME.get_or_init(|| {
        // SAFETY: every out pointer is valid for its call, the token is closed
        // once, and each SID read lives inside `info`, which outlives its use.
        unsafe {
            let mut token = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return None;
            }
            // TOKEN_USER and TOKEN_MANDATORY_LABEL both start with the SID asked for.
            let sid_of = |class| {
                let mut info = [0u64; 64];
                let mut size = 0;
                let read = GetTokenInformation(
                    token,
                    class,
                    info.as_mut_ptr().cast(),
                    size_of_val(&info) as u32,
                    &mut size,
                );
                (read != 0).then_some(info)
            };
            let user =
                sid_of(TokenUser).and_then(|i| sid_text((*(i.as_ptr() as *const SID_AND_ATTRIBUTES)).Sid));
            let level = sid_of(TokenIntegrityLevel)
                .map(|i| sid_level((*(i.as_ptr() as *const SID_AND_ATTRIBUTES)).Sid));
            CloseHandle(token);
            Some(Me { user: user?, level: level.unwrap_or(MEDIUM_LEVEL) })
        }
    })
    .as_ref()
}

fn no_account() -> io::Error {
    io::Error::other("this account could not be read")
}

/// Every account on the computer sees the same pipe names: the name carries
/// this account and this sign-in session, so each person, and each of their
/// sessions, has its own.
fn pipe_path(name: &str) -> io::Result<Vec<u16>> {
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    let me = me().ok_or_else(no_account)?;
    let mut session = 0;
    // SAFETY: `session` is valid for the call.
    if unsafe { ProcessIdToSessionId(std::process::id(), &mut session) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(format!(r"\\.\pipe\{name}-{}-{session}", me.user).encode_utf16().chain([0]).collect())
}

/// Who may open a pipe, as a security descriptor Windows allocated.
struct Access(PSECURITY_DESCRIPTOR);

impl Access {
    /// Owned by this account, open to it alone, and labelled with this
    /// program's integrity level: a program of the same account running with
    /// fewer rights, a sandboxed one say, can neither read nor write it, and
    /// a later start can tell a pipe made by such a program from ours.
    fn only_me() -> io::Result<Self> {
        let me = me().ok_or_else(no_account)?;
        Self::from_sddl(&format!("O:{0}D:P(A;;GA;;;{0})S:(ML;;NWNR;;;S-1-16-{1})", me.user, me.level))
    }

    fn from_sddl(sddl: &str) -> io::Result<Self> {
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        let sddl: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
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

impl Drop for Access {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW, freed once.
        unsafe {
            LocalFree(self.0);
        }
    }
}

// SAFETY: the descriptor is only read, and freed once, on drop.
unsafe impl Send for Access {}

/// Pipe instances at once: the one waiting for a caller, and those being
/// answered. More callers wait their turn.
const PIPE_INSTANCES: u32 = 10;

fn new_pipe(path: &[u16], access: &Access, first: bool) -> io::Result<File> {
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
    let pipe =
        unsafe { CreateNamedPipeW(path.as_ptr(), open, mode, PIPE_INSTANCES, 4096, 4096, 0, &attributes) };
    if pipe == INVALID_HANDLE_VALUE {
        let e = io::Error::last_os_error();
        // The first instance exists already: something holds the name.
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
    access: Access,
    waiting: File,
}

pub type LocalStream = File;

impl LocalListener {
    /// Fails with `AddrInUse` while something holds the name.
    pub fn bind(name: &str) -> io::Result<Self> {
        let path = pipe_path(name)?;
        let access = Access::only_me()?;
        let waiting = new_pipe(&path, &access, true)?;
        Ok(Self { path, access, waiting })
    }

    /// The next caller. An error when one left before being answered, or no
    /// instance is free: the caller tries again.
    pub fn accept(&mut self) -> io::Result<LocalStream> {
        use windows_sys::Win32::System::Pipes::ConnectNamedPipe;
        // SAFETY: a pipe this listener created and still owns; no overlapped I/O.
        let joined = unsafe { ConnectNamedPipe(self.waiting.as_raw_handle(), std::ptr::null_mut()) } != 0;
        let caller = joined || io::Error::last_os_error().raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32);
        // The next one waits before this one is handed out, so the name is
        // never free while this program runs.
        let next = new_pipe(&self.path, &self.access, false)?;
        let current = std::mem::replace(&mut self.waiting, next);
        if caller { Ok(current) } else { Err(io::ErrorKind::ConnectionAborted.into()) }
    }
}

/// Opens the pipe of the LiveCraft already running for this account:
/// `NotFound` when there is none, `PermissionDenied` when what holds the name
/// is not ours or cannot be checked.
pub fn connect_local(name: &str) -> io::Result<LocalStream> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;
    use windows_sys::Win32::System::Pipes::{GetNamedPipeServerProcessId, WaitNamedPipeW};
    use windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;
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
            // Every instance stays taken: whoever holds them cannot be checked.
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, e));
            }
            other => break other?,
        }
    };
    if !made_by_me(&pipe) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "not this account's LiveCraft"));
    }
    let mut server = 0;
    // Windows lets a program come to the front only with leave from the one
    // the person just started: this one, or the wrapper that started it.
    // SAFETY: an open pipe handle and a valid out pointer; then a plain call.
    unsafe {
        if GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut server) != 0 {
            AllowSetForegroundWindow(server);
        }
    }
    Ok(pipe)
}

/// Whether the pipe is owned by this account and labelled at this program's
/// integrity level or above. The pipe itself says so, which no process
/// number can fake: another account cannot make this account the owner, and
/// a program cannot label anything above its own level.
fn made_by_me(pipe: &File) -> bool {
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
    use windows_sys::Win32::Security::{
        ACE_HEADER, ACL, GetAce, LABEL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
    };
    const MANDATORY_LABEL: u8 = 0x11;
    let Some(me) = me() else { return false };
    let (mut owner, mut sacl, mut sd) =
        (std::ptr::null_mut(), std::ptr::null_mut::<ACL>(), std::ptr::null_mut());
    // SAFETY: an open pipe handle and valid out pointers; Windows allocates the
    // descriptor that owner and label point into, freed once below.
    unsafe {
        let asked = GetSecurityInfo(
            pipe.as_raw_handle(),
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sacl,
            &mut sd,
        );
        if asked != 0 {
            return false;
        }
        let ours = !owner.is_null() && sid_text(owner).as_deref() == Some(me.user.as_str());
        // Without a label, Windows counts an object as medium.
        let mut level = MEDIUM_LEVEL;
        if !sacl.is_null() {
            for i in 0..u32::from((*sacl).AceCount) {
                let mut ace = std::ptr::null_mut();
                if GetAce(sacl, i, &mut ace) != 0 && (*(ace as *const ACE_HEADER)).AceType == MANDATORY_LABEL
                {
                    let label = ace as *const windows_sys::Win32::Security::SYSTEM_MANDATORY_LABEL_ACE;
                    level = sid_level(std::ptr::addr_of!((*label).SidStart) as PSID);
                }
            }
        }
        LocalFree(sd);
        ours && level >= me.level
    }
}

/// Reads what has come, waiting until `until` at most: `TimedOut` then, and
/// 0 once the other side hung up. A plain read on a pipe cannot time out.
pub fn read_some(stream: &mut LocalStream, buf: &mut [u8], until: std::time::Instant) -> io::Result<usize> {
    use std::io::Read;
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;
    loop {
        let mut ready = 0;
        // SAFETY: an open pipe handle; only the count of waiting bytes is asked for.
        let peeked = unsafe {
            PeekNamedPipe(
                stream.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut ready,
                std::ptr::null_mut(),
            )
        };
        if peeked == 0 {
            let e = io::Error::last_os_error();
            let gone = [ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED]
                .map(|c| Some(c as i32))
                .contains(&e.raw_os_error());
            return if gone { Ok(0) } else { Err(e) };
        }
        if ready > 0 {
            let n = (ready as usize).min(buf.len());
            return stream.read(&mut buf[..n]);
        }
        if std::time::Instant::now() >= until {
            return Err(io::ErrorKind::TimedOut.into());
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Explorer lets the program it started come to the front: the portable
/// build passes that leave on before starting the faster one, so that a later
/// start can bring the window already open forward. False when it had none.
pub fn pass_on_foreground_leave() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};
    // SAFETY: a plain call.
    unsafe { AllowSetForegroundWindow(ASFW_ANY) != 0 }
}

/// Sets up a pipe under that name as a program of this account with fewer
/// rights could: tests check that it is not taken for a LiveCraft.
#[cfg(test)]
pub fn bind_as_lesser(name: &str) -> io::Result<File> {
    let me = me().ok_or_else(no_account)?;
    let access = Access::from_sddl(&format!("O:{0}D:P(A;;GA;;;{0})S:(ML;;NWNR;;;LW)", me.user))?;
    new_pipe(&pipe_path(name)?, &access, true)
}

/// Opens a web page in the person's own browser.
pub fn open_in_browser(url: &str) {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    // Handed anything else, the shell would start it.
    if !url.starts_with("https://") {
        return;
    }
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let (verb, page) = (wide("open"), wide(url));
    // SAFETY: both strings are NUL-terminated and outlive the call.
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            page.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
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
