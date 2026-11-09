//! Text drawn straight into frames, for titles and the clock. Titles can name
//! a font installed on this computer; anything else uses the embedded one.

use crate::frame::Frame;
use ab_glyph::{Font, FontArc, FontRef, FontVec, ScaleFont, VariableFont, point};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

fn default_font() -> &'static FontArc {
    static FONT: OnceLock<FontArc> = OnceLock::new();
    FONT.get_or_init(|| {
        FontArc::new(FontRef::try_from_slice(epaint_default_fonts::UBUNTU_LIGHT).expect("embedded font"))
    })
}

struct Installed {
    families: Vec<String>,
    bold: bool,
    italic: bool,
    path: PathBuf,
    index: u32,
}

fn read_font_file(path: &std::path::Path, list: &mut Vec<Installed>) {
    let Ok(data) = std::fs::read(path) else { return };
    for index in 0..ttf_parser::fonts_in_collection(&data).unwrap_or(1) {
        let Ok(face) = ttf_parser::Face::parse(&data, index) else { continue };
        let names = face.names().into_iter().filter(|n| {
            n.name_id == ttf_parser::name_id::FAMILY || n.name_id == ttf_parser::name_id::TYPOGRAPHIC_FAMILY
        });
        let families =
            names.filter_map(|n| n.to_string().or_else(|| mac_roman(&n))).map(|n| n.to_lowercase()).collect();
        list.push(Installed {
            families,
            bold: face.is_bold(),
            italic: face.is_italic(),
            path: path.to_path_buf(),
            index,
        });
    }
}

/// Apple's own fonts may name themselves only in Mac Roman, which agrees
/// with ASCII for the names people type.
fn mac_roman(name: &ttf_parser::name::Name) -> Option<String> {
    let roman = name.platform_id == ttf_parser::PlatformId::Macintosh && name.encoding_id == 0;
    (roman && name.name.is_ascii()).then(|| String::from_utf8_lossy(name.name).into_owned())
}

/// What is installed, by the names inside the fonts: a file name says little
/// (Arial Black is ariblk.ttf, Segoe UI is segoeui.ttf). Read once.
fn installed() -> &'static [Installed] {
    static LIST: OnceLock<Vec<Installed>> = OnceLock::new();
    LIST.get_or_init(|| {
        let mut list = Vec::new();
        for dir in crate::platform::font_dirs() {
            read_font_dir(&dir, 0, &mut list);
        }
        list
    })
}

/// Linux sorts fonts into folders by maker, several levels deep.
fn read_font_dir(dir: &std::path::Path, depth: u8, list: &mut Vec<Installed>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for path in entries.flatten().map(|e| e.path()) {
        let ext = path.extension().map_or(String::new(), |e| e.to_string_lossy().to_lowercase());
        if matches!(ext.as_str(), "ttf" | "otf" | "ttc") {
            read_font_file(&path, list);
        } else if depth < 4 && path.is_dir() {
            read_font_dir(&path, depth + 1, list);
        }
    }
}

/// The right weight of a family, never an italic if an upright one exists.
fn find_font(family: &str, bold: bool) -> Option<(PathBuf, u32)> {
    let wanted = family.trim().to_lowercase();
    if wanted.is_empty() {
        return None;
    }
    let fits = installed().iter().filter(|f| f.families.contains(&wanted));
    let best = fits.max_by_key(|f| (!f.italic, f.bold == bold))?;
    Some((best.path.clone(), best.index))
}

fn load_face(family: &str, bold: bool) -> Option<FontArc> {
    let (path, index) = find_font(family, bold)?;
    let bytes = std::fs::read(path).ok()?;
    let mut face = FontVec::try_from_vec_and_index(bytes, index).ok()?;
    if bold {
        face.set_variation(b"wght", 700.0);
    }
    Some(FontArc::new(face))
}

/// The named font if this computer has it, else the embedded one.
pub fn face(family: &str, bold: bool) -> FontArc {
    static CACHE: OnceLock<Mutex<HashMap<(String, bool), Option<FontArc>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let mut map = cache.lock().unwrap();
    map.entry((family.to_string(), bold))
        .or_insert_with(|| load_face(family, bold))
        .clone()
        .unwrap_or_else(|| default_font().clone())
}

