//! Web page input: Chrome, Chromium or Edge runs without a window, and streams what it
//! draws over its debugging connection. Only this computer can reach it.

use crate::api::base64_decode;
use crate::frame::Frame;
pub(crate) use crate::platform::{ProcessTree, find_browser};
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

pub(crate) fn free_port() -> Option<u16> {
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

pub(crate) fn http(port: u16, verb: &str, path: &str) -> Option<Vec<u8>> {
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
    tree: Option<ProcessTree>,
}

static HOST: Mutex<Option<Host>> = Mutex::new(None);

fn launch(exe: &PathBuf) -> Option<Host> {
    let port = free_port()?;
    let profile = private_profile("LiveCraft-web", port)?;
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
    crate::platform::prepare_child(&mut cmd);
    let child = cmd.spawn().ok()?;
    let tree = ProcessTree::hold(&child);
    Some(Host { child, port, users: 0, profile, tree })
}

/// Joins the running browser, starting it if needed. Returns its port.
fn join_host(exe: &PathBuf) -> Option<u16> {
    let mut slot = HOST.lock().unwrap();
    let dead = slot.as_mut().is_some_and(|h| crate::platform::has_exited(&mut h.child));
    if dead && let Some(mut host) = slot.take() {
        // Ended, but not reaped yet: its number is still its own, so what it
        // left behind can be ended and the tree let go before the number is freed.
        crate::platform::end_process_tree(&mut host.child);
        clear_profile_later(host.profile.clone());
        drop(host.tree.take());
        let _ = host.child.wait();
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
        crate::platform::end_process_tree(&mut host.child);
        clear_profile_later(host.profile.clone());
        // The tree lets go before the browser is reaped, so the number it
        // holds can never belong to another program by then.
        drop(host.tree.take());
        thread::spawn(move || {
            let _ = host.child.wait();
        });
    }
}

pub(crate) type Socket = WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>;

pub(crate) fn send(ws: &mut Socket, id: u64, method: &str, params: Value) -> bool {
    let msg = json!({"id": id, "method": method, "params": params}).to_string();
    ws.send(Message::text(msg)).is_ok()
}

/// What a page needs beyond showing its pictures: the video call talks to its
/// page while it runs. Command numbers come from `next_id`, so they never
/// collide with the ones the picture loop uses.
pub(crate) trait PageHook {
    /// Before the page loads, so nothing it says early is missed.
    fn prepare(&mut self, _ws: &mut Socket, _next_id: &mut u64) -> bool {
        true
    }
    /// Each turn of the loop, at least every few milliseconds.
    fn turn(&mut self, _ws: &mut Socket, _next_id: &mut u64) {}
    /// Each message from the browser that is not a picture.
    fn message(&mut self, _v: &Value) {}
}

struct PlainPage;

impl PageHook for PlainPage {}

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
    run_page(rt, url, alpha, (w, h), fps, &mut PlainPage);
}

pub(crate) fn run_page(
    rt: &Arc<InputRt>,
    url: &str,
    alpha: bool,
    (w, h): (usize, usize),
    fps: u32,
    hook: &mut dyn PageHook,
) {
    let Some(exe) = find_browser() else {
        rt.set_status(t!("browser.needs_edge_or_chrome"));
        return;
    };
    clear_old_profiles();
    while !rt.stopped() {
        let Some(port) = join_host(&exe) else {
            rt.set_status(t!("browser.cannot_launch"));
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
            stream_page(rt, &address, url, alpha, (w, h), fps, hook);
            let _ = http(port, "GET", &format!("/json/close/{id}"));
        }
        leave_host(port);
        if rt.stopped() {
            return;
        }
        if rt.status().is_none() {
            rt.set_status(t!("browser.page_lost_retrying"));
        }
        thread::sleep(Duration::from_secs(2));
    }
}

/// A browser's profile folder, made new and private: in the account's own
/// runtime folder where there is one (not the shared temporary folder), under
/// a name nobody can predict, and refused if it already exists.
pub(crate) fn private_profile(prefix: &str, port: u16) -> Option<PathBuf> {
    let base = profile_base();
    for _ in 0..8 {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).ok()?;
        let path = base.join(format!("{prefix}-{port}-{:016x}", u64::from_le_bytes(random)));
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut make = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut make, 0o700);
        if make.create(&path).is_ok() {
            return Some(path);
        }
    }
    None
}

