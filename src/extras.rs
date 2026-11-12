//! Smaller pieces of the desk: scripts, data sources and PTZ camera control.

use crate::engine::Engine;
use crate::functions::Args;
use crate::model::Kind;
use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

/// One parsed line of a script.
#[derive(Debug, PartialEq)]
pub enum Line {
    Call { name: String, args: Vec<(String, String)> },
    Sleep(u64),
    LoopStart(Option<u32>),
    LoopEnd,
}

/// `API.Function("Fade",Input:="2",Duration:=1000)`, or the short form
/// `Fade Input=2 Duration=1000`.
fn parse_line(text: &str) -> Option<Line> {
    let t = text.trim();
    let low = t.to_lowercase();
    if t.is_empty() || t.starts_with('\'') {
        return None;
    }
    if let Some(v) = low.strip_prefix("sleep(") {
        return v.trim_end_matches(')').trim().parse().ok().map(Line::Sleep);
    }
    if low == "do" || low.starts_with("do while") {
        return Some(Line::LoopStart(None));
    }
    if low == "loop" || low.starts_with("loop ") || low == "next" || low.starts_with("next ") {
        return Some(Line::LoopEnd);
    }
    if low.starts_with("for ") {
        let count = low.split(" to ").nth(1).and_then(|v| v.trim().parse().ok());
        return Some(Line::LoopStart(Some(count.unwrap_or(1))));
    }
    let call = "api.function(";
    let inner = low.starts_with(call).then(|| t[call.len()..].strip_suffix(')')).flatten();
    if let Some(inner) = inner {
        let parts = split_args(inner);
        let name = parts.first()?.trim().trim_matches('"').to_string();
        let args = parts[1..]
            .iter()
            .filter_map(|p| {
                let (k, v) = p.split_once(":=")?;
                Some((k.trim().to_string(), v.trim().trim_matches('"').to_string()))
            })
            .collect();
        return Some(Line::Call { name, args });
    }
    let mut words = t.split_whitespace();
    let name = words.next()?.to_string();
    let args = words.filter_map(|w| w.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
    Some(Line::Call { name, args })
}

/// Splits on commas outside quotes.
fn split_args(s: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                out.last_mut().unwrap().push(c);
            }
            ',' if !quoted => out.push(String::new()),
            _ => out.last_mut().unwrap().push(c),
        }
    }
    out
}

pub fn parse_script(code: &str) -> Vec<Line> {
    code.lines().filter_map(parse_line).collect()
}

impl Engine {
    /// Runs on its own thread; a stop request ends it at the next line.
    pub fn run_script(self: &Arc<Self>, code: String) {
        let engine = self.clone();
        let started_at = self.script_stop.load(Ordering::Relaxed);
        thread::spawn(move || {
            let lines = parse_script(&code);
            let stopped = || {
                engine.script_stop.load(Ordering::Relaxed) != started_at
                    || engine.quit.load(Ordering::Relaxed)
            };
            let mut loops: Vec<(usize, Option<u32>)> = Vec::new();
            let mut at = 0;
            while at < lines.len() && !stopped() {
                match &lines[at] {
                    Line::Sleep(ms) => {
                        let end = Instant::now() + Duration::from_millis(*ms);
                        while Instant::now() < end && !stopped() {
                            thread::sleep(Duration::from_millis(10));
                        }
                    }
                    Line::Call { name, args } => {
                        let mut a = Args::new();
                        for (k, v) in args {
                            match k.to_lowercase().as_str() {
                                "input" => a.input = Some(v.clone()),
                                "value" => a.value = Some(v.clone()),
                                "duration" => a.duration = v.parse().ok(),
                                "selectedname" => a.selected_name = Some(v.clone()),
                                _ => {}
                            }
                        }
                        let _ = engine.call(name, &a);
                    }
                    Line::LoopStart(count) => loops.push((at, *count)),
                    Line::LoopEnd => {
                        if let Some(top) = loops.last_mut() {
                            let again = match &mut top.1 {
                                None => true,
                                Some(left) => {
                                    *left = left.saturating_sub(1);
                                    *left > 0
                                }
                            };
                            if again {
                                at = top.0;
                            } else {
                                loops.pop();
                            }
                        }
                    }
                }
                at += 1;
            }
        });
    }

