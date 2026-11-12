//! Remote control, on the published addresses: HTTP on port 8088 and TCP on
//! 8099, so control surfaces and companion apps that speak them can connect.
//! Only this computer can reach it unless network access is turned on.

use crate::engine::Engine;
use crate::functions::Args;
use crate::model::*;
use crate::titles::{Stroke, StrokeShape};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const HTTP_PORT: u16 = 8088;
pub const TCP_PORT: u16 = 8099;

/// Far above any real request; a client sending more is not a controller.
const MAX_LINE: usize = 16 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_CONNECTIONS: usize = 64;
/// For a whole request, headers and body: a client sending a byte now and
/// then must not keep a place for ever.
const REQUEST_TIME: Duration = Duration::from_secs(10);

static CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

/// Holds a place among the connections for as long as one is served.
struct Place;

impl Place {
    fn take() -> Option<Self> {
        if CONNECTIONS.fetch_add(1, Ordering::Relaxed) >= MAX_CONNECTIONS {
            CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
            return None;
        }
        Some(Self)
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A connection whose reads share one deadline, rather than each having its
/// own: every read gets only the time left.
struct Timed {
    stream: TcpStream,
    until: Instant,
}

impl Timed {
    fn new(stream: TcpStream, time: Duration) -> Self {
        Self { stream, until: Instant::now() + time }
    }

    /// Time left to read, or an error once it has run out.
    fn left(&self) -> std::io::Result<Duration> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "request too slow"));
        }
        self.stream.set_read_timeout(Some(left))?;
        Ok(left)
    }

    fn peek(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.left()?;
        self.stream.peek(buf)
    }

    /// The answer is sent: a moment more to read the client's goodbye.
    fn end(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Write);
        self.until = Instant::now() + Duration::from_secs(1);
    }
}

impl Read for Timed {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.left()?;
        self.stream.read(buf)
    }
}