fn profile_base() -> PathBuf {
    #[cfg(unix)]
    if let Some(dir) =
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|d| d.is_absolute() && d.is_dir())
    {
        return dir;
    }
    std::env::temp_dir()
}

/// Profiles left by a run that ended abruptly; a day old means nothing uses them.
fn clear_old_profiles() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        thread::spawn(|| {
            let day = Duration::from_secs(86_400);
            let mut folders = vec![std::env::temp_dir()];
            if profile_base() != folders[0] {
                folders.push(profile_base());
            }
            let list = folders.into_iter().filter_map(|d| std::fs::read_dir(d).ok()).flatten();
            for entry in list.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                // The program was called Regie before, and left the same kind.
                let ours = name.starts_with("LiveCraft-web-") || name.starts_with("Regie-web-");
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

fn stream_page(
    rt: &InputRt,
    address: &str,
    url: &str,
    alpha: bool,
    size: (usize, usize),
    fps: u32,
    hook: &mut dyn PageHook,
) {
    let (w, h) = size;
    let mut ws = match tungstenite::connect(address) {
        Ok((ws, _)) => ws,
        Err(e) => {
            rt.set_status(t!("browser.cannot_connect", error = e));
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
    let mut asked = vec![1, 2, 3];
    if alpha {
        let clear = json!({"color": {"r": 0, "g": 0, "b": 0, "a": 0}});
        ok &= send(&mut ws, 4, "Emulation.setDefaultBackgroundColorOverride", clear);
        asked.push(4);
    }
    let mut id = 10;
    ok &= hook.prepare(&mut ws, &mut id);
    asked.extend(11..=id);
    if !ok {
        rt.set_status(t!("browser.command_refused"));
        return;
    }
    if !answered(&mut ws, rt, asked, hook) {
        return;
    }
    // Pictures with transparency need PNG, which is heavier than JPEG.
    let format = if alpha { "png" } else { "jpeg" };
    let cast = json!({"format": format, "quality": 85, "maxWidth": w, "maxHeight": h, "everyNthFrame": 1});
    let mut ok = send(&mut ws, 5, "Page.startScreencast", cast);
    ok &= send(&mut ws, 6, "Page.navigate", json!({"url": url}));
    if !ok {
        rt.set_status(t!("browser.command_refused"));
        return;
    }
    // A page that stops changing sends nothing, so ask for a picture now and
    // then; the first request also covers pages that never move at all.
    let shot = json!({"format": format, "quality": 85});
    let mut last_picture = Instant::now() - Duration::from_secs(60);
    let mut got_one = false;
    let mut last_hash = 0u64;
    let mut waiting: Option<(String, Value)> = None;
    while !rt.stopped() {
        hook.turn(&mut ws, &mut id);
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
                } else if crashed(rt, &v) {
                    return;
                } else {
                    hook.message(&v);
                }
            }
            Ok(_) => {}
            Err(e) if timed_out(&e) => {}
            Err(e) => {
                rt.set_status(t!("browser.page_lost", error = e));
                return;
            }
        }
    }
}

/// Reads until the page has answered each command in `asked`, handing all it
/// says to the hook. A page sent elsewhere before answering moves to a new
/// renderer without them: Edge then never draws it, and crashes it moments
/// later. False if the page is lost, or the input stopped, first.
fn answered(ws: &mut Socket, rt: &InputRt, mut asked: Vec<u64>, hook: &mut dyn PageHook) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !asked.is_empty() {
        if rt.stopped() {
            return false;
        }
        if Instant::now() > deadline {
            rt.set_status(t!("browser.page_silent"));
            return false;
        }
        match ws.read() {
            Ok(Message::Text(text)) => {
                let Ok(v) = serde_json::from_str::<Value>(text.as_str()) else { continue };
                if crashed(rt, &v) {
                    return false;
                }
                asked.retain(|&id| v["id"].as_u64() != Some(id));
                hook.message(&v);
            }
            Ok(_) => {}
            Err(e) if timed_out(&e) => {}
            Err(e) => {
                rt.set_status(t!("browser.page_lost", error = e));
                return false;
            }
        }
    }
    true
}

