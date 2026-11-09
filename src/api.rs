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
use std::time::Duration;

pub const HTTP_PORT: u16 = 8088;
pub const TCP_PORT: u16 = 8099;

/// Far above any real request; a client sending more is not a controller.
const MAX_LINE: usize = 16 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_CONNECTIONS: usize = 64;

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
            // Counted from 1 where the input has items: a list's file, a DVD's chapter.
            let selected = rt.as_ref().map_or(0, |r| {
                let has_items = r.items.load(Ordering::Relaxed) > 0;
                if has_items { r.item.load(Ordering::Relaxed) + 1 } else { 0 }
            });
            let looped = matches!(i.kind, Kind::Video { looped: true, .. });
            let title = esc(&i.name);
            let cap = |b: bool| if b { "True" } else { "False" };
            x.push_str(&format!(
                "<input key=\"{}\" number=\"{}\" type=\"{}\" title=\"{title}\" state=\"{state}\" \
                 position=\"{pos}\" duration=\"{dur}\" muted=\"{}\" loop=\"{}\" selectedIndex=\"{selected}\">{title}",
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
                input.map(|i| i.id).ok_or("Aucun télestrateur")?
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
            Err(_) => failed = Some(format!("Port {HTTP_PORT} déjà utilisé")),
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
            Err(_) => failed = Some(format!("Port {TCP_PORT} déjà utilisé")),
        }
        failed
    }

    fn serve_http(self: Arc<Self>, stream: TcpStream, cfg: &Mutex<ApiConfig>) {
        let local = stream.peer_addr().is_ok_and(|a| a.ip().is_loopback());
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut reader = BufReader::new(match stream.try_clone() {
            Ok(s) => s,
            Err(_) => return,
        });
        let Some(first) = read_line(&mut reader) else { return };
        let (mut auth, mut host, mut origin, mut fetch_site) =
            (String::new(), String::new(), String::new(), String::new());
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
                _ => {}
            }
        }
        let (password, lan) = {
            let c = cfg.lock().unwrap();
            (c.password.clone(), c.lan)
        };
        let mut out = stream;
        if !request_allowed(&host, &origin, &fetch_site, lan) {
            let _ =
                out.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            return;
        }
        if !local && !password.is_empty() {
            let given = String::from_utf8_lossy(&base64_decode(&auth)).into_owned();
            if given.split_once(':').map(|(_, p)| p) != Some(password.as_str()) {
                let _ = out.write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"LiveCraft\"\r\n\
                      Content-Length: 0\r\nConnection: close\r\n\r\n",
                );
                return;
            }
        }
        let target = first.split_whitespace().nth(1).unwrap_or("/");
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let lower = path.trim_end_matches('/').to_lowercase();
        let (status, kind, body) = match lower.as_str() {
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
            "" => ("200 OK", "text/html; charset=utf-8", CONTROLLER_PAGE.to_string()),
            "/telestrator" => ("200 OK", "text/html; charset=utf-8", TELESTRATOR_PAGE.to_string()),
            _ => ("404 Not Found", "text/plain", "Introuvable".to_string()),
        };
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n",
            body.len()
        );
        let _ = out.write_all(head.as_bytes());
        let _ = out.write_all(body.as_bytes());
        // Dropping the socket outright can reach the client as a reset before
        // it has read the answer: say the end, then wait for theirs.
        let _ = out.shutdown(std::net::Shutdown::Write);
        let _ = out.set_read_timeout(Some(Duration::from_secs(1)));
        let _ = reader.read_to_end(&mut Vec::new());
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
            other => format!("{other} ER Commande non prise en charge\r\n"),
        })
    }
}

const CONTROLLER_PAGE: &str = r#"<!doctype html><html lang="fr"><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><title>LiveCraft</title>
<style>body{background:#131314;color:#eee;font:16px system-ui;margin:0;padding:12px}
button{background:#343538;color:#fff;border:0;border-radius:4px;padding:14px;margin:4px;font-size:16px}
#i button{min-width:130px}.p{background:#f28c00}.o{background:#0a7a16}a{color:#8cf}</style>
<h3>LiveCraft</h3><div><button onclick="f('Cut')">Coupe</button><button onclick="f('Fade')">Fondu</button>
<button onclick="f('FadeToBlack')">Fondu au noir</button> <a href="/telestrator">Télestrateur</a></div>
<div id="i"></div><script>
function f(n,i){fetch('/api/?Function='+n+(i?'&Input='+i:'')).then(r)}
function r(){fetch('/api/').then(x=>x.text()).then(t=>{let d=new DOMParser().parseFromString(t,'text/xml');
let p=d.querySelector('preview').textContent,o=d.querySelector('active').textContent,l=[];
d.querySelectorAll('inputs>input').forEach(e=>{let n=e.getAttribute('number'),b=document.createElement('button');
b.className=n==o?'o':n==p?'p':'';b.textContent=n+' '+e.getAttribute('title');b.onclick=()=>f('PreviewInput',n);l.push(b)});
document.getElementById('i').replaceChildren(...l)})}r();setInterval(r,1000)</script></html>"#;

const TELESTRATOR_PAGE: &str = r##"<!doctype html><html lang="fr"><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><title>Télestrateur</title>
<style>body{background:#131314;color:#eee;font:16px system-ui;margin:0;padding:8px}
canvas{background:#222;width:100%;touch-action:none;display:block;margin-top:8px}
button,select,input{font-size:16px;margin:2px}</style>
<div><input type=color id=c value="#ff2020"><select id=s><option value=line>Ligne<option value=rect>Carré<option value=circle>Cercle</select>
<select id=z><option value=0.006>Fin<option value=0.012 selected>Moyen<option value=0.025>Épais</select>
<button onclick="a('TelestratorUndo')">Annuler</button><button onclick="a('TelestratorClear')">Effacer</button></div>
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
}