pub fn measure(text: &str, px: f32) -> f32 {
    measure_with(default_font(), text, px)
}

pub fn measure_with(font: &FontArc, text: &str, px: f32) -> f32 {
    let f = font.as_scaled(px);
    let mut w = 0.0;
    let mut last = None;
    for c in text.chars() {
        let g = f.glyph_id(c);
        if let Some(l) = last {
            w += f.kern(l, g);
        }
        w += f.h_advance(g);
        last = Some(g);
    }
    w
}

/// Draws `text` with its top-left at (x, y). `bold` strikes it twice, offset by
/// a fraction of a pixel, because the embedded font only has a light weight.
pub fn draw(frame: &mut Frame, text: &str, x: f32, y: f32, px: f32, rgb: [u8; 3], bold: bool) {
    let offsets: &[f32] = if bold { &[0.0, px / 40.0] } else { &[0.0] };
    for off in offsets {
        draw_with(default_font(), frame, text, x + off, y, px, rgb);
    }
}

pub fn draw_with(font: &FontArc, frame: &mut Frame, text: &str, x: f32, y: f32, px: f32, rgb: [u8; 3]) {
    let f = font.as_scaled(px);
    let baseline = y + f.ascent();
    let mut pen = x;
    let mut last = None;
    let (fw, fh) = (frame.w as i32, frame.h as i32);
    for c in text.chars() {
        let id = f.glyph_id(c);
        if let Some(l) = last {
            pen += f.kern(l, id);
        }
        let glyph = id.with_scale_and_position(px, point(pen, baseline));
        pen += f.h_advance(id);
        last = Some(id);
        let Some(outline) = font.outline_glyph(glyph) else { continue };
        let b = outline.px_bounds();
        outline.draw(|gx, gy, cov| {
            let px_x = b.min.x as i32 + gx as i32;
            let px_y = b.min.y as i32 + gy as i32;
            if px_x < 0 || px_y < 0 || px_x >= fw || px_y >= fh {
                return;
            }
            let a = (cov.clamp(0.0, 1.0) * 255.0) as u32;
            let i = (px_y as usize * frame.w + px_x as usize) * 4;
            let inv = 255 - a;
            for ch in 0..3 {
                let d = frame.data[i + ch] as u32;
                frame.data[i + ch] = ((rgb[ch] as u32 * a + d * inv) / 255) as u8;
            }
            let d = frame.data[i + 3] as u32;
            frame.data[i + 3] = (a + d * inv / 255).min(255) as u8;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_visible_pixels_including_accents() {
        let mut f = Frame::new(200, 60);
        draw(&mut f, "LiveCraft é", 4.0, 4.0, 32.0, [255, 255, 255], true);
        assert!(f.data.as_chunks::<4>().0.iter().any(|p| p[3] > 200));
        assert!(measure("LiveCraft", 32.0) > 30.0);
    }

    #[test]
    fn fonts_are_found_by_their_real_name() {
        // Every system has its own; the file names do not spell the family.
        let family = if cfg!(windows) {
            "Segoe UI"
        } else if cfg!(target_os = "macos") {
            "Helvetica"
        } else {
            "DejaVu Sans"
        };
        let missing = || {
            let some: Vec<_> = installed().iter().flat_map(|f| f.families.first()).take(20).collect();
            panic!("{family} absente parmi {} polices, dont {some:?}", installed().len())
        };
        let plain = find_font(family, false).unwrap_or_else(missing);
        let bold = find_font(&family.to_uppercase(), true).unwrap_or_else(missing);
        assert_ne!(plain, bold, "bold is its own face");
        assert!(find_font("Police Qui N Existe Pas", false).is_none());
    }

    #[test]
    fn an_unknown_font_falls_back_and_still_draws() {
        let font = face("Police Qui N Existe Pas", true);
        let mut f = Frame::new(200, 60);
        draw_with(&font, &mut f, "Texte", 4.0, 4.0, 32.0, [255, 255, 255]);
        assert!(f.data.as_chunks::<4>().0.iter().any(|p| p[3] > 200));
    }
}