    /// Keeps titles linked to a data source up to date.
    pub(crate) fn data_loop(self: Arc<Self>) {
        let mut tick = 0u64;
        while !self.quit.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_secs(1));
            tick += 1;
            let (sources, bound) = {
                let mut s = self.state.lock().unwrap();
                for d in &mut s.project.data_sources {
                    let every = d.auto_next_secs as u64;
                    if every > 0 && tick.is_multiple_of(every) {
                        d.row += 1;
                    }
                }
                let sources = s.project.data_sources.clone();
                let bound: Vec<(u64, (usize, usize, usize), String, String)> = s
                    .project
                    .inputs
                    .iter()
                    .filter_map(|i| match &i.kind {
                        Kind::Title { text, sub, data: Some(link), .. } => {
                            Some((i.id, *link, text.clone(), sub.clone()))
                        }
                        _ => None,
                    })
                    .collect();
                (sources, bound)
            };
            for (id, (src, col_text, col_sub), text, sub) in bound {
                let Some(source) = sources.get(src) else { continue };
                let rows = read_table(&source.path);
                if rows.is_empty() {
                    continue;
                }
                let row = &rows[source.row % rows.len()];
                let cell = |c: usize| row.get(c).cloned().unwrap_or_default();
                let (new_text, new_sub) = (cell(col_text), cell(col_sub));
                if new_text != text || new_sub != sub {
                    {
                        let mut s = self.state.lock().unwrap();
                        if let Some(Kind::Title { text, sub, .. }) = s.input_mut(id).map(|i| &mut i.kind) {
                            *text = new_text;
                            *sub = new_sub;
                        }
                    }
                    self.restart_input(id, true);
                }
            }
        }
    }
}

/// Reads a CSV or plain text file into rows of cells.
pub fn read_table(path: &str) -> Vec<Vec<String>> {
    std::fs::read(path).map(|b| parse_csv(&decode_text(&b))).unwrap_or_default()
}

/// Spreadsheets save text in several encodings; Excel's plain CSV is not UTF-8.
fn decode_text(bytes: &[u8]) -> String {
    if let [0xff, 0xfe, rest @ ..] = bytes {
        let units: Vec<u16> =
            rest.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return String::from_utf16_lossy(&units);
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(t) => t.to_string(),
        Err(_) => bytes.iter().map(|b| windows_1252(*b)).collect(),
    }
}

fn windows_1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}',
        '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
    ];
    if (0x80..0xa0).contains(&b) { HIGH[b as usize - 0x80] } else { b as char }
}

pub fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let sep =
        if text.lines().next().is_some_and(|l| l.contains(';') && !l.contains(',')) { ';' } else { ',' };
    let mut rows = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let mut row = vec![String::new()];
        let mut quoted = false;
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '"' if quoted && chars.peek() == Some(&'"') => {
                    chars.next();
                    row.last_mut().unwrap().push('"');
                }
                '"' => quoted = !quoted,
                c if c == sep && !quoted => row.push(String::new()),
                c => row.last_mut().unwrap().push(c),
            }
        }
        rows.push(row);
    }
    rows
}

/// Camera moves sent over VISCA, the common PTZ protocol on IP.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ptz {
    Move { pan: i8, tilt: i8 },
    Stop,
    ZoomIn,
    ZoomOut,
    ZoomStop,
    Home,
    Recall(u8),
    Store(u8),
}