impl Write for Timed {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.stream.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

/// Reads one line, refusing one that never ends.
fn read_line(reader: &mut impl BufRead) -> Option<String> {
    let mut line = String::new();
    let read = reader.by_ref().take(MAX_LINE as u64).read_line(&mut line).ok()?;
    (read > 0 && line.ends_with('\n')).then_some(line)
}

/// The host part of `Host` or `Origin`, without scheme or port.
fn host_name(value: &str) -> &str {
    let v = value.trim();
    let v = v.split_once("://").map_or(v, |(_, rest)| rest);
    let v = v.split('/').next().unwrap_or(v);
    if let Some(end) = v.strip_prefix('[').and_then(|r| r.find(']')) {
        return &v[..end + 2];
    }
    v.split(':').next().unwrap_or(v)
}

/// A web page open in any browser on this computer can reach these ports, so
/// a request that a foreign page caused is turned away. Tools and the desk's
/// own pages pass, as they send none of these marks.
fn request_allowed(host: &str, origin: &str, fetch_site: &str, lan: bool) -> bool {
    let name = host_name(host);
    if !lan && !matches!(name, "localhost" | "127.0.0.1" | "[::1]") {
        return false;
    }
    if !origin.is_empty() && !host_name(origin).eq_ignore_ascii_case(name) {
        return false;
    }
    !fetch_site.eq_ignore_ascii_case("cross-site")
}

#[derive(Clone, Default)]
pub struct ApiConfig {
    pub lan: bool,
    pub password: String,
}

/// What a guest link may ask: its page and its call, nothing else.
#[derive(Clone, Copy, Debug, PartialEq)]
enum GuestAsk<'a> {
    Page(&'a str),
    Offer(&'a str),
}

fn guest_request(path: &str) -> Option<GuestAsk<'_>> {
    let rest = path.strip_prefix("/call/")?;
    let (token, tail) = rest.split_once('/').unwrap_or((rest, ""));
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    match tail {
        "" => Some(GuestAsk::Page(token)),
        "offer" => Some(GuestAsk::Offer(token)),
        _ => None,
    }
}

/// A certificate made for this run. Guests' browsers do not know it and ask
/// them to accept it, but nothing has to be bought or installed, and its key
/// never leaves memory.
fn tls_config() -> Option<Arc<rustls::ServerConfig>> {
    static CONFIG: std::sync::LazyLock<Option<Arc<rustls::ServerConfig>>> = std::sync::LazyLock::new(|| {
        let mut names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
        names.extend(crate::call::lan_address());
        let made = rcgen::generate_simple_self_signed(names).ok()?;
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(made.signing_key.serialize_der());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .ok()?
            .with_no_client_auth()
            .with_single_cert(vec![made.cert.der().clone()], key.into())
            .ok()?;
        Some(Arc::new(config))
    });
    CONFIG.clone()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn parse_query(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

pub fn base64_decode(s: &str) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for v in s.bytes().filter_map(value) {
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

impl Engine {
    /// The whole state as XML, in the published shape.
    pub fn xml_state(&self) -> String {
        let s = self.state.lock().unwrap();
        let p = &s.project;
        let number =
            |id: Option<u64>| id.and_then(|i| p.inputs.iter().position(|x| x.id == i)).map_or(0, |n| n + 1);
        let mut x = String::from("<vmix><version>29.0.0.0</version><edition>LiveCraft</edition>");
        x.push_str("<inputs>");
        for (n, i) in p.inputs.iter().enumerate() {
            let rt = self.rt(i.id);
            let state = if rt.as_ref().is_some_and(|r| r.is_playing()) { "Running" } else { "Paused" };
            let pos = rt.as_ref().map_or(0, |r| r.pos_ms.load(Ordering::Relaxed));
            let dur = rt.as_ref().map_or(0, |r| r.dur_ms.load(Ordering::Relaxed));
            // Lists, photos and slides count from 1 here, as SelectIndex does.
            let index = rt
                .as_ref()
                .filter(|r| r.items.load(Ordering::Relaxed) > 0)
                .map_or(0, |r| r.item.load(Ordering::Relaxed) + 1);
            let looped = matches!(i.kind, Kind::Video { looped: true, .. });
            let title = esc(&i.name);
            let cap = |b: bool| if b { "True" } else { "False" };
            x.push_str(&format!(
                "<input key=\"{}\" number=\"{}\" type=\"{}\" title=\"{title}\" state=\"{state}\" \
                 position=\"{pos}\" duration=\"{dur}\" muted=\"{}\" loop=\"{}\" selectedIndex=\"{index}\">{title}",
                i.key_id(),
                n + 1,
                i.kind.api_type(),
                cap(i.audio.muted),
                cap(looped),
            ));
            if let Kind::Title { text, sub, .. } = &i.kind {
                x.push_str(&format!("<text index=\"0\" name=\"Headline\">{}</text>", esc(text)));
                x.push_str(&format!("<text index=\"1\" name=\"Subtitle\">{}</text>", esc(sub)));
            }
            x.push_str("</input>");
        }
        x.push_str("</inputs><overlays>");
        for (n, o) in p.overlays.iter().enumerate() {
            match number(*o) {
                0 => x.push_str(&format!("<overlay number=\"{}\"/>", n + 1)),
                v => x.push_str(&format!("<overlay number=\"{}\">{v}</overlay>", n + 1)),
            }
        }
        x.push_str("</overlays>");
        x.push_str(&format!(
            "<preview>{}</preview><active>{}</active><fadeToBlack>{}</fadeToBlack>",
            number(p.preview),
            number(p.program),
            if s.ftb { "True" } else { "False" }
        ));
        x.push_str("<transitions>");
        for (n, t) in p.trans.iter().enumerate() {
            x.push_str(&format!(
                "<transition number=\"{}\" effect=\"{}\" duration=\"{}\"/>",
                n + 1,
                t.kind.api_name(),
                t.ms
            ));
        }
        x.push_str("</transitions>");
        let flag = |b: bool| if b { "True" } else { "False" };
        x.push_str(&format!(
            "<recording>{}</recording><external>False</external><streaming>{}</streaming>\
             <playList>{}</playList><multiCorder>{}</multiCorder>",
            flag(self.is_recording()),
            flag(self.is_streaming()),
            flag(s.playlist_on),
            flag(self.is_multi()),
        ));
        x.push_str("</vmix>");
        x
    }

    /// One digit per input: 0 off, 1 on air, 2 in preview.
    pub fn tally(&self) -> String {
        let s = self.state.lock().unwrap();
        let air = s.on_air();
        s.project
            .inputs
            .iter()
            .map(|i| {
                if air.contains(&i.id) {
                    '1'
                } else if s.project.preview == Some(i.id) {
                    '2'
                } else {
                    '0'
                }
            })
            .collect()
    }

    /// Adds a stroke from the browser page to a Telestrator input.
    fn telestrator_stroke(&self, id: u64, spec: &str) {
        let Some(rt) = self.rt(id) else { return };
        let mut fields = spec.split('|');
        let shape = match fields.next() {
            Some("rect") => StrokeShape::Rect,
            Some("circle") => StrokeShape::Circle,
            _ => StrokeShape::Line,
        };
        let hex = fields.next().unwrap_or("ff0000");
        let c = |i: usize| u8::from_str_radix(hex.get(i..i + 2).unwrap_or("00"), 16).unwrap_or(0);
        let size = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0.01);
        let points = fields
            .next()
            .unwrap_or("")
            .split(';')
            .filter_map(|p| {
                let (a, b) = p.split_once(',')?;
                Some((a.parse().ok()?, b.parse().ok()?))
            })
            .collect();
        rt.strokes.lock().unwrap().push(Stroke { shape, points, color: [c(0), c(2), c(4)], size });
        rt.strokes_gen.fetch_add(1, Ordering::Release);
    }

    pub fn telestrator_clear(&self, id: u64, last_only: bool) {
        let Some(rt) = self.rt(id) else { return };
        let mut list = rt.strokes.lock().unwrap();
        if last_only {
            list.pop();
        } else {
            list.clear();
        }
        rt.strokes_gen.fetch_add(1, Ordering::Release);
    }

    fn handle_function(self: &Arc<Self>, name: &str, q: &HashMap<String, String>) -> Result<(), String> {
        let args = Args::from_query(q);
        if name.starts_with("Telestrator") {
            let id = {
                let s = self.state.lock().unwrap();
                let input = s.project.inputs.iter().find(|i| matches!(i.kind, Kind::Telestrator { .. }));
                input.map(|i| i.id).ok_or(t!("api.no_telestrator"))?
            };
            match name {
                "TelestratorStroke" => self.telestrator_stroke(id, args.value.as_deref().unwrap_or("")),
                "TelestratorUndo" => self.telestrator_clear(id, true),
                _ => self.telestrator_clear(id, false),
            }
            return Ok(());
        }
        self.call(name, &args)
    }

    /// Opens the two control ports. Returns what could not be opened.
    pub fn start_api(self: &Arc<Self>, cfg: ApiConfig) -> Option<String> {
        let ip = if cfg.lan { IpAddr::V4(Ipv4Addr::UNSPECIFIED) } else { IpAddr::V4(Ipv4Addr::LOCALHOST) };
        let shared = Arc::new(Mutex::new(cfg));
        let mut failed = None;
        match TcpListener::bind(SocketAddr::new(ip, HTTP_PORT)) {
            Ok(l) => {
                let (engine, cfg) = (self.clone(), shared.clone());
                thread::spawn(move || {
                    for stream in l.incoming().flatten() {
                        let Some(place) = Place::take() else { continue };
                        let (engine, cfg) = (engine.clone(), cfg.clone());
                        thread::spawn(move || {
                            let _place = place;
                            engine.serve_http(stream, &cfg);
                        });
                    }
                });
            }
            Err(_) => failed = Some(t!("api.port_in_use", port = HTTP_PORT)),
        }
        match TcpListener::bind(SocketAddr::new(ip, TCP_PORT)) {
            Ok(l) => {
                let (engine, cfg) = (self.clone(), shared.clone());
                thread::spawn(move || {
                    for stream in l.incoming().flatten() {
                        // This protocol has no way to give a password, so
                        // setting one closes it to other computers.
                        let local = stream.peer_addr().is_ok_and(|a| a.ip().is_loopback());
                        if !local && !cfg.lock().unwrap().password.is_empty() {
                            continue;
                        }
                        let Some(place) = Place::take() else { continue };
                        let engine = engine.clone();
                        thread::spawn(move || {
                            let _place = place;
                            engine.serve_tcp(stream);
                        });
                    }
                });
            }
            Err(_) => failed = Some(t!("api.port_in_use", port = TCP_PORT)),
        }
        failed
    }

    fn serve_http(self: Arc<Self>, stream: TcpStream, cfg: &Mutex<ApiConfig>) {
        let local = stream.peer_addr().is_ok_and(|a| a.ip().is_loopback());
        let _ = stream.set_write_timeout(Some(REQUEST_TIME));
        let stream = Timed::new(stream, REQUEST_TIME);
        // A guest's browser only opens the camera on https. The same port
        // answers both, told apart by the first byte a TLS client sends.
        let mut first = [0u8; 1];
        if matches!(stream.peek(&mut first), Ok(1)) && first[0] == 0x16 {
            let Some(conn) = tls_config().and_then(|c| rustls::ServerConnection::new(c).ok()) else { return };
            let reader = BufReader::new(rustls::StreamOwned::new(conn, stream));
            self.answer(reader, local, true, cfg, |s| {
                s.conn.send_close_notify();
                let _ = s.flush();
                s.sock.end();
            });
        } else {
            self.answer(BufReader::new(stream), local, false, cfg, Timed::end);
        }
    }

    /// Reads one request and answers it. `finish` says the end of the answer.
    fn answer<S: Read + Write>(
        self: &Arc<Self>,
        mut reader: BufReader<S>,
        local: bool,
        tls: bool,
        cfg: &Mutex<ApiConfig>,
        finish: impl FnOnce(&mut S),
    ) {
        let Some(first) = read_line(&mut reader) else { return };
        let (mut auth, mut host, mut origin, mut fetch_site) =
            (String::new(), String::new(), String::new(), String::new());
        let mut length = 0usize;
        for _ in 0..MAX_HEADERS {
            let Some(line) = read_line(&mut reader) else { return };
            if line.trim().is_empty() {
                break;
            }
            let Some((name, value)) = line.split_once(':') else { continue };
            let value = value.trim().to_string();
            match name.trim().to_ascii_lowercase().as_str() {
                "authorization" => {
                    let scheme = value.get(..6).is_some_and(|s| s.eq_ignore_ascii_case("basic "));
                    if scheme {
                        auth = value[6..].trim().to_string();
                    }
                }
                "host" => host = value,
                "origin" => origin = value,
                "sec-fetch-site" => fetch_site = value,
                "content-length" => length = value.parse().unwrap_or(usize::MAX),
                _ => {}
            }
        }
        let (password, lan) = {
            let c = cfg.lock().unwrap();
            (c.password.clone(), c.lan)
        };
        if !request_allowed(&host, &origin, &fetch_site, lan) {
            let _ = reader
                .get_mut()
                .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            return;
        }
        let mut words = first.split_whitespace();
        let verb = words.next().unwrap_or("GET").to_string();
        let target = words.next().unwrap_or("/");
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let guest = guest_request(path);
        // A guest link is its own key: it opens that guest's page and call,
        // and nothing else, so it needs no password and gives none away.
        if guest.is_none() && !local && !password.is_empty() {
            let given = String::from_utf8_lossy(&base64_decode(&auth)).into_owned();
            if given.split_once(':').map(|(_, p)| p) != Some(password.as_str()) {
                let _ = reader.get_mut().write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"LiveCraft\"\r\n\
                      Content-Length: 0\r\nConnection: close\r\n\r\n",
                );
                return;
            }
        }
        let (status, kind, body, extra) = match guest {
            Some(ask) => {
                let plain_from_afar = !tls && !local;
                let (status, kind, body) =
                    self.guest_answer(ask, &verb, &mut reader, length, plain_from_afar);
                // The token is in the address: it must not leak to another
                // site, sit in a cache or be framed by a page that is not ours.
                (
                    status,
                    kind,
                    body,
                    "Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Frame-Options: DENY\r\n",
                )
            }
            None => {
                let (status, kind, body) = self.desk_answer(path, query);
                (status, kind, body, "")
            }
        };
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n{extra}\
             Connection: close\r\n\r\n",
            body.len()
        );
        let out = reader.get_mut();
        let _ = out.write_all(head.as_bytes());
        let _ = out.write_all(body.as_bytes());
        // Dropping the socket outright can reach the client as a reset before
        // it has read the answer: say the end, then wait for theirs.
        finish(out);
        let _ = reader.read_to_end(&mut Vec::new());
    }

    fn desk_answer(self: &Arc<Self>, path: &str, query: &str) -> (&'static str, &'static str, String) {
        let lower = path.trim_end_matches('/').to_lowercase();
        match lower.as_str() {
            "/api" => {
                let q = parse_query(query);
                let function = q.iter().find(|(k, _)| k.eq_ignore_ascii_case("Function"));
                match function.map(|(_, v)| v.clone()) {
                    None => ("200 OK", "text/xml", self.xml_state()),
                    Some(f) => match self.handle_function(&f, &q) {
                        Ok(()) => ("200 OK", "text/plain", "Function completed successfully.".into()),
                        Err(e) => ("500 Internal Server Error", "text/plain", e),
                    },
                }
            }
            "" => ("200 OK", "text/html; charset=utf-8", controller_page()),
            "/telestrator" => ("200 OK", "text/html; charset=utf-8", telestrator_page()),
            _ => ("404 Not Found", "text/plain", t!("api.not_found").to_string()),
        }
    }

    /// `plain_from_afar`: plain http from another computer, which carries the
    /// link and the call's addresses in clear.
    fn guest_answer(
        &self,
        ask: GuestAsk,
        verb: &str,
        reader: &mut impl Read,
        length: usize,
        plain_from_afar: bool,
    ) -> (&'static str, &'static str, String) {
        let token = match ask {
            GuestAsk::Page(token) | GuestAsk::Offer(token) => token,
        };
        // The link first: nothing is read for a request that opens nothing.
        let Some(route) = self.guest_route(token) else {
            return ("404 Not Found", "text/plain", t!("api.not_found").to_string());
        };
        // Only an https relay named in the link's address may pass it on in
        // plain http, as it took the guest's https itself.
        if plain_from_afar && !route.relayed {
            return ("403 Forbidden", "text/plain; charset=utf-8", t!("api.guest_needs_https").to_string());
        }
        match ask {
            GuestAsk::Page(_) if verb == "GET" => {
                ("200 OK", "text/html; charset=utf-8", crate::call::guest_html(&route.ice))
            }
            GuestAsk::Offer(_) if verb == "POST" => {
                if length > crate::call::MAX_OFFER {
                    return ("413 Content Too Large", "text/plain", String::new());
                }
                let mut offer = Vec::with_capacity(length);
                if reader.take(length as u64).read_to_end(&mut offer).is_err() || offer.len() < length {
                    return ("400 Bad Request", "text/plain", String::new());
                }
                match route.link.answer(String::from_utf8_lossy(&offer).into_owned()) {
                    Ok(sdp) => ("200 OK", "application/sdp", sdp),
                    Err(e) => ("503 Service Unavailable", "text/plain; charset=utf-8", e),
                }
            }
            _ => ("405 Method Not Allowed", "text/plain", String::new()),
        }
    }

    fn serve_tcp(self: Arc<Self>, stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
        let mut out = match stream.try_clone() {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut reader = stream;
        let mut pending = Vec::new();
        let mut buf = [0u8; 1024];
        let mut subscribed = false;
        let mut last_tally = String::new();
        loop {
            match reader.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => pending.extend_from_slice(&buf[..n]),
                Err(e)
                    if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(_) => return,
            }
            if pending.len() > MAX_LINE && !pending.contains(&b'\n') {
                return;
            }
            while let Some(at) = pending.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = pending.drain(..=at).collect();
                let line = String::from_utf8_lossy(&line).trim().to_string();
                let reply = self.tcp_command(&line, &mut subscribed);
                match reply {
                    Some(r) => {
                        if out.write_all(r.as_bytes()).is_err() {
                            return;
                        }
                    }
                    None => return,
                }
            }
            if subscribed {
                let tally = self.tally();
                if tally != last_tally {
                    last_tally.clone_from(&tally);
                    if out.write_all(format!("TALLY OK {tally}\r\n").as_bytes()).is_err() {
                        return;
                    }
                }
            }
        }
    }

