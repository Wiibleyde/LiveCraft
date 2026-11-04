//! Web page input: Edge or Chrome runs without a window, and streams what it
//! draws over its debugging connection. Only this computer can reach it.

use crate::api::base64_decode;
use crate::frame::Frame;
use crate::sources::InputRt;
use crate::titles;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket};

/// A browser runs as dozens of processes. A Windows job holds them together,
/// so closing the job ends them all, even if this program crashes.
struct ProcessTree(windows_sys::Win32::Foundation::HANDLE);

impl ProcessTree {
    fn hold(child: &std::process::Child) -> Option<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::*;
        // SAFETY: the job handle is checked, the information block is sized for
        // the call, and the child's handle is valid while child is borrowed.
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
            AssignProcessToJobObject(job, child.as_raw_handle() as _);
            Some(Self(job))
        }
    }
}

// SAFETY: a job handle is not tied to a thread and is only closed once, on drop.
unsafe impl Send for ProcessTree {}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        // SAFETY: this handle came from CreateJobObjectW and is closed once.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

fn find_browser() -> Option<PathBuf> {
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

fn free_port() -> Option<u16> {
    TcpListener::bind("127.0.0.1:0").ok()?.local_addr().ok().map(|a| a.port())
}

/// Reads one response by its length: this server keeps the connection open.
fn read_http_body(s: &mut TcpStream) -> Option<Vec<u8>> {
    let mut data = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = s.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&chunk[..n]);
        let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
        let head = String::from_utf8_lossy(&data[..end]).to_lowercase();
        let length: usize =
            head.lines().find_map(|l| l.strip_prefix("content-length:"))?.trim().parse().ok()?;
        if data.len() >= end + 4 + length {
            return Some(data[end + 4..end + 4 + length].to_vec());
        }
    }
}

fn http(port: u16, verb: &str, path: &str) -> Option<Vec<u8>> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(s, "{verb} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").ok()?;
    read_http_body(&mut s)
}

/// Opens a blank page in a window of its own, returning its id and control
/// address: a page in a background tab of a shared window draws nothing.
fn open_tab(port: u16, w: usize, h: usize) -> Option<(String, String)> {
    let body = http(port, "GET", "/json/version")?;
    let version: Value = serde_json::from_slice(&body).ok()?;
    let (mut ws, _) = tungstenite::connect(version["webSocketDebuggerUrl"].as_str()?).ok()?;
    let params = json!({"url": "about:blank", "newWindow": true, "width": w, "height": h});
    if !send(&mut ws, 1, "Target.createTarget", params) {
        return None;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let Ok(Message::Text(text)) = ws.read() else { return None };
        let Ok(v) = serde_json::from_str::<Value>(text.as_str()) else { continue };
        if v["id"] == 1 {
            let id = v["result"]["targetId"].as_str()?.to_string();
            let _ = ws.close(None);
            let address = format!("ws://127.0.0.1:{port}/devtools/page/{id}");
            return Some((id, address));
        }
    }
    None
}

/// One browser for every web input, each with its own page: a browser costs
/// hundreds of megabytes before showing anything, a page far less.
struct Host {
    child: Child,
    port: u16,
    users: usize,
    profile: PathBuf,
    _tree: Option<ProcessTree>,
}

static HOST: Mutex<Option<Host>> = Mutex::new(None);

fn launch(exe: &PathBuf) -> Option<Host> {
    let port = free_port()?;
    let profile = std::env::temp_dir().join(format!("Regie-web-{port}"));
    let mut cmd = Command::new(exe);
    cmd.args(["--headless=new", "--hide-scrollbars", "--mute-audio", "--no-first-run"])
        .args(["--no-default-browser-check", "--disable-sync", "--disable-extensions"])
        .arg("--autoplay-policy=no-user-gesture-required")
        // Pages behind the first one must keep drawing at full speed.
        .args(["--disable-background-timer-throttling", "--disable-renderer-backgrounding"])
        .arg("--disable-backgrounding-occluded-windows")
        .arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let child = cmd.spawn().ok()?;
    let tree = ProcessTree::hold(&child);
    Some(Host { child, port, users: 0, profile, _tree: tree })
}

