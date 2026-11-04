//! Pictures made by the program itself: test charts, titles and drawings.

use crate::frame::Frame;
use crate::model::TitleObject;
use crate::text;

/// A generic round test card: grid, checker border, colour and grey bands.
pub fn test_card(w: usize, h: usize) -> Frame {
    const COLOURS: [[u8; 3]; 6] =
        [[240, 240, 0], [0, 240, 240], [0, 230, 0], [240, 0, 240], [240, 0, 0], [0, 0, 240]];
    let mut f = Frame::new(w, h);
    let cell = (h / 18).max(1);
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    let radius = h as f32 * 0.44;
    let corners = [(0.14, 0.2), (0.86, 0.2), (0.14, 0.8), (0.86, 0.8)];
    for y in 0..h {
        for x in 0..w {
            let (gx, gy) = (x / cell, y / cell);
            let border = y < cell || y + cell >= h || x < cell || x + cell >= w;
            let mut c = if border {
                if (gx + gy) % 2 == 0 { [0, 0, 0] } else { [255, 255, 255] }
            } else if x % cell == 0 || y % cell == 0 {
                [235, 235, 235]
            } else {
                [128, 128, 128]
            };
            for (u, v) in corners {
                let (dx, dy) = (x as f32 - u * w as f32, y as f32 - v * h as f32);
                if (dx * dx + dy * dy).sqrt() < h as f32 * 0.09 {
                    c = [250, 250, 250];
                }
            }
            let (dx, dy) = (x as f32 - cx, y as f32 - cy);
            if (dx * dx + dy * dy).sqrt() < radius {
                let v = (y as f32 - (cy - radius)) / (radius * 2.0);
                let u = (dx / radius + 1.0) / 2.0;
                c = match (v * 10.0) as u32 {
                    0 | 8 => [250, 250, 250],
                    1 => {
                        if ((u * 14.0) as u32).is_multiple_of(2) {
                            [0, 0, 0]
                        } else {
                            [250, 250, 250]
                        }
                    }
                    2 => {
                        let s = (u * 8.0) as u32 * 32;
                        [s as u8, s as u8, s as u8]
                    }
                    3 | 4 => COLOURS[((u * 6.0) as usize).min(5)],
                    5 => [10, 10, 10],
                    6 => {
                        if (x / 3) % 2 == 0 {
                            [245, 245, 245]
                        } else {
                            [10, 10, 10]
                        }
                    }
                    7 => {
                        let s = 60 + (u * 5.0) as u32 * 40;
                        [s as u8, s as u8, s as u8]
                    }
                    _ => {
                        if (0.42..0.58).contains(&u) {
                            [230, 0, 0]
                        } else {
                            [240, 240, 0]
                        }
                    }
                };
            }
            let i = (y * w + x) * 4;
            f.data[i..i + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
        }
    }
    f
}

pub fn fill_rect(f: &mut Frame, x: usize, y: usize, w: usize, h: usize, rgba: [u8; 4]) {
    for row in y..(y + h).min(f.h) {
        for col in x..(x + w).min(f.w) {
            let i = (row * f.w + col) * 4;
            f.data[i..i + 4].copy_from_slice(&rgba);
        }
    }
}

pub fn lower_third(title: &str, sub: &str, color: [u8; 3], accent: [u8; 3], w: usize, h: usize) -> Frame {
    let mut f = Frame::new(w, h);
    let (px, spx) = (h as f32 * 0.058, h as f32 * 0.036);
    let pad = h as f32 * 0.022;
    let text_w = text::measure(title, px).max(text::measure(sub, spx));
    let has_sub = !sub.is_empty();
    let box_h = pad * 2.0 + px * 1.2 + if has_sub { spx * 1.3 } else { 0.0 };
    let box_w = text_w + pad * 3.0;
    let x = (w as f32 * 0.06) as usize;
    let y = (h as f32 * 0.86 - box_h) as usize;
    let bar = (h as f32 * 0.012) as usize;
    let a = 225u32;
    let dark = [(16 * a / 255) as u8, (18 * a / 255) as u8, (24 * a / 255) as u8, a as u8];
    fill_rect(&mut f, x, y, bar, box_h as usize, [accent[0], accent[1], accent[2], 255]);
    fill_rect(&mut f, x + bar, y, box_w as usize, box_h as usize, dark);
    let tx = (x + bar) as f32 + pad * 1.4;
    text::draw(&mut f, title, tx, y as f32 + pad, px, color, true);
    if has_sub {
        text::draw(&mut f, sub, tx, y as f32 + pad + px * 1.2, spx, [205, 208, 216], false);
    }
    f
}

pub fn centered(title: &str, sub: &str, color: [u8; 3], font: &str, size: f32, w: usize, h: usize) -> Frame {
    let mut f = Frame::new(w, h);
    let face = text::face(font, true);
    let mut px = if size > 0.0 { size * h as f32 / 1080.0 } else { h as f32 * 0.12 };
    let spx = px * 0.42;
    // Long titles shrink to stay on screen.
    let widest = w as f32 * 0.94;
    let natural = text::measure_with(&face, title, px);
    if natural > widest {
        px *= widest / natural;
    }
    let tw = text::measure_with(&face, title, px);
    let block = if sub.is_empty() { px * 1.2 } else { px * 1.2 + spx * 1.3 };
    let y = (h as f32 - block) / 2.0;
    let x = (w as f32 - tw) / 2.0;
    let shadow = (px / 40.0).max(1.0);
    text::draw_with(&face, &mut f, title, x + shadow, y + shadow, px, [0, 0, 0]);
    text::draw_with(&face, &mut f, title, x, y, px, color);
    if !sub.is_empty() {
        let sw = text::measure_with(&face, sub, spx);
        let (sx, sy) = ((w as f32 - sw) / 2.0, y + px * 1.25);
        text::draw_with(&face, &mut f, sub, sx + 1.0, sy + 1.0, spx, [0, 0, 0]);
        text::draw_with(&face, &mut f, sub, sx, sy, spx, color);
    }
    f
}

pub fn countdown(ms: u64, color: [u8; 3], w: usize, h: usize) -> Frame {
    let mut f = Frame::new(w, h);
    let total = ms.div_ceil(1000);
    let label = format!("{:02}:{:02}", total / 60, total % 60);
    let px = h as f32 * 0.22;
    let tw = text::measure(&label, px);
    let (x, y) = ((w as f32 - tw) / 2.0, (h as f32 - px) / 2.2);
    text::draw(&mut f, &label, x + 4.0, y + 4.0, px, [0, 0, 0], true);
    text::draw(&mut f, &label, x, y, px, color, true);
    f
}

pub fn designed(objects: &[TitleObject], w: usize, h: usize) -> Frame {
    let mut f = Frame::new(w, h);
    let (fw, fh) = (w as f32, h as f32);
    for o in objects {
        match o {
            TitleObject::Rect { x, y, w, h, color, alpha } => {
                f.fill_round(x * fw, y * fh, w * fw, h * fh, 0.0, *color, *alpha);
            }
            TitleObject::Text { text: t, x, y, size, color, bold } => {
                text::draw(&mut f, t, x * fw, y * fh, size * fh, *color, *bold);
            }
            TitleObject::Image { path, x, y, w, h } => {
                if let Ok(img) = image::open(path) {
                    let img = img.to_rgba8();
                    let mut src =
                        Frame { w: img.width() as usize, h: img.height() as usize, data: img.into_raw() };
                    premultiply(&mut src);
                    f.blit_region(&src, [0.0; 4], x * fw, y * fh, w * fw, h * fh, 255, 0.0);
                }
            }
        }
    }
    f
}

pub fn premultiply(f: &mut Frame) {
    for p in f.data.as_chunks_mut::<4>().0 {
        let a = p[3] as u32;
        if a < 255 {
            for c in &mut p[..3] {
                *c = (*c as u32 * a / 255) as u8;
            }
        }
    }
}

pub fn load_image(path: &str, w: usize, h: usize) -> Result<Frame, String> {
    let img = image::open(path).map_err(|e| e.to_string())?.to_rgba8();
    let (iw, ih) = (img.width() as usize, img.height() as usize);
    let mut src = Frame { w: iw, h: ih, data: img.into_raw() };
    premultiply(&mut src);
    let scale = (w as f32 / iw as f32).min(h as f32 / ih as f32);
    let (tw, th) = ((iw as f32 * scale) as usize, (ih as f32 * scale) as usize);
    let mut out = Frame::new(w, h);
    out.blit_scaled(&src, ((w - tw) / 2) as i32, ((h - th) / 2) as i32, tw, th, 255);
    Ok(out)
}

/// A mark on the telestrator, in fractions of the picture.
#[derive(Clone, PartialEq, Debug)]
pub struct Stroke {
    pub shape: StrokeShape,
    pub points: Vec<(f32, f32)>,
    pub color: [u8; 3],
    pub size: f32,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum StrokeShape {
    Line,
    Rect,
    Circle,
}

fn stamp(f: &mut Frame, x: f32, y: f32, size: f32, color: [u8; 3]) {
    let r = size / 2.0;
    f.fill_round(x - r, y - r, size, size, r, color, 255);
}

fn line(f: &mut Frame, a: (f32, f32), b: (f32, f32), size: f32, color: [u8; 3]) {
    let d = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
    let steps = (d / (size / 3.0).max(1.0)).ceil().max(1.0) as usize;
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        stamp(f, a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t, size, color);
    }
}

pub fn draw_strokes(strokes: &[Stroke], w: usize, h: usize) -> Frame {
    let mut f = Frame::new(w, h);
    let (fw, fh) = (w as f32, h as f32);
    let px = |p: (f32, f32)| (p.0 * fw, p.1 * fh);
    for s in strokes {
        let size = s.size * fh;
        let (Some(first), Some(last)) = (s.points.first(), s.points.last()) else { continue };
        let (a, b) = (px(*first), px(*last));
        match s.shape {
            StrokeShape::Line => {
                for pair in s.points.windows(2) {
                    line(&mut f, px(pair[0]), px(pair[1]), size, s.color);
                }
                if s.points.len() == 1 {
                    stamp(&mut f, a.0, a.1, size, s.color);
                }
            }
            StrokeShape::Rect => {
                let (c, d) = ((b.0, a.1), (a.0, b.1));
                for (p, q) in [(a, c), (c, b), (b, d), (d, a)] {
                    line(&mut f, p, q, size, s.color);
                }
            }
            StrokeShape::Circle => {
                let r = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
                let steps = 96;
                let at = |i: usize| {
                    let t = i as f32 / steps as f32 * std::f32::consts::TAU;
                    (a.0 + r * t.cos(), a.1 + r * t.sin())
                };
                for i in 0..steps {
                    line(&mut f, at(i), at(i + 1), size, s.color);
                }
            }
        }
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_styles_leave_most_of_the_picture_transparent() {
        let f = lower_third("Nom", "Rôle", [255; 3], [200, 0, 0], 640, 360);
        let clear = f.data.as_chunks::<4>().0.iter().filter(|p| p[3] == 0).count();
        assert!(clear > 640 * 360 / 2);
        assert!(f.data.as_chunks::<4>().0.iter().any(|p| p[3] == 255));
    }

    #[test]
    fn countdown_rounds_up_and_draws() {
        let f = countdown(61_500, [255; 3], 320, 180);
        assert!(f.data.as_chunks::<4>().0.iter().any(|p| p[3] > 200));
    }

    #[test]
    fn designed_title_draws_its_objects() {
        let objects = vec![
            TitleObject::Rect { x: 0.0, y: 0.0, w: 0.5, h: 1.0, color: [255, 0, 0], alpha: 255 },
            TitleObject::Text {
                text: "A".into(),
                x: 0.6,
                y: 0.2,
                size: 0.4,
                color: [0, 255, 0],
                bold: false,
            },
        ];
        let f = designed(&objects, 100, 50);
        assert_eq!(f.data[(10 * 100 + 10) * 4], 255);
        assert!(f.data.as_chunks::<4>().0.iter().any(|p| p[1] > 200));
    }

    #[test]
    fn telestrator_draws_a_line_between_points() {
        let s = Stroke {
            shape: StrokeShape::Line,
            points: vec![(0.1, 0.5), (0.9, 0.5)],
            color: [255, 0, 0],
            size: 0.05,
        };
        let f = draw_strokes(&[s], 100, 100);
        assert_eq!(f.data[(50 * 100 + 50) * 4], 255);
    }
}