/// A page whose renderer died stays blank and answers nothing again: it must
/// be opened anew.
fn crashed(rt: &InputRt, v: &Value) -> bool {
    let dead = v["method"] == "Inspector.targetCrashed";
    if dead {
        rt.set_status(t!("browser.page_crashed"));
    }
    dead
}

fn timed_out(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(e)
        if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_profile_is_new_private_and_hard_to_guess() {
        let a = private_profile("LiveCraft-web-test", 1234).unwrap();
        let b = private_profile("LiveCraft-web-test", 1234).unwrap();
        assert_ne!(a, b, "never the same name twice");
        assert!(a.is_dir() && a.starts_with(profile_base()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o700);
        }
        let _ = std::fs::remove_dir_all(a);
        let _ = std::fs::remove_dir_all(b);
    }

    use std::sync::mpsc;

    type Browser = WebSocket<TcpStream>;

    /// A page to talk to in place of Edge or Chrome: `script` plays the
    /// browser's part on the connection `stream_page` opens.
    fn fake_page<T: Send + 'static>(
        script: impl FnOnce(&mut Browser) -> T + Send + 'static,
    ) -> (String, thread::JoinHandle<T>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("ws://{}/devtools/page/test", listener.local_addr().unwrap());
        let browser = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut page = tungstenite::accept(stream).unwrap();
            page.get_ref().set_read_timeout(Some(Duration::from_millis(20))).unwrap();
            script(&mut page)
        });
        (address, browser)
    }

    /// The next command, if one comes before `wait` is over.
    fn command(page: &mut Browser, wait: Duration) -> Option<(u64, String)> {
        let end = Instant::now() + wait;
        while Instant::now() < end {
            match page.read() {
                Ok(Message::Text(text)) => {
                    let v: Value = serde_json::from_str(text.as_str()).unwrap();
                    return Some((v["id"].as_u64().unwrap(), v["method"].as_str().unwrap().to_string()));
                }
                Err(e) if !timed_out(&e) => return None,
                _ => {}
            }
        }
        None
    }

    fn say(page: &mut Browser, v: Value) {
        page.send(Message::text(v.to_string())).unwrap();
    }

    fn picture() -> String {
        use base64::Engine as _;
        let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([200, 40, 40, 255]));
        let mut png = std::io::Cursor::new(Vec::new());
        img.write_to(&mut png, image::ImageFormat::Png).unwrap();
        base64::prelude::BASE64_STANDARD.encode(png.into_inner())
    }

    /// Runs `stream_page` as a web input does, until `done` or a few seconds.
    /// Also tells whether it returned by itself.
    fn stream(address: String, done: impl Fn(&InputRt) -> bool) -> (Arc<InputRt>, bool) {
        let rt = InputRt::new(false, true);
        let (tx, rx) = mpsc::channel();
        let page = rt.clone();
        thread::spawn(move || {
            stream_page(&page, &address, "data:text/html,x", false, (4, 4), 30, &mut PlainPage);
            let _ = tx.send(());
        });
        let end = Instant::now() + Duration::from_secs(5);
        let mut returned = false;
        while !returned && Instant::now() < end && !done(&rt) {
            returned = rx.try_recv().is_ok();
            thread::sleep(Duration::from_millis(10));
        }
        rt.shutdown();
        let _ = rx.recv_timeout(Duration::from_secs(1));
        (rt, returned)
    }

    /// The bug behind pages that never showed: told where to go before it had
    /// answered its first commands, a page just opened by a browser still
    /// starting moved to a new renderer without them. Edge then never drew it
    /// and crashed it a few seconds later, leaving the window on about:blank.
    /// Here the page answers late, as such a renderer does.
    #[test]
    fn a_page_is_sent_to_its_address_only_once_it_has_answered() {
        let (address, browser) = fake_page(|page| {
            let mut asked = Vec::new();
            while let Some(command) = command(page, Duration::from_millis(300)) {
                asked.push(command);
            }
            let early = asked.iter().any(|(_, method)| method == "Page.navigate");
            for (id, _) in &asked {
                say(page, json!({"id": id, "result": {}}));
            }
            let mut navigated = early;
            while let Some((id, method)) = command(page, Duration::from_secs(2)) {
                say(page, json!({"id": id, "result": {}}));
                if method == "Page.navigate" {
                    navigated = true;
                    let frame = json!({"data": picture(), "sessionId": 1, "metadata": {}});
                    say(page, json!({"method": "Page.screencastFrame", "params": frame}));
                }
            }
            (asked, early, navigated)
        });
        let (rt, _) = stream(address, |rt| rt.seq() > 0);
        let (asked, early, navigated) = browser.join().unwrap();
        assert!(!early, "sent to its address before answering: {asked:?}");
        assert!(navigated, "never sent to its address");
        assert!(rt.seq() > 0, "the page never drew: {:?}", rt.status());
    }

    /// A page whose renderer died says so once, then answers every request
    /// with an error: it must be opened again, not waited on forever.
    #[test]
    fn a_crashed_page_is_given_up_with_a_status() {
        let (address, browser) = fake_page(|page| {
            while let Some((id, method)) = command(page, Duration::from_secs(2)) {
                if method == "Page.navigate" {
                    say(page, json!({"id": id, "result": {}}));
                    say(page, json!({"method": "Inspector.targetCrashed", "params": {}}));
                } else if method == "Page.captureScreenshot" {
                    say(page, json!({"id": id, "error": {"code": -32603, "message": "Internal error"}}));
                } else {
                    say(page, json!({"id": id, "result": {}}));
                }
            }
        });
        let (rt, returned) = stream(address, |_| false);
        browser.join().unwrap();
        assert!(returned, "still waiting on a dead page");
        assert!(rt.status().is_some());
        assert_eq!(rt.seq(), 0);
    }

    /// Web pages started again and again, each time in a browser just
    /// launched, as `--selftest` and the soak do: each must draw, and none
    /// may need opening twice. Needs Edge or Chrome, and the machine to
    /// itself; `ROUNDS` and `PAGES` set the size.
    #[test]
    #[ignore]
    fn web_pages_draw_every_time() {
        let number = |name: &str, default: usize| {
            std::env::var(name).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
        };
        let (rounds, pages) = (number("ROUNDS", 20), number("PAGES", 2));
        let page = "data:text/html,<body style='margin:0'><div id=a style='width:100vw;height:100vh'></div>\
            <script>let n=0;setInterval(()=>{a.style.background='rgb('+(n++%25200+40)+',40,40)'},33)</script>";
        let (mut silent, mut troubled) = (0, 0);
        for round in 0..rounds {
            let start = Instant::now();
            let rts: Vec<_> = (0..pages).map(|_| InputRt::new(false, true)).collect();
            let threads: Vec<_> = rts
                .iter()
                .map(|rt| {
                    let rt = rt.clone();
                    thread::spawn(move || run_browser(&rt, page, false, 640, 360, 30))
                })
                .collect();
            let mut first = vec![None; pages];
            let mut statuses = vec![None; pages];
            while start.elapsed() < Duration::from_secs(20) && first.iter().any(Option::is_none) {
                for (i, rt) in rts.iter().enumerate() {
                    if first[i].is_none() && rt.seq() >= 2 {
                        first[i] = Some(start.elapsed().as_millis());
                    }
                    statuses[i] = statuses[i].take().or(rt.status());
                }
                thread::sleep(Duration::from_millis(10));
            }
            silent += first.iter().filter(|f| f.is_none()).count();
            troubled += statuses.iter().filter(|s| s.is_some()).count();
            eprintln!("round {round}: first pictures after {first:?} ms, statuses {statuses:?}");
            for rt in &rts {
                rt.shutdown();
            }
            for t in threads {
                t.join().unwrap();
            }
            while HOST.lock().unwrap().is_some() {
                thread::sleep(Duration::from_millis(50));
            }
        }
        eprintln!("{silent} silent and {troubled} troubled of {} pages", rounds * pages);
        assert_eq!((silent, troubled), (0, 0));
    }
}