/// Joins the running browser, starting it if needed. Returns its port.
fn join_host(exe: &PathBuf) -> Option<u16> {
    let mut slot = HOST.lock().unwrap();
    let dead = slot.as_mut().is_some_and(|h| !matches!(h.child.try_wait(), Ok(None)));
    if dead {
        *slot = None;
    }
    if slot.is_none() {
        *slot = launch(exe);
    }
    let host = slot.as_mut()?;
    host.users += 1;
    Some(host.port)
}

/// The last page out closes the browser.
fn leave_host(port: u16) {
    let mut slot = HOST.lock().unwrap();
    let Some(host) = slot.as_mut().filter(|h| h.port == port) else { return };
    host.users = host.users.saturating_sub(1);
    if host.users == 0
        && let Some(mut host) = slot.take()
    {
        crate::sources::end_process_tree(&mut host.child);
        clear_profile_later(host.profile.clone());
    }
}

type Socket = WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>;

fn send(ws: &mut Socket, id: u64, method: &str, params: Value) -> bool {
    let msg = json!({"id": id, "method": method, "params": params}).to_string();
    ws.send(Message::text(msg)).is_ok()
}

fn frame_from(data: &str, w: usize, h: usize) -> Option<Frame> {
    let bytes = base64_decode(data);
    let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let (iw, ih) = (img.width() as usize, img.height() as usize);
    let mut f = Frame { w: iw, h: ih, data: img.into_raw() };
    titles::premultiply(&mut f);
    if iw == w && ih == h {
        return Some(f);
    }
    let mut out = Frame::new(w, h);
    out.blit_region(&f, [0.0; 4], 0.0, 0.0, w as f32, h as f32, 255, 0.0);
    Some(out)
}

pub fn run_browser(rt: &Arc<InputRt>, url: &str, alpha: bool, w: usize, h: usize, fps: u32) {
    let Some(exe) = find_browser() else {
        rt.set_status("Edge ou Chrome est nécessaire pour les pages web");
        return;
    };
    clear_old_profiles();
    while !rt.stopped() {
        let Some(port) = join_host(&exe) else {
            rt.set_status("Impossible de lancer le navigateur");
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut tab = None;
        while tab.is_none() && Instant::now() < deadline && !rt.stopped() {
            tab = open_tab(port, w, h);
            if tab.is_none() {
                thread::sleep(Duration::from_millis(200));
            }
        }
        if let Some((id, address)) = tab {
            stream_page(rt, &address, url, alpha, (w, h), fps);
            let _ = http(port, "GET", &format!("/json/close/{id}"));
        }
        leave_host(port);
        if rt.stopped() {
            return;
        }
        if rt.status().is_none() {
            rt.set_status("Page web perdue, nouvel essai…");
        }
        thread::sleep(Duration::from_secs(2));
    }
}

/// Profiles left by a run that ended abruptly; a day old means nothing uses them.
fn clear_old_profiles() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        thread::spawn(|| {
            let Ok(list) = std::fs::read_dir(std::env::temp_dir()) else { return };
            let day = Duration::from_secs(86_400);
            for entry in list.flatten() {
                let ours = entry.file_name().to_string_lossy().starts_with("Regie-web-");
                let age = entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
                if ours && age.is_some_and(|a| a > day) {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        });
    });
}

/// The browser holds its files for a moment after being stopped.
fn clear_profile_later(profile: std::path::PathBuf) {
    thread::spawn(move || {
        for _ in 0..20 {
            thread::sleep(Duration::from_millis(500));
            if std::fs::remove_dir_all(&profile).is_ok() || !profile.exists() {
                return;
            }
        }
    });
}

/// Pictures are only acknowledged when the next one is wanted, and the browser
/// waits for that before sending more. So a page off air, which only feeds its
/// card, makes both the browser and this program do a fraction of the work.
fn next_picture_in(rt: &InputRt, fps: u32) -> Duration {
    if rt.showing.load(Ordering::Relaxed) {
        Duration::from_secs_f32(1.0 / fps.max(1) as f32)
    } else {
        Duration::from_millis(500)
    }
}