pub fn visca_payload(cmd: Ptz) -> Vec<u8> {
    let (speed_p, speed_t) = (0x0c, 0x0a);
    match cmd {
        Ptz::Move { pan, tilt } => {
            let x = match pan {
                p if p < 0 => 0x01,
                p if p > 0 => 0x02,
                _ => 0x03,
            };
            let y = match tilt {
                t if t > 0 => 0x01,
                t if t < 0 => 0x02,
                _ => 0x03,
            };
            vec![0x81, 0x01, 0x06, 0x01, speed_p, speed_t, x, y, 0xff]
        }
        Ptz::Stop => vec![0x81, 0x01, 0x06, 0x01, speed_p, speed_t, 0x03, 0x03, 0xff],
        Ptz::ZoomIn => vec![0x81, 0x01, 0x04, 0x07, 0x25, 0xff],
        Ptz::ZoomOut => vec![0x81, 0x01, 0x04, 0x07, 0x35, 0xff],
        Ptz::ZoomStop => vec![0x81, 0x01, 0x04, 0x07, 0x00, 0xff],
        Ptz::Home => vec![0x81, 0x01, 0x06, 0x04, 0xff],
        Ptz::Recall(n) => vec![0x81, 0x01, 0x04, 0x3f, 0x02, n, 0xff],
        Ptz::Store(n) => vec![0x81, 0x01, 0x04, 0x3f, 0x01, n, 0xff],
    }
}

/// Wraps a command in the VISCA over IP header, on UDP port 52381.
pub fn visca_packet(seq: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![0x01, 0x00];
    p.extend((payload.len() as u16).to_be_bytes());
    p.extend(seq.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

pub fn ptz_send(host: &str, seq: u32, cmd: Ptz) -> Result<(), String> {
    let host = host.trim();
    if host.is_empty() {
        return Err(t!("extras.ptz_missing_address").into());
    }
    let target = if host.contains(':') { host.to_string() } else { format!("{host}:52381") };
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    // UDP can lose a packet, and a lost Stop leaves the camera turning.
    let copies = if matches!(cmd, Ptz::Stop | Ptz::ZoomStop) { 3 } else { 1 };
    let packet = visca_packet(seq, &visca_payload(cmd));
    for _ in 0..copies {
        sock.send_to(&packet, &target).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_lines_parse() {
        let code = "' note\nAPI.Function(\"Fade\",Input:=\"CAM, 2\",Duration:=1000)\nSleep(500)\nFor i = 1 To 3\nCut\nNext\n";
        let lines = parse_script(code);
        assert_eq!(lines.len(), 5);
        match &lines[0] {
            Line::Call { name, args } => {
                assert_eq!(name, "Fade");
                assert_eq!(args[0], ("Input".to_string(), "CAM, 2".to_string()));
                assert_eq!(args[1], ("Duration".to_string(), "1000".to_string()));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(lines[1], Line::Sleep(500));
        assert_eq!(lines[2], Line::LoopStart(Some(3)));
        assert_eq!(lines[4], Line::LoopEnd);
    }

    #[test]
    fn csv_handles_quotes_and_semicolons() {
        let rows = parse_csv("Nom,Rôle\n\"Dupont, Jean\",\"Il dit \"\"oui\"\"\"\n");
        assert_eq!(rows[1][0], "Dupont, Jean");
        assert_eq!(rows[1][1], "Il dit \"oui\"");
        let rows = parse_csv("a;b\nc;d\n");
        assert_eq!(rows[1], ["c", "d"]);
    }

    #[test]
    fn loops_with_conditions_still_close() {
        assert_eq!(parse_line("Loop While x"), Some(Line::LoopEnd));
        assert_eq!(parse_line("loop until y"), Some(Line::LoopEnd));
        assert!(matches!(parse_line("api.function(\"Cut\")"), Some(Line::Call { .. })));
    }

    #[test]
    fn excel_text_keeps_its_accents() {
        assert_eq!(decode_text(b"caf\xe9;\x80"), "café;€");
        assert_eq!(decode_text("\u{feff}été".as_bytes()), "été");
        assert_eq!(decode_text(&[0xff, 0xfe, 0xe9, 0x00]), "é");
    }

    #[test]
    fn ptz_packets_follow_the_protocol() {
        let p = visca_packet(7, &visca_payload(Ptz::Home));
        assert_eq!(&p[..4], &[0x01, 0x00, 0x00, 0x05]);
        assert_eq!(&p[4..8], &[0, 0, 0, 7]);
        assert_eq!(*p.last().unwrap(), 0xff);
        let left = visca_payload(Ptz::Move { pan: -1, tilt: 0 });
        assert_eq!(&left[6..8], &[0x01, 0x03]);
    }
}
