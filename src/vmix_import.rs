//! Opens .vmix presets. The format is not published: everything here was read
//! from real presets the person provided, so unknown input types are kept as
//! named placeholders and listed in the report instead of being dropped.
//! Stream settings stay behind on purpose: they can hold private keys.

use crate::model::*;
use roxmltree::{Document, Node};
use std::collections::HashMap;
use std::path::Path;

#[derive(Default, Debug, Clone)]
pub struct Report {
    pub name: String,
    pub inputs: usize,
    pub layers: usize,
    pub overlays: usize,
    pub placeholders: Vec<String>,
    pub missing_files: Vec<String>,
    pub notes: Vec<String>,
}

pub fn load(path: &str) -> Result<(Project, Report), String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let text = decode(&bytes);
    let (project, mut report) = parse(&text)?;
    report.name = Path::new(path).file_stem().map_or(String::new(), |s| s.to_string_lossy().into_owned());
    Ok((project, report))
}

fn decode(bytes: &[u8]) -> String {
    match bytes {
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        [0xFF, 0xFE, rest @ ..] => {
            let units: Vec<u16> =
                rest.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            String::from_utf16_lossy(&units)
        }
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn is_true(n: Node, name: &str) -> bool {
    n.attribute(name).is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

fn number(n: Node, name: &str, default: f32) -> f32 {
    n.attribute(name).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn child_text<'a>(n: Node<'a, 'a>, name: &str) -> Option<&'a str> {
    n.children().find(|c| c.has_tag_name(name)).and_then(|c| c.text())
}

fn child_num(n: Node, name: &str, default: f32) -> f32 {
    child_text(n, name).and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Colours are stored as a signed 32-bit ARGB number.
fn argb(v: &str) -> Option<[u8; 4]> {
    let raw = v.trim().parse::<i64>().ok()? as u32;
    Some([(raw >> 16) as u8, (raw >> 8) as u8, raw as u8, (raw >> 24) as u8])
}

/// Keeps an address readable by ffmpeg whatever the passphrase holds.
fn percent_encode(v: &str) -> String {
    v.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// `#AARRGGBB` or `#RRGGBB`.
fn hex_colour(v: &str) -> Option<[u8; 3]> {
    let h = v.trim().trim_start_matches('#');
    let h = if h.len() == 8 && h.is_ascii() { &h[2..] } else { h };
    if h.len() != 6 || !h.is_ascii() {
        return None;
    }
    let c = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some([c(0)?, c(2)?, c(4)?])
}

/// Pan runs from -1 to 1 across the frame, up being positive.
fn placement(m: Node) -> Placement {
    let z = child_num(m, "ZoomX", 1.0);
    let (px, py) = (child_num(m, "PanX", 0.0), child_num(m, "PanY", 0.0));
    Placement { x: 0.5 + px / 2.0 - z / 2.0, y: 0.5 - py / 2.0 - z / 2.0, scale: z }
}

fn border(m: Node) -> Border {
    let Some(b) = m.children().find(|c| c.has_tag_name("Border")) else {
        return Border::default();
    };
    if child_text(b, "Enabled") != Some("true") {
        return Border::default();
    }
    let color = child_text(b, "ColourHTML").and_then(hex_colour).unwrap_or([255, 255, 255]);
    Border { color, thickness: child_num(b, "Thickness", 0.0), radius: child_num(b, "Radius", 0.0) }
}

/// Runs `f` over the entries of an embedded position list.
fn with_positions<R>(text: Option<&str>, f: impl FnOnce(Vec<Node>) -> R) -> Option<R> {
    let doc = Document::parse(text?).ok()?;
    let list = doc.root_element().children().filter(|n| n.has_tag_name("MatrixPosition")).collect();
    Some(f(list))
}

#[derive(Clone, Copy)]
struct Pos {
    placement: Placement,
    border: Border,
    hidden: bool,
    zoom: f32,
    pan: (f32, f32),
    rotate: f32,
    turns: u8,
}

/// Positions holds the input itself then its first layers; the extended list
/// carries on where it stops.
fn positions(input: Node) -> Vec<Pos> {
    let read = |list: Vec<Node>| -> Vec<Pos> {
        list.iter()
            .map(|m| Pos {
                placement: placement(*m),
                border: border(*m),
                hidden: child_text(*m, "Hidden") == Some("true"),
                zoom: child_num(*m, "ZoomX", 1.0),
                pan: (child_num(*m, "PanX", 0.0), child_num(*m, "PanY", 0.0)),
                rotate: m
                    .children()
                    .find(|c| c.has_tag_name("Rotate"))
                    .map_or(0.0, |r| child_num(r, "Z", 0.0)),
                turns: match child_text(*m, "SourceRotation") {
                    Some("R90") => 1,
                    Some("R180") => 2,
                    Some("R270") => 3,
                    _ => 0,
                },
            })
            .collect()
    };
    let mut main = with_positions(input.attribute("Positions"), read).unwrap_or_default();
    let more = with_positions(input.attribute("PositionsExtended"), read).unwrap_or_default();
    if more.len() > main.len() {
        main.extend_from_slice(&more[main.len()..]);
    }
    main
}

struct TitleData {
    texts: Vec<String>,
    color: [u8; 3],
    font: String,
    size: f32,
    presets: Vec<TitlePreset>,
}

fn title_data(input: Node) -> TitleData {
    let mut t = TitleData {
        texts: Vec::new(),
        color: [255, 255, 255],
        font: String::new(),
        size: 72.0,
        presets: Vec::new(),
    };
    if let Some(doc) = input.attribute("XML").and_then(|x| Document::parse(x).ok()) {
        for item in doc.root_element().children().filter(|n| n.has_tag_name("item")) {
            let Some(v) = child_text(item, "value") else { continue };
            t.texts.push(v.to_string());
            if t.texts.len() > 1 {
                continue;
            }
            // The look of a title is the look of its first text.
            if let Some(f) = item.children().find(|c| c.has_tag_name("Font")) {
                if let Some(c) = f.attribute("FillColor").and_then(hex_colour) {
                    t.color = c;
                }
                t.font = f.attribute("Name").unwrap_or("").to_string();
                t.size = number(f, "Size", t.size);
            }
        }
    }
    if let Some(doc) = input.attribute("PresetsXML").and_then(|x| Document::parse(x).ok()) {
        for preset in doc.descendants().filter(|n| n.has_tag_name("TitlePreset")) {
            let values: Vec<&str> = preset
                .descendants()
                .filter(|n| n.has_tag_name("TitlePresetItem"))
                .filter_map(|i| child_text(i, "Value"))
                .collect();
            if let Some(first) = values.first() {
                t.presets.push(TitlePreset {
                    name: (*first).to_string(),
                    text: (*first).to_string(),
                    sub: values.get(1).map_or(String::new(), |s| (*s).to_string()),
                });
            }
        }
    }
    t
}

/// The LUT's own file if it is still there, else the copy kept in the preset.
fn lut_file(original: Option<&str>, embedded: Option<&str>) -> String {
    if let Some(p) = original.filter(|p| Path::new(p).exists()) {
        return p.to_string();
    }
    let Some(data) = embedded else { return String::new() };
    let stem = Path::new(original.unwrap_or("lut"))
        .file_stem()
        .map_or("lut".to_string(), |s| s.to_string_lossy().into_owned());
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    let dir = Path::new(&base).join("LiveCraft").join("luts");
    // Two projects can hold different LUTs under the same name.
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(data, &mut hash);
    let file = dir.join(format!("{stem}-{:016x}.png", std::hash::Hasher::finish(&hash)));
    let saved =
        std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&file, crate::api::base64_decode(data)));
    if saved.is_ok() { file.to_string_lossy().into_owned() } else { String::new() }
}

fn effects(input: Node, notes: &mut Vec<String>, title: &str) -> Vec<Effect> {
    let Some(doc) = input.attribute("RenderEffectsXML").and_then(|x| Document::parse(x).ok()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in doc.descendants().filter(|n| n.has_tag_name("renderEffect")) {
        let kind = match e.attribute("type") {
            Some("Gaussian Blur") => EffectKind::Blur,
            Some("LUT") => EffectKind::Lut,
            Some("Multi Effect") | None => continue,
            Some(other) => {
                notes.push(format!("{title} : effet « {other} » non repris"));
                continue;
            }
        };
        let strength = e
            .descendants()
            .find(|n| n.has_tag_name("percentValue"))
            .and_then(|n| n.text())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.5);
        let original = e.descendants().find(|n| n.has_tag_name("dataFilename")).and_then(|n| n.text());
        let embedded = e.descendants().find(|n| n.has_tag_name("dataValue")).and_then(|n| n.text());
        let path = lut_file(original, embedded);
        if kind == EffectKind::Lut && path.is_empty() {
            notes.push(format!("{title} : LUT intégrée non reprise"));
            continue;
        }
        let mut crop = [0.0; 4];
        if let Some(c) = child_text(e, "cropping") {
            let v: Vec<f32> = c.split(',').filter_map(|p| p.trim().parse().ok()).collect();
            if v.len() == 4 {
                crop = [v[0], 1.0 - v[2], v[1], 1.0 - v[3]];
            }
        }
        let enabled = e.attribute("enabled") != Some("False");
        out.push(Effect { kind, enabled, strength, path, crop });
    }
    out
}

/// The picture settings, which all default to no change.
fn shader(n: Node, i: &mut Input) {
    let v = |name: &str, d: f32| number(n, &format!("VideoShader_{name}"), d);
    let c = &mut i.colour;
    c.black = v("Black", 0.0);
    c.white = v("White", 1.0);
    c.rgb = [1.0 + v("Red", 0.0), 1.0 + v("Green", 0.0), 1.0 + v("Blue", 0.0)];
    c.alpha = v("Alpha", 1.0);
    c.saturation = v("Saturation", 1.0);
    c.hue = v("Hue", 0.0);
    c.lift = [v("CCLiftR", 0.0), v("CCLiftG", 0.0), v("CCLiftB", 0.0)];
    c.gamma = [v("CCGammaR", 1.0), v("CCGammaG", 1.0), v("CCGammaB", 1.0)];
    c.gain = [v("CCGainR", 1.0), v("CCGainG", 1.0), v("CCGainB", 1.0)];
    i.position.crop =
        [v("ClippingX1", 0.0), 1.0 - v("ClippingX2", 1.0), v("ClippingY1", 0.0), 1.0 - v("ClippingY2", 1.0)];
    if let Some(key) = n.attribute("VideoShader_ColorKey").and_then(argb)
        && key != [0, 0, 0, 0]
    {
        i.key.mode = KeyMode::Colour;
        i.key.color = [key[0], key[1], key[2]];
        i.key.tolerance = v("ToleranceGreen", 0.25).clamp(0.0, 1.0);
    }
    let luma = v("LumaKeyThreshold", 0.0);
    if luma > 0.0 {
        i.key.mode = KeyMode::Luma;
        i.key.tolerance = luma;
    }
}

fn audio(n: Node, i: &mut Input) {
    let a = &mut i.audio;
    a.volume = number(n, "VolumeF", 1.0).max(0.0).sqrt();
    a.muted = is_true(n, "Muted");
    a.balance = number(n, "BalanceF", 0.0);
    a.follow = is_true(n, "AutoAudioMixing");
    a.headphones = number(n, "HeadphonesVolumeF", 1.0);
    a.delay_ms = number(n, "AudioDelay", 0.0);
    let gain = number(n, "AudioGain", 1.0);
    a.gain_db = 20.0 * gain.max(0.0001).log10();
    a.visible = n.attribute("MixerVisible") != Some("False");
    if n.attribute("BusMaster") == Some("False") {
        a.buses &= !1;
    }
    if is_true(n, "AudioEQEnabled") {
        for (b, v) in a.eq.iter_mut().enumerate() {
            *v = number(n, &format!("AudioEQGainDB{b}"), 0.0);
        }
    }
    if is_true(n, "AudioCompressorEnabled") {
        a.comp_ratio = number(n, "AudioCompressorRatio", 1.0);
        let t = number(n, "AudioCompressorThreshold", 0.1);
        a.comp_threshold = 20.0 * t.max(0.0001).log10();
    }
    if is_true(n, "AudioNoiseGateEnabled") {
        let t = number(n, "AudioNoiseGateThreshold", 0.0);
        a.gate_threshold = 20.0 * t.max(0.00003).log10();
    }
    a.channel = match n.attribute("AudioChannel") {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    };
}

fn srt_url(n: Node, host: &str) -> String {
    let port = n.attribute("SRTPort").unwrap_or("9000");
    let latency = n.attribute("SRTLatencyMS").and_then(|v| v.parse::<u64>().ok()).unwrap_or(200);
    let mut url = format!("srt://{host}:{port}?mode=caller&latency={}", latency * 1000);
    if let Some(id) = n.attribute("SRTStreamID").filter(|v| !v.is_empty()) {
        url.push_str(&format!("&streamid={id}"));
    }
    if let Some(pass) = n.attribute("SRTPassPhrase").filter(|v| !v.is_empty()) {
        url.push_str(&format!("&passphrase={}", percent_encode(pass)));
    }
    url
}

fn kind_of(n: Node, title: &str, r: &mut Report) -> Kind {
    let text = n.text().unwrap_or("").trim().to_string();
    match n.attribute("Type").unwrap_or("") {
        "0" => Kind::Video {
            path: text,
            looped: is_true(n, "Loop"),
            mark_in: number(n, "RangeStart", 0.0) as u64,
            mark_out: number(n, "RangeStop", 0.0) as u64,
            rate: number(n, "Rate", 1.0),
        },
        "1" => Kind::Image { path: text },
        "6" => {
            r.notes.push(format!("{title} : capture de tout l'écran, pas d'une seule fenêtre"));
            Kind::Screen
        }
        "7" => {
            let path = n.attribute("AudioPath").unwrap_or("");
            let device = path.split_once(':').map_or(path, |(_, d)| d).to_string();
            Kind::Microphone { device }
        }
        "12" => {
            if is_true(n, "ColourBars") {
                return Kind::Bars;
            }
            match n.attribute("Colour").and_then(argb) {
                Some([r, g, b, a]) if a > 0 => Kind::Solid { color: [r, g, b] },
                _ => Kind::Scene,
            }
        }
        "5000" => Kind::Browser { url: text, alpha: false },
        "7000" => {
            let buffer_ms = number(n, "Buffer", 0.0) as u32;
            Kind::Network { url: text, buffer_ms }
        }
        "10000" => Kind::Network { url: srt_url(n, &text), buffer_ms: 0 },
        "9000" => {
            let t = title_data(n);
            let template = n.attribute("OriginalTitle").unwrap_or("").to_lowercase();
            let style = if template.contains("centre") || template.contains("middle") {
                TitleStyle::Centered
            } else {
                TitleStyle::LowerThird
            };
            r.notes.push(format!("{title} : titre refait, sans le modèle graphique d'origine"));
            Kind::Title {
                text: t.texts.first().cloned().unwrap_or_default(),
                sub: t.texts.get(1).cloned().unwrap_or_default(),
                style,
                color: t.color,
                accent: [240, 120, 40],
                ticker_speed: 4.0,
                countdown_ms: 300_000,
                font: t.font,
                size: t.size,
                presets: t.presets,
                data: None,
            }
        }
        other => {
            r.placeholders.push(format!("{title} (type {other})"));
            Kind::Scene
        }
    }
}

const COLOUR_TABS: [&str; 6] = ["Red", "Green", "Orange", "Purple", "Aqua", "Blue"];

fn categories(root: Node, p: &mut Project) {
    let Some(c) = root.children().find(|n| n.has_tag_name("CategorySettings")) else { return };
    let get = |name: &str, field: &str| child_text(c, &format!("{name}.{field}"));
    let mut read = |name: &str, index: usize| {
        let color = get(name, "Color").and_then(argb).map(|v| [v[0], v[1], v[2]]);
        let cat = Category {
            name: get(name, "Text").unwrap_or("").to_string(),
            color: color.unwrap_or([90, 90, 96]),
            enabled: get(name, "Enabled") != Some("0"),
        };
        // Custom tabs keep their numbers, so gaps are filled with hidden ones.
        while p.categories.len() <= index {
            let hidden = Category { name: String::new(), color: [90, 90, 96], enabled: false };
            p.categories.push(hidden);
        }
        p.categories[index] = cat;
    };
    for (n, name) in COLOUR_TABS.iter().enumerate() {
        read(name, n);
    }
    for n in 1..=16 {
        if get(&format!("Custom{n}"), "Enabled") == Some("1") {
            read(&format!("Custom{n}"), 5 + n);
        }
    }
}

fn format(root: Node, p: &mut Project) {
    let Some(f) = root.children().find(|n| n.has_tag_name("OutputFormat")) else { return };
    if let Some((w, h)) = f.attribute("OutputSize").and_then(|s| s.split_once('x'))
        && let (Ok(w), Ok(h)) = (w.parse(), h.parse())
    {
        p.out_w = w;
        p.out_h = h;
    }
    // The frame rate is a frame length in 100-nanosecond units.
    let ticks = number(f, "OutputFrameRate", 0.0);
    if ticks > 0.0 {
        p.fps = (10_000_000.0 / ticks).round() as u32;
    }
}

fn clocks(root: Node, p: &mut Project) {
    for (n, name) in ["ProductionClockSettings", "ProductionClockSettings2"].iter().enumerate() {
        let Some(c) = root.children().find(|x| x.has_tag_name(*name)) else { continue };
        let Some(inner) = c.children().find(|x| x.has_tag_name("ProductionClockSettings")) else {
            continue;
        };
        match child_text(inner, "Type") {
            Some("1") => p.clocks[n].kind = ClockType::Clock,
            Some("3") => p.clocks[n].kind = ClockType::TimeRemaining,
            _ => {}
        }
    }
}

pub fn parse(xml: &str) -> Result<(Project, Report), String> {
    let doc = Document::parse(xml).map_err(|_| "Ce fichier n'est pas un projet lisible".to_string())?;
    let root = doc.root_element();
    if !root.has_tag_name("XML") {
        return Err("Ce fichier n'est pas un projet vMix".into());
    }
    let nodes: Vec<Node> = root.children().filter(|n| n.has_tag_name("Input")).collect();
    let mut r = Report { inputs: nodes.len(), ..Report::default() };
    let mut p = Project::blank();
    format(root, &mut p);
    categories(root, &mut p);
    clocks(root, &mut p);

    // Inputs refer to each other by key; ours number from 1 in file order.
    let ids: HashMap<&str, u64> =
        nodes.iter().enumerate().filter_map(|(n, i)| Some((i.attribute("Key")?, n as u64 + 1))).collect();
    for (n, node) in nodes.iter().enumerate() {
        let id = n as u64 + 1;
        let title = node
            .attribute("Title")
            .filter(|t| !t.is_empty())
            .or(node.attribute("OriginalTitle"))
            .unwrap_or("Entrée")
            .to_string();
        let kind = kind_of(*node, &title, &mut r);
        if let Kind::Video { path, .. } | Kind::Image { path } = &kind
            && !Path::new(path).exists()
        {
            r.missing_files.push(path.clone());
        }
        let mut input = Input::new(id, title.clone(), kind);
        input.set_preset_category(number(*node, "Category", 0.0).max(0.0) as usize);
        input.compact = is_true(*node, "Collapsed");
        input.mirror = is_true(*node, "Mirror");
        input.auto_play = is_true(*node, "AutoPlay");
        input.auto_pause = is_true(*node, "AutoPause");
        input.auto_restart = is_true(*node, "AutoRestart");
        input.click = ClickAction::Preview;
        audio(*node, &mut input);
        shader(*node, &mut input);
        input.effects = effects(*node, &mut r.notes, &title);

        let list = positions(*node);
        if let Some(own) = list.first() {
            input.position.zoom = own.zoom;
            input.position.pan_x = own.pan.0;
            input.position.pan_y = own.pan.1;
            input.position.rotate = own.rotate;
            input.position.quarter_turns = own.turns;
        }
        for layer in 0..10 {
            let Some(key) = node.attribute(format!("Overlay{layer}").as_str()) else { continue };
            let Some(target) = ids.get(key) else { continue };
            let mut l = Layer::new(*target, Placement::FULL);
            if let Some(pos) = list.get(layer + 1) {
                l.placement = pos.placement;
                l.border = pos.border;
                l.visible = !pos.hidden;
            }
            input.layers.push(l);
            r.layers += 1;
        }
        for ch in 0..8 {
            if is_true(*node, &format!("MasterOverlay{ch}")) {
                p.overlays[ch] = Some(id);
                p.overlay_last[ch] = Some(id);
                r.overlays += 1;
            }
            if is_true(*node, &format!("MasterOverlayLast{ch}")) && p.overlay_last[ch].is_none() {
                p.overlay_last[ch] = Some(id);
            }
        }
        if node.attribute("Triggers").is_some_and(|t| t.contains("<InputTrigger>")) {
            r.notes.push(format!("{title} : déclencheurs non repris"));
        }
        p.inputs.push(input);
    }

    // Both states count from zero.
    if let Some(s) = root.children().find(|n| n.has_tag_name("State")) {
        let pick = |name: &str| {
            let at = s.attribute(name).and_then(|v| v.parse::<usize>().ok())?;
            p.inputs.get(at).map(|i| i.id)
        };
        let (main, preview) = (pick("Main"), pick("Preview"));
        p.program = main;
        p.preview = preview;
    }
    if let Some(rec) = root.children().find(|n| n.has_tag_name("RecordingSettings")) {
        let file = child_text(rec, "Filename").unwrap_or("");
        if let Some(dir) = Path::new(file).parent() {
            p.records[0].folder = dir.to_string_lossy().into_owned();
        }
    }
    r.notes.push("Destinations de diffusion non reprises : à saisir dans les réglages".into());
    Ok((p, r))
}

#[cfg(test)]
mod tests {
    use super::*;

    const POS: &str = "&lt;ArrayOfMatrixPosition&gt;\
        &lt;MatrixPosition&gt;&lt;Hidden&gt;false&lt;/Hidden&gt;&lt;ZoomX&gt;1&lt;/ZoomX&gt;&lt;PanX&gt;0&lt;/PanX&gt;&lt;PanY&gt;0&lt;/PanY&gt;&lt;/MatrixPosition&gt;\
        &lt;MatrixPosition&gt;&lt;Hidden&gt;false&lt;/Hidden&gt;&lt;Border&gt;&lt;Enabled&gt;true&lt;/Enabled&gt;&lt;Thickness&gt;20&lt;/Thickness&gt;&lt;Radius&gt;0&lt;/Radius&gt;&lt;ColourHTML&gt;White&lt;/ColourHTML&gt;&lt;/Border&gt;\
        &lt;ZoomX&gt;0.3&lt;/ZoomX&gt;&lt;PanX&gt;0.6&lt;/PanX&gt;&lt;PanY&gt;-0.5&lt;/PanY&gt;&lt;/MatrixPosition&gt;\
        &lt;/ArrayOfMatrixPosition&gt;";

    fn sample() -> String {
        format!(
            "<XML><Version>9</Version>\
             <Input Type=\"1\" Title=\"Logo\" OriginalTitle=\"logo.png\" Key=\"a\" Category=\"2\" Collapsed=\"True\">C:\\none\\logo.png</Input>\
             <Input Type=\"12\" Title=\"Scene\" OriginalTitle=\"Colour\" Key=\"b\" Colour=\"-16777216\" ColourBars=\"False\" Overlay0=\"c\" Overlay1=\"a\" Positions=\"{POS}\" Category=\"3\" MasterOverlay3=\"True\"/>\
             <Input Type=\"10000\" Title=\"\" OriginalTitle=\"SRT\" Key=\"c\" SRTPort=\"8890\" SRTLatencyMS=\"200\" SRTStreamID=\"read:cam1\" Muted=\"True\" VolumeF=\"0.25\">10.0.0.2</Input>\
             <Input Type=\"7\" Title=\"Son\" Key=\"d\" AudioPath=\"WASAPI:Micro (USB)\" Muted=\"False\" BusMaster=\"False\" AutoAudioMixing=\"False\"/>\
             <Input Type=\"9000\" Title=\"Texte\" OriginalTitle=\"Text Middle Centre.gtzip\" Key=\"e\" XML=\"&lt;items&gt;&lt;item name=&quot;Message.Text&quot;&gt;&lt;Font FillColor=&quot;#FF102030&quot;/&gt;&lt;value&gt;MERCI&lt;/value&gt;&lt;/item&gt;&lt;/items&gt;\"/>\
             <Input Type=\"5000\" Title=\"Web\" Key=\"f\">https://exemple.test/</Input>\
             <Input Type=\"4242\" Title=\"Inconnu\" Key=\"g\"/>\
             <State Main=\"1\" Preview=\"0\"/>\
             <CategorySettings><Red.Enabled>1</Red.Enabled><Red.Color>-7667712</Red.Color><Red.Text>CAMS</Red.Text></CategorySettings>\
             <OutputFormat OutputSize=\"1280x720\" OutputFrameRate=\"400000\"/>\
             </XML>"
        )
    }

    #[test]
    fn inputs_keep_their_kind_order_and_names() {
        let (p, r) = parse(&sample()).unwrap();
        assert_eq!(r.inputs, 7);
        assert!(matches!(p.inputs[0].kind, Kind::Image { .. }));
        assert!(matches!(p.inputs[1].kind, Kind::Solid { color: [0, 0, 0] }));
        assert!(matches!(&p.inputs[3].kind, Kind::Microphone { device } if device == "Micro (USB)"));
        assert!(matches!(&p.inputs[5].kind, Kind::Browser { url, .. } if url == "https://exemple.test/"));
        assert_eq!(p.inputs[2].name, "SRT");
    }

    #[test]
    fn state_counts_from_zero_and_format_reads() {
        let (p, _) = parse(&sample()).unwrap();
        assert_eq!(p.program, Some(2));
        assert_eq!(p.preview, Some(1));
        assert_eq!((p.out_w, p.out_h, p.fps), (1280, 720, 25));
    }

    #[test]
    fn layers_land_where_the_pan_and_zoom_say() {
        let (p, r) = parse(&sample()).unwrap();
        assert_eq!(r.layers, 2);
        let scene = &p.inputs[1];
        assert_eq!(scene.layers[0].input, 3);
        // First layer uses the second position entry; the box sits low right.
        let l = scene.layers[1];
        assert_eq!(l.input, 1);
        assert_eq!(scene.layers[0].placement.scale, 0.3);
        assert!((scene.layers[0].placement.x - 0.65).abs() < 0.001);
        assert!((scene.layers[0].placement.y - 0.6).abs() < 0.001);
        assert_eq!(scene.layers[0].border.thickness, 20.0);
    }

    #[test]
    fn overlays_categories_and_folding_are_kept() {
        let (p, _) = parse(&sample()).unwrap();
        assert_eq!(p.overlays[3], Some(2));
        assert_eq!(p.inputs[0].category, Some(1));
        assert!(p.inputs[0].compact);
        assert_eq!(p.categories[0].name, "CAMS");
        assert_eq!(p.categories[0].color, [139, 0, 0]);
    }

    #[test]
    fn srt_address_is_rebuilt() {
        let (p, _) = parse(&sample()).unwrap();
        let Kind::Network { url, .. } = &p.inputs[2].kind else { panic!() };
        assert_eq!(url, "srt://10.0.0.2:8890?mode=caller&latency=200000&streamid=read:cam1");
    }

    #[test]
    fn titles_bring_their_text_and_colour() {
        let (p, _) = parse(&sample()).unwrap();
        let Kind::Title { text, color, style, .. } = &p.inputs[4].kind else { panic!() };
        assert_eq!(text, "MERCI");
        assert_eq!(*color, [0x10, 0x20, 0x30]);
        assert_eq!(*style, TitleStyle::Centered);
    }

    #[test]
    fn sound_settings_convert() {
        let (p, _) = parse(&sample()).unwrap();
        assert!((p.inputs[2].audio.volume - 0.5).abs() < 0.001);
        assert!(p.inputs[2].audio.muted);
        assert_eq!(p.inputs[3].audio.buses & 1, 0);
        assert!(!p.inputs[3].audio.follow);
    }

    #[test]
    fn unknown_types_stay_as_named_placeholders() {
        let (p, r) = parse(&sample()).unwrap();
        assert!(matches!(p.inputs[6].kind, Kind::Scene));
        assert_eq!(r.placeholders.len(), 1);
        assert!(r.placeholders[0].contains("Inconnu"));
    }

    #[test]
    fn a_missing_file_is_reported_and_other_xml_refused() {
        let (_, r) = parse(&sample()).unwrap();
        assert_eq!(r.missing_files.len(), 1);
        assert!(parse("<vmix></vmix>").is_err());
        assert!(parse("pas du xml").is_err());
    }

    #[test]
    fn signed_colours_read_as_argb() {
        assert_eq!(argb("-16777216"), Some([0, 0, 0, 255]));
        assert_eq!(argb("0"), Some([0, 0, 0, 0]));
        assert_eq!(hex_colour("#FFFFFFFF"), Some([255, 255, 255]));
        assert_eq!(hex_colour("White"), None);
        assert_eq!(hex_colour("éééé"), None, "must not cut inside a letter");
        assert_eq!(percent_encode("a&b c"), "a%26b%20c");
    }
}