    /// Answers one line. None closes the connection.
    pub fn tcp_command(self: &Arc<Self>, line: &str, subscribed: &mut bool) -> Option<String> {
        let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
        Some(match cmd.to_uppercase().as_str() {
            "TALLY" => format!("TALLY OK {}\r\n", self.tally()),
            "XML" => {
                let x = self.xml_state();
                format!("XML {}\r\n{x}", x.len())
            }
            "FUNCTION" => {
                let (name, query) = rest.split_once(' ').unwrap_or((rest, ""));
                match self.handle_function(name, &parse_query(query)) {
                    Ok(()) => "FUNCTION OK Completed\r\n".to_string(),
                    Err(e) => format!("FUNCTION ER {e}\r\n"),
                }
            }
            "SUBSCRIBE" if rest.eq_ignore_ascii_case("TALLY") => {
                *subscribed = true;
                "SUBSCRIBE OK TALLY\r\n".to_string()
            }
            "UNSUBSCRIBE" if rest.eq_ignore_ascii_case("TALLY") => {
                *subscribed = false;
                "UNSUBSCRIBE OK TALLY\r\n".to_string()
            }
            "QUIT" => return None,
            other => format!("{other} ER {}\r\n", t!("api.unsupported_command")),
        })
    }
}

/// Fills the `{{word}}` slots of a remote-control page in the interface language.
pub(crate) fn localize(page: &str, words: &[(&str, &str)]) -> String {
    let mut out = page.replace("{{lang}}", crate::i18n::language().code());
    for (slot, word) in words {
        out = out.replace(&format!("{{{{{slot}}}}}"), &esc(word));
    }
    out
}