fn stream_page(rt: &InputRt, address: &str, url: &str, alpha: bool, size: (usize, usize), fps: u32) {
    let (w, h) = size;
    let mut ws = match tungstenite::connect(address) {
        Ok((ws, _)) => ws,
        Err(e) => {
            rt.set_status(format!("Connexion au navigateur impossible : {e}"));
            return;
        }
    };
    if let tungstenite::stream::MaybeTlsStream::Plain(s) = ws.get_mut() {
        let _ = s.set_read_timeout(Some(Duration::from_millis(15)));
    }
    let metrics = json!({"width": w, "height": h, "deviceScaleFactor": 1, "mobile": false});
    let mut ok = send(&mut ws, 1, "Page.enable", json!({}));
    ok &= send(&mut ws, 2, "Emulation.setDeviceMetricsOverride", metrics);
    ok &= send(&mut ws, 3, "Emulation.setFocusEmulationEnabled", json!({"enabled": true}));
    if alpha {
        let clear = json!({"color": {"r": 0, "g": 0, "b": 0, "a": 0}});
        ok &= send(&mut ws, 4, "Emulation.setDefaultBackgroundColorOverride", clear);
    }
    // Pictures with transparency need PNG, which is heavier than JPEG.
    let format = if alpha { "png" } else { "jpeg" };
    let cast = json!({"format": format, "quality": 85, "maxWidth": w, "maxHeight": h, "everyNthFrame": 1});
    ok &= send(&mut ws, 5, "Page.startScreencast", cast);
    ok &= send(&mut ws, 6, "Page.navigate", json!({"url": url}));
    if !ok {
        rt.set_status("Commande refusée par le navigateur");
        return;
    }
    // A page that stops changing sends nothing, so ask for a picture now and
    // then; the first request also covers pages that never move at all.
    let shot = json!({"format": format, "quality": 85});
    let mut last_picture = Instant::now() - Duration::from_secs(60);
    let mut got_one = false;
    let mut last_hash = 0u64;
    let mut id = 10;
    let mut waiting: Option<(String, Value)> = None;
    while !rt.stopped() {
        let patience = if got_one { 15 } else { 2 };
        if waiting.is_none() && last_picture.elapsed() > Duration::from_secs(patience) {
            last_picture = Instant::now();
            id += 1;
            send(&mut ws, id, "Page.captureScreenshot", shot.clone());
        }
        if waiting.is_some()
            && last_picture.elapsed() >= next_picture_in(rt, fps)
            && let Some((data, session)) = waiting.take()
        {
            last_picture = Instant::now();
            got_one |= show(rt, &data, (w, h), &mut last_hash);
            id += 1;
            send(&mut ws, id, "Page.screencastFrameAck", json!({"sessionId": session}));
        }
        match ws.read() {
            Ok(Message::Text(text)) => {
                let Ok(v) = serde_json::from_str::<Value>(text.as_str()) else { continue };
                if v["method"] == "Page.screencastFrame" {
                    if let Some(data) = v["params"]["data"].as_str() {
                        waiting = Some((data.to_string(), v["params"]["sessionId"].clone()));
                    }
                } else if let Some(data) = v["result"]["data"].as_str() {
                    last_picture = Instant::now();
                    got_one |= show(rt, data, (w, h), &mut last_hash);
                }
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => {
                rt.set_status(format!("Page web perdue : {e}"));
                return;
            }
        }
    }
}

/// Decodes and publishes a picture, unless it is the same one as before: the
/// rest of the chain then has nothing to redo either.
fn show(rt: &InputRt, data: &str, size: (usize, usize), last_hash: &mut u64) -> bool {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(data, &mut hash);
    let hash = std::hash::Hasher::finish(&hash);
    if hash == *last_hash {
        return true;
    }
    let Some(f) = frame_from(data, size.0, size.1) else { return false };
    *last_hash = hash;
    rt.clear_status();
    rt.publish(f);
    true
}