fn controller_page() -> String {
    localize(
        CONTROLLER_PAGE,
        &[
            ("cut", t!("api.page.cut")),
            ("fade", t!("api.page.fade")),
            ("ftb", t!("api.page.fade_to_black")),
            ("telestrator", t!("api.page.telestrator")),
        ],
    )
}

fn telestrator_page() -> String {
    localize(
        TELESTRATOR_PAGE,
        &[
            ("telestrator", t!("api.page.telestrator")),
            ("line", t!("api.page.line")),
            ("rect", t!("api.page.rect")),
            ("circle", t!("api.page.circle")),
            ("thin", t!("api.page.thin")),
            ("medium", t!("api.page.medium")),
            ("thick", t!("api.page.thick")),
            ("undo", t!("api.page.undo")),
            ("clear", t!("api.page.clear")),
        ],
    )
}

const CONTROLLER_PAGE: &str = r#"<!doctype html><html lang="{{lang}}"><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><title>LiveCraft</title>
<style>body{background:#131314;color:#eee;font:16px system-ui;margin:0;padding:12px}
button{background:#343538;color:#fff;border:0;border-radius:4px;padding:14px;margin:4px;font-size:16px}
#i button{min-width:130px}.p{background:#f28c00}.o{background:#0a7a16}a{color:#8cf}</style>
<h3>LiveCraft</h3><div><button onclick="f('Cut')">{{cut}}</button><button onclick="f('Fade')">{{fade}}</button>
<button onclick="f('FadeToBlack')">{{ftb}}</button> <a href="/telestrator">{{telestrator}}</a></div>
<div id="i"></div><script>
function f(n,i){fetch('/api/?Function='+n+(i?'&Input='+i:'')).then(r)}
function r(){fetch('/api/').then(x=>x.text()).then(t=>{let d=new DOMParser().parseFromString(t,'text/xml');
let p=d.querySelector('preview').textContent,o=d.querySelector('active').textContent,l=[];
d.querySelectorAll('inputs>input').forEach(e=>{let n=e.getAttribute('number'),b=document.createElement('button');
b.className=n==o?'o':n==p?'p':'';b.textContent=n+' '+e.getAttribute('title');b.onclick=()=>f('PreviewInput',n);l.push(b)});
document.getElementById('i').replaceChildren(...l)})}r();setInterval(r,1000)</script></html>"#;

const TELESTRATOR_PAGE: &str = r##"<!doctype html><html lang="{{lang}}"><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><title>{{telestrator}}</title>
<style>body{background:#131314;color:#eee;font:16px system-ui;margin:0;padding:8px}
canvas{background:#222;width:100%;touch-action:none;display:block;margin-top:8px}
button,select,input{font-size:16px;margin:2px}</style>
<div><input type=color id=c value="#ff2020"><select id=s><option value=line>{{line}}<option value=rect>{{rect}}<option value=circle>{{circle}}</select>
<select id=z><option value=0.006>{{thin}}<option value=0.012 selected>{{medium}}<option value=0.025>{{thick}}</select>
<button onclick="a('TelestratorUndo')">{{undo}}</button><button onclick="a('TelestratorClear')">{{clear}}</button></div>
<canvas id=v width=1280 height=720></canvas><script>
let v=document.getElementById('v'),g=v.getContext('2d'),pts=[],on=false;
function a(n,q){fetch('/api/?Function='+n+(q?'&Value='+encodeURIComponent(q):''))}
function p(e){let r=v.getBoundingClientRect();return[(e.clientX-r.left)/r.width,(e.clientY-r.top)/r.height]}
v.onpointerdown=e=>{on=true;pts=[p(e)]};
v.onpointermove=e=>{if(!on)return;let q=p(e),l=pts[pts.length-1];pts.push(q);g.strokeStyle=c.value;g.lineWidth=z.value*720;
g.beginPath();g.moveTo(l[0]*1280,l[1]*720);g.lineTo(q[0]*1280,q[1]*720);g.stroke()};
v.onpointerup=e=>{on=false;let l=s.value=='line'?pts:[pts[0],pts[pts.length-1]];
a('TelestratorStroke',[s.value,c.value.slice(1),z.value,l.map(x=>x[0].toFixed(4)+','+x[1].toFixed(4)).join(';')].join('|'));g.clearRect(0,0,1280,720)}
</script></html>"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_web_pages_are_turned_away() {
        assert!(request_allowed("127.0.0.1:8088", "", "", false));
        assert!(request_allowed("localhost:8088", "http://localhost:8088", "same-origin", false));
        assert!(request_allowed("[::1]:8088", "", "", false));
        assert!(!request_allowed("evil.example:8088", "", "", false), "rebound name");
        assert!(!request_allowed("127.0.0.1:8088", "https://evil.example", "", false));
        assert!(!request_allowed("127.0.0.1:8088", "", "cross-site", false), "image or link");
        assert!(request_allowed("192.168.1.20:8088", "", "", true));
        assert!(!request_allowed("192.168.1.20:8088", "http://evil.example", "", true));
    }

    #[test]
    fn endless_lines_are_refused() {
        let mut long = std::io::Cursor::new(vec![b'a'; MAX_LINE * 2]);
        assert_eq!(read_line(&mut long), None);
        let mut fine = std::io::Cursor::new(b"GET / HTTP/1.1\r\nHost: x\r\n".to_vec());
        assert_eq!(read_line(&mut fine).as_deref(), Some("GET / HTTP/1.1\r\n"));
    }

    #[test]
    fn query_strings_decode() {
        let q = parse_query("Function=SetText&Value=Bonjour%20%C3%A0+tous&Input=3");
        assert_eq!(q["Value"], "Bonjour à tous");
        assert_eq!(q["Input"], "3");
    }

    #[test]
    fn a_stray_percent_survives() {
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("a%zz"), "a%zz");
    }

    #[test]
    fn basic_auth_decodes() {
        assert_eq!(base64_decode("dXNlcjpzZWNyZXQ="), b"user:secret");
    }

    #[test]
    fn special_characters_are_escaped() {
        assert_eq!(esc("a<b&\"c\""), "a&lt;b&amp;&quot;c&quot;");
    }

    #[test]
    fn a_guest_link_only_reaches_its_page_and_call() {
        assert_eq!(guest_request("/call/abc123"), Some(GuestAsk::Page("abc123")));
        assert_eq!(guest_request("/call/abc123/offer"), Some(GuestAsk::Offer("abc123")));
        for other in ["/api", "/", "/telestrator", "/call/", "/call/abc/../api", "/call/a.b", "/CALL/abc"] {
            assert_eq!(guest_request(other), None, "{other}");
        }
    }

    /// One request and its answer, without a network.
    struct Exchange<'a> {
        request: std::io::Cursor<&'a [u8]>,
        reply: &'a mut Vec<u8>,
    }

    impl Read for Exchange<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.request.read(buf)
        }
    }

    impl Write for Exchange<'_> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.reply.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Asks as another computer would, the desk having a password; a guest's
    /// browser asks over https.
    fn ask_from_afar(engine: &Arc<Engine>, request: &str) -> String {
        ask_from_afar_over(engine, request, true)
    }

    fn ask_from_afar_over(engine: &Arc<Engine>, request: &str, tls: bool) -> String {
        let cfg = Mutex::new(ApiConfig { lan: true, password: "secret".into() });
        let mut reply = Vec::new();
        let exchange = Exchange { request: std::io::Cursor::new(request.as_bytes()), reply: &mut reply };
        engine.answer(BufReader::new(exchange), false, tls, &cfg, |_| {});
        String::from_utf8_lossy(&reply).into_owned()
    }

    fn desk_with_calls(calls: &[(&str, &str)]) -> Arc<Engine> {
        let mut p = Project::blank();
        for (n, (token, address)) in calls.iter().enumerate() {
            let mut call = crate::call::new_call();
            if let Kind::VideoCall { token: t, address: a, .. } = &mut call {
                (*t, *a) = (token.to_string(), address.to_string());
            }
            p.inputs.push(Input::new(n as u64 + 1, "Invité", call));
        }
        Engine::start(p)
    }

    /// The link and the call's addresses never cross the network in clear:
    /// plain http from afar is turned away, unless the link names an https
    /// relay, which passes requests on.
    #[test]
    fn a_guest_link_needs_https_from_afar() {
        let (direct, relayed) = (crate::call::new_token(), crate::call::new_token());
        let engine = desk_with_calls(&[(&direct, ""), (&relayed, "https://relay.example")]);
        let get = |token: &str| format!("GET /call/{token} HTTP/1.1\r\nHost: 192.168.1.20:8088\r\n\r\n");
        let plain = ask_from_afar_over(&engine, &get(&direct), false);
        assert!(plain.starts_with("HTTP/1.1 403"), "{plain}");
        assert!(!plain.contains("RTCPeerConnection"), "no page in clear");
        assert!(ask_from_afar_over(&engine, &get(&direct), true).starts_with("HTTP/1.1 200"));
        assert!(ask_from_afar_over(&engine, &get(&relayed), false).starts_with("HTTP/1.1 200"));
        engine.shutdown();
    }

    /// A request for an unknown link is answered before its body is read,
    /// so a stranger cannot hold a connection by sending it slowly.
    #[test]
    fn an_unknown_link_is_refused_before_its_body() {
        let token = crate::call::new_token();
        let engine = desk_with_calls(&[(&token, "")]);
        // Announces a body and sends none: reading it would fail.
        let post = |token: &str| {
            format!(
                "POST /call/{token}/offer HTTP/1.1\r\nHost: 192.168.1.20:8088\r\nContent-Length: 1000\r\n\r\n"
            )
        };
        let unknown = ask_from_afar(&engine, &post(&crate::call::new_token()));
        assert!(unknown.starts_with("HTTP/1.1 404"), "{unknown}");
        let known = ask_from_afar(&engine, &post(&token));
        assert!(known.starts_with("HTTP/1.1 400"), "the real link reads its body: {known}");
        engine.shutdown();
    }

    /// A client sending a byte now and then, each in time on its own, is
    /// cut off when the whole request has taken too long.
    #[test]
    fn a_slow_request_is_cut_off_as_a_whole() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let writer = thread::spawn(move || {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            for byte in b"POST /call/x/offer HTTP/1.1\r\n".iter().cycle().take(100) {
                if s.write_all(&[*byte]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(40));
            }
        });
        let (stream, _) = listener.accept().unwrap();
        let start = Instant::now();
        let mut reader = BufReader::new(Timed::new(stream, Duration::from_millis(400)));
        let mut got = Vec::new();
        assert!(reader.read_to_end(&mut got).is_err());
        assert!(start.elapsed() < Duration::from_secs(2), "cut off after {:?}", start.elapsed());
        assert!(got.len() < 40, "{} bytes", got.len());
        drop(reader);
        let _ = writer.join();
    }

    #[test]
    fn a_guest_link_cannot_control_the_desk() {
        let token = crate::call::new_token();
        let mut p = Project::blank();
        p.inputs.push(Input::new(1, "A", Kind::Bars));
        p.inputs.push(Input::new(2, "B", Kind::Bars));
        let mut call = crate::call::new_call();
        if let Kind::VideoCall { token: t, .. } = &mut call {
            t.clone_from(&token);
        }
        p.inputs.push(Input::new(3, "Invité", call));
        (p.program, p.preview) = (Some(1), Some(2));
        let engine = Engine::start(p);
        let get = |path: &str| format!("GET {path} HTTP/1.1\r\nHost: 192.168.1.20:8088\r\n\r\n");

        let page = ask_from_afar(&engine, &get(&format!("/call/{token}")));
        assert!(page.starts_with("HTTP/1.1 200"), "{page}");
        assert!(page.contains("Rejoindre") && page.contains("no-referrer"));
        for path in [
            "/api/?Function=Cut".to_string(),
            format!("/call/{token}/../api/?Function=Cut"),
            format!("/api/?Function=Cut&Value={token}"),
            "/".to_string(),
        ] {
            let reply = ask_from_afar(&engine, &get(&path));
            assert!(reply.starts_with("HTTP/1.1 401"), "{path}: {reply}");
        }
        let wrong = ask_from_afar(&engine, &get(&format!("/call/{}", crate::call::new_token())));
        assert!(wrong.starts_with("HTTP/1.1 404"), "{wrong}");
        let offer_by_get = ask_from_afar(&engine, &get(&format!("/call/{token}/offer")));
        assert!(offer_by_get.starts_with("HTTP/1.1 405"), "{offer_by_get}");
        assert_eq!(engine.state.lock().unwrap().project.program, Some(1), "nothing was cut");

        let auth = format!("Authorization: Basic {}\r\n", crate::secret::base64(b"x:secret"));
        let with_password =
            format!("GET /api/?Function=Cut HTTP/1.1\r\nHost: 192.168.1.20:8088\r\n{auth}\r\n");
        assert!(ask_from_afar(&engine, &with_password).starts_with("HTTP/1.1 200"));
        assert_eq!(engine.state.lock().unwrap().project.program, Some(2), "the password still works");
        engine.shutdown();
    }
}
