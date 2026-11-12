//! Per-input picture processing: position, colour, keying and effects.
//! The mixer calls this only when an input asks for it and only once per new
//! source frame, so an input left untouched costs nothing.

use crate::frame::{Frame, ROWS};
use crate::model::{Colour, Effect, EffectKind, Input, Key, KeyMode, Position};
use rayon::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

#[inline]
fn sample(src: &Frame, x: f32, y: f32) -> [f32; 4] {
    let (w, h) = (src.w as i32, src.h as i32);
    let (x0, y0) = (x.floor(), y.floor());
    let (fx, fy) = (x - x0, y - y0);
    let (ix, iy) = (x0 as i32, y0 as i32);
    let at = |px: i32, py: i32| {
        let (px, py) = (px.clamp(0, w - 1), py.clamp(0, h - 1));
        let i = (py as usize * src.w + px as usize) * 4;
        [src.data[i], src.data[i + 1], src.data[i + 2], src.data[i + 3]]
    };
    let (a, b, c, d) = (at(ix, iy), at(ix + 1, iy), at(ix, iy + 1), at(ix + 1, iy + 1));
    let mut o = [0f32; 4];
    for k in 0..4 {
        let top = a[k] as f32 * (1.0 - fx) + b[k] as f32 * fx;
        let bot = c[k] as f32 * (1.0 - fx) + d[k] as f32 * fx;
        o[k] = top * (1.0 - fy) + bot * fy;
    }
    o
}

#[inline]
fn over(d: &mut [u8], s: [f32; 4], op: f32) {
    let a = s[3] * op / 255.0;
    let inv = 1.0 - a;
    for k in 0..3 {
        d[k] = (s[k] * op + d[k] as f32 * inv).min(255.0) as u8;
    }
    d[3] = (s[3] * op + d[3] as f32 * inv).min(255.0) as u8;
}

impl Frame {
    /// Draws part of `src` (crop: left, right, top, bottom fractions) into a
    /// rectangle, with optional rounded corners. The rectangle may stick out.
    #[allow(clippy::too_many_arguments)]
    pub fn blit_region(
        &mut self,
        src: &Frame,
        crop: [f32; 4],
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        opacity: u8,
        radius: f32,
    ) {
        if w < 1.0 || h < 1.0 || opacity == 0 || src.w == 0 || src.h == 0 {
            return;
        }
        let (x0, x1) = (x.max(0.0) as usize, (x + w).min(self.w as f32).max(0.0) as usize);
        let (y0, y1) = (y.max(0.0) as usize, (y + h).min(self.h as f32).max(0.0) as usize);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let (sw, sh) = (src.w as f32, src.h as f32);
        let (u0, v0) = (crop[0] * sw, crop[2] * sh);
        let (us, vs) = ((1.0 - crop[0] - crop[1]) * sw / w, (1.0 - crop[2] - crop[3]) * sh / h);
        let op = opacity as u32;
        let stride = self.w * 4;
        // Where each column reads is the same on every row: worked out once.
        let columns: Vec<(u32, u32, u32)> = (x0..x1)
            .map(|px| {
                let s = (u0 + (px as f32 + 0.5 - x) * us - 0.5).clamp(0.0, sw - 1.0);
                let i = s as u32;
                let next = (i + 1).min(src.w as u32 - 1);
                (i * 4, next * 4, ((s - i as f32) * 256.0) as u32)
            })
            .collect();
        let src_stride = src.w * 4;
        let rows = self.data[y0 * stride..y1 * stride].par_chunks_mut(stride).with_min_len(ROWS);
        rows.enumerate().for_each(|(n, row)| {
            let py = (y0 + n) as f32 + 0.5;
            let sy = (v0 + (py - y) * vs - 0.5).clamp(0.0, sh - 1.0);
            let iy = sy as usize;
            let wy = ((sy - iy as f32) * 256.0) as u32;
            let top = &src.data[iy * src_stride..(iy + 1) * src_stride];
            let below = (iy + 1).min(src.h - 1);
            let bottom = &src.data[below * src_stride..(below + 1) * src_stride];
            for (px, &(a, b, wx)) in (x0..x1).zip(&columns) {
                let (a, b) = (a as usize, b as usize);
                // Most of a title or a logo is empty.
                if top[a + 3] | top[b + 3] | bottom[a + 3] | bottom[b + 3] == 0 {
                    continue;
                }
                if radius > 0.0 && outside_round(px as f32 + 0.5 - x, py - y, w, h, radius) {
                    continue;
                }
                let mut s = [0u32; 4];
                for (k, v) in s.iter_mut().enumerate() {
                    let up = top[a + k] as u32 * (256 - wx) + top[b + k] as u32 * wx;
                    let down = bottom[a + k] as u32 * (256 - wx) + bottom[b + k] as u32 * wx;
                    *v = (up * (256 - wy) + down * wy) >> 16;
                }
                crate::frame::blend_px(&mut row[px * 4..px * 4 + 4], s, op);
            }
        });
    }

    pub fn fill_round(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, rgb: [u8; 3], opacity: u8) {
        let (x0, x1) = (x.max(0.0) as usize, (x + w).min(self.w as f32).max(0.0) as usize);
        let (y0, y1) = (y.max(0.0) as usize, (y + h).min(self.h as f32).max(0.0) as usize);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let s = [rgb[0] as f32, rgb[1] as f32, rgb[2] as f32, 255.0];
        let op = opacity as f32 / 255.0;
        let stride = self.w * 4;
        self.data[y0 * stride..y1 * stride].par_chunks_mut(stride).enumerate().for_each(|(n, row)| {
            let py = (y0 + n) as f32 + 0.5 - y;
            for px in x0..x1 {
                if radius > 0.0 && outside_round(px as f32 + 0.5 - x, py, w, h, radius) {
                    continue;
                }
                over(&mut row[px * 4..px * 4 + 4], s, op);
            }
        });
    }

    /// Copy of this frame moved, zoomed, turned and cropped in place.
    pub fn transform(&self, p: &Position, mirror: bool) -> Frame {
        let mut out = Frame::new(self.w, self.h);
        let t = match placement(self.w, self.h, p, mirror) {
            Placed::Box { crop, x, y, w, h } => {
                out.blit_region(self, crop, x, y, w, h, 255, 0.0);
                return out;
            }
            Placed::Turned(t) => t,
        };
        let stride = self.w * 4;
        out.data.par_chunks_mut(stride).with_min_len(ROWS).enumerate().for_each(|(y, row)| {
            for x in 0..self.w {
                let mut dx = x as f32 + 0.5 - t.cx - t.pan_x;
                let dy = y as f32 + 0.5 - t.cy + t.pan_y;
                if t.mirror {
                    dx = -dx;
                }
                let sx = t.cx + (dx * t.cos + dy * t.sin) / t.zoom;
                let sy = t.cy + (-dx * t.sin + dy * t.cos) / t.zoom;
                if sx < t.bounds[0] || sx >= t.bounds[1] || sy < t.bounds[2] || sy >= t.bounds[3] {
                    continue;
                }
                let s = sample(self, sx - 0.5, sy - 0.5);
                let d = &mut row[x * 4..x * 4 + 4];
                for k in 0..4 {
                    d[k] = s[k] as u8;
                }
            }
        });
        out
    }

    /// Three box passes approximate a gaussian, in time independent of size.
    pub fn blur(&mut self, radius: usize) {
        for _ in 0..3 {
            self.box_rows(radius);
            let mut t = self.transposed();
            t.box_rows(radius);
            *self = t.transposed();
        }
    }

    fn box_rows(&mut self, radius: usize) {
        let w = self.w;
        let r = radius.min(w.saturating_sub(1));
        let div = (2 * r + 1) as u32;
        self.data.par_chunks_mut(w * 4).for_each(|row| {
            let src = row.to_vec();
            for c in 0..4 {
                let px = |i: i64| src[(i.clamp(0, w as i64 - 1) as usize) * 4 + c] as u32;
                let mut sum: u32 = (-(r as i64)..=r as i64).map(px).sum();
                for x in 0..w {
                    row[x * 4 + c] = (sum / div) as u8;
                    sum += px(x as i64 + r as i64 + 1);
                    sum -= px(x as i64 - r as i64);
                }
            }
        });
    }

    fn transposed(&self) -> Frame {
        let mut out = Frame::new(self.h, self.w);
        let (w, h) = (self.w, self.h);
        out.data.par_chunks_mut(h * 4).enumerate().for_each(|(x, row)| {
            for y in 0..h {
                let i = (y * w + x) * 4;
                row[y * 4..y * 4 + 4].copy_from_slice(&self.data[i..i + 4]);
            }
        });
        out
    }
}

/// True when (x, y) in a w by h box lies beyond its rounded corner.
#[inline]
fn outside_round(x: f32, y: f32, w: f32, h: f32, r: f32) -> bool {
    let dx = (r - x).max(x - (w - r));
    let dy = (r - y).max(y - (h - r));
    dx > 0.0 && dy > 0.0 && dx * dx + dy * dy > r * r
}

/// Where an input's position puts its picture.
pub enum Placed {
    /// Without a turn or a mirror: a plain scaled draw, far cheaper.
    Box {
        crop: [f32; 4],
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    },
    Turned(Turn),
}

/// Every output pixel looks back into the source through this.
pub struct Turn {
    pub cx: f32,
    pub cy: f32,
    pub pan_x: f32,
    pub pan_y: f32,
    pub sin: f32,
    pub cos: f32,
    pub zoom: f32,
    pub mirror: bool,
    /// Left, right, top, bottom edges kept from the source, in pixels.
    pub bounds: [f32; 4],
}

pub fn placement(w: usize, h: usize, p: &Position, mirror: bool) -> Placed {
    let (w, h) = (w as f32, h as f32);
    let (cx, cy) = (w / 2.0, h / 2.0);
    let turns = p.quarter_turns % 4;
    // A quarter turn swaps the sides: shrink to keep the picture inside.
    let fit = if turns % 2 == 1 { (h / w).min(w / h) } else { 1.0 };
    let zoom = (p.zoom * fit).max(0.001);
    let angle = p.rotate + turns as f32 * std::f32::consts::FRAC_PI_2;
    let (sin, cos) = angle.sin_cos();
    let (pan_x, pan_y) = (p.pan_x * cx, p.pan_y * cy);
    let (l, r) = (p.crop[0] * w, w - p.crop[1] * w);
    let (t, b) = (p.crop[2] * h, h - p.crop[3] * h);
    if angle == 0.0 && !mirror {
        let (x, y) = (cx + pan_x + (l - cx) * zoom, cy - pan_y + (t - cy) * zoom);
        return Placed::Box { crop: p.crop, x, y, w: (r - l) * zoom, h: (b - t) * zoom };
    }
    Placed::Turned(Turn { cx, cy, pan_x, pan_y, sin, cos, zoom, mirror, bounds: [l, r, t, b] })
}

/// Applies an input's settings to a source frame, in the order a signal
/// passes through them: geometry, colour, key, then effects.
pub fn process(src: &Frame, input: &Input, fill: Option<&Frame>) -> Frame {
    let mut f = if !input.position.is_identity() || input.mirror {
        src.transform(&input.position, input.mirror)
    } else {
        src.clone()
    };
    if !input.colour.is_identity() {
        apply_colour(&mut f, &input.colour);
    }
    apply_key(&mut f, &input.key, fill);
    for e in input.effects.iter().filter(|e| e.enabled) {
        apply_effect(&mut f, e);
    }
    f
}

/// One lookup per channel holds everything that treats channels separately.
pub fn colour_table(c: &Colour, ch: usize) -> [u8; 256] {
    let mut t = [0u8; 256];
    let span = (c.white - c.black).max(0.001);
    for (i, o) in t.iter_mut().enumerate() {
        let mut v = i as f32 / 255.0;
        v = ((v - c.black) / span) * c.rgb[ch];
        v = v * c.gain[ch] + c.lift[ch] * (1.0 - v);
        v = v.clamp(0.0, 1.0).powf(1.0 / c.gamma[ch].max(0.01));
        *o = (v * 255.0).round().clamp(0.0, 255.0) as u8;
    }
    t
}

/// Saturation and hue as one matrix, turning around the grey axis.
pub fn colour_matrix(sat: f32, hue: f32) -> [[f32; 3]; 3] {
    const L: [f32; 3] = [0.299, 0.587, 0.114];
    let (s, c) = (hue.to_radians().sin(), hue.to_radians().cos());
    let k = 1.0 / 3f32.sqrt();
    let third = (1.0 - c) / 3.0;
    let hue_m = [
        [c + third, third - k * s, third + k * s],
        [third + k * s, c + third, third - k * s],
        [third - k * s, third + k * s, c + third],
    ];
    let mut m = [[0f32; 3]; 3];
    for r in 0..3 {
        for col in 0..3 {
            m[r][col] = L[col] * (1.0 - sat) + if r == col { sat } else { 0.0 };
        }
    }
    // Saturate then rotate hue.
    let mut o = [[0f32; 3]; 3];
    for r in 0..3 {
        for col in 0..3 {
            o[r][col] = (0..3).map(|n| hue_m[r][n] * m[n][col]).sum();
        }
    }
    o
}

fn apply_colour(f: &mut Frame, c: &Colour) {
    let tables = [colour_table(c, 0), colour_table(c, 1), colour_table(c, 2)];
    let matrix = (c.saturation != 1.0 || c.hue != 0.0).then(|| colour_matrix(c.saturation, c.hue));
    let alpha = c.alpha.clamp(0.0, 1.0);
    let stride = f.w * 4;
    f.data.par_chunks_mut(stride).with_min_len(ROWS).for_each(|row| {
        for p in row.as_chunks_mut::<4>().0 {
            // Nothing there to correct: a lift must not tint transparent areas.
            if p[3] == 0 {
                continue;
            }
            let (r, g, b) = (tables[0][p[0] as usize], tables[1][p[1] as usize], tables[2][p[2] as usize]);
            if let Some(m) = &matrix {
                let v = [r as f32, g as f32, b as f32];
                for k in 0..3 {
                    p[k] = (m[k][0] * v[0] + m[k][1] * v[1] + m[k][2] * v[2]).clamp(0.0, 255.0) as u8;
                }
            } else {
                p[0] = r;
                p[1] = g;
                p[2] = b;
            }
            for k in 0..3 {
                p[k] = p[k].min(p[3]);
            }
            if alpha < 1.0 {
                for v in p.iter_mut() {
                    *v = (*v as f32 * alpha) as u8;
                }
            }
        }
    });
}

fn scale_px(p: &mut [u8], a: f32) {
    for v in p.iter_mut() {
        *v = (*v as f32 * a) as u8;
    }
}

fn apply_key(f: &mut Frame, key: &Key, fill: Option<&Frame>) {
    let stride = f.w * 4;
    match key.mode {
        KeyMode::Off => {}
        KeyMode::Chroma => {
            let keyed = f.chroma_key(key.color, key.tolerance, key.softness);
            *f = keyed;
            remove_spill(f, key);
        }
        KeyMode::Colour => {
            let k = [key.color[0] as f32, key.color[1] as f32, key.color[2] as f32];
            let lo = key.tolerance * 441.0;
            let span = key.softness.max(0.002) * 441.0;
            f.data.par_chunks_mut(stride).for_each(|row| {
                for p in row.as_chunks_mut::<4>().0 {
                    let d: f32 = (0..3).map(|i| (p[i] as f32 - k[i]).powi(2)).sum::<f32>().sqrt();
                    let a = ((d - lo) / span).clamp(0.0, 1.0);
                    if a < 1.0 {
                        scale_px(p, a);
                    }
                }
            });
        }
        KeyMode::Luma => {
            let lo = key.tolerance * 255.0;
            let span = key.softness.max(0.002) * 255.0;
            f.data.par_chunks_mut(stride).for_each(|row| {
                for p in row.as_chunks_mut::<4>().0 {
                    let y = 0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32;
                    let a = ((y - lo) / span).clamp(0.0, 1.0);
                    if a < 1.0 {
                        scale_px(p, a);
                    }
                }
            });
        }
        KeyMode::Fill => {
            let Some(mask) = fill.filter(|m| m.w == f.w && m.h == f.h) else { return };
            f.data.par_chunks_mut(stride).zip(mask.data.par_chunks(stride)).for_each(|(row, m)| {
                for (p, q) in row.as_chunks_mut::<4>().0.iter_mut().zip(m.as_chunks::<4>().0) {
                    let y = 0.299 * q[0] as f32 + 0.587 * q[1] as f32 + 0.114 * q[2] as f32;
                    scale_px(p, y / 255.0);
                }
            });
        }
    }
}

/// The channel a key colour is made of, pulled down on what remains.
pub fn spill_channel(key: &Key) -> usize {
    (0..3).max_by_key(|&i| key.color[i]).unwrap_or(1)
}

/// Pulls the key colour's dominant channel down to its neighbours on what
/// remains, which removes the green or blue halo around people.
fn remove_spill(f: &mut Frame, key: &Key) {
    let dominant = spill_channel(key);
    let (a, b) = ((dominant + 1) % 3, (dominant + 2) % 3);
    let amount = key.spill.clamp(0.0, 1.0);
    let stride = f.w * 4;
    f.data.par_chunks_mut(stride).for_each(|row| {
        for p in row.as_chunks_mut::<4>().0 {
            let limit = p[a].max(p[b]) as f32;
            let v = p[dominant] as f32;
            if v > limit {
                p[dominant] = (v - (v - limit) * amount) as u8;
            }
        }
    });
}

/// The pixels an effect's crop leaves, as column and row ranges.
pub fn crop_bounds(fw: usize, fh: usize, crop: [f32; 4]) -> (usize, usize, usize, usize) {
    let (w, h) = (fw as f32, fh as f32);
    let x0 = (crop[0] * w) as usize;
    let x1 = ((1.0 - crop[1]) * w) as usize;
    let y0 = (crop[2] * h) as usize;
    let y1 = ((1.0 - crop[3]) * h) as usize;
    let (x0, y0) = (x0.min(fw), y0.min(fh));
    // Crops that meet or cross leave nothing, not a backwards range.
    (x0, x1.min(fw).max(x0), y0, y1.min(fh).max(y0))
}

/// Puts `done` back into `f`, but only inside the effect's crop.
fn merge_in_crop(f: &mut Frame, done: &Frame, crop: [f32; 4]) {
    if crop == [0.0; 4] {
        f.copy_from(done);
        return;
    }
    let (x0, x1, y0, y1) = crop_bounds(f.w, f.h, crop);
    for y in y0..y1 {
        let i = (y * f.w + x0) * 4;
        let j = (y * f.w + x1) * 4;
        f.data[i..j].copy_from_slice(&done.data[i..j]);
    }
}

/// Runs `work` on the picture, straight on it unless a crop limits the effect.
fn within_crop(f: &mut Frame, crop: [f32; 4], work: impl FnOnce(&mut Frame)) {
    if crop == [0.0; 4] {
        work(f);
        return;
    }
    let mut out = f.clone();
    work(&mut out);
    merge_in_crop(f, &out, crop);
}

fn apply_effect(f: &mut Frame, e: &Effect) {
    match e.kind {
        EffectKind::Blur => {
            let radius = (e.strength.clamp(0.0, 1.0) * f.h as f32 / 24.0) as usize;
            if radius > 0 {
                // A blur loses detail anyway, so it can run on a smaller copy.
                let step = (radius / 2).clamp(1, 8);
                let (sw, sh) = ((f.w / step).max(1), (f.h / step).max(1));
                let mut small = Frame::new(sw, sh);
                small.blit_region(f, [0.0; 4], 0.0, 0.0, sw as f32, sh as f32, 255, 0.0);
                small.blur((radius / step).max(1));
                let mut b = Frame::new(f.w, f.h);
                b.blit_region(&small, [0.0; 4], 0.0, 0.0, f.w as f32, f.h as f32, 255, 0.0);
                merge_in_crop(f, &b, e.crop);
            }
        }
        EffectKind::Sharpen => {
            let mut soft = f.clone();
            soft.blur(1);
            let k = e.strength.clamp(0.0, 1.0) * 2.0;
            let mut out = f.clone();
            for (o, s) in out.data.iter_mut().zip(&soft.data) {
                *o = (*o as f32 + (*o as f32 - *s as f32) * k).clamp(0.0, 255.0) as u8;
            }
            merge_in_crop(f, &out, e.crop);
        }
        EffectKind::Lut => {
            let Some(lut) = load_lut(&e.path) else { return };
            let mix = (e.strength.clamp(0.0, 1.0) * 256.0) as u32;
            within_crop(f, e.crop, |out| {
                out.data.par_chunks_mut(out.w * 4).with_min_len(ROWS).for_each(|row| {
                    for p in row.as_chunks_mut::<4>().0 {
                        if p[3] == 0 {
                            continue;
                        }
                        let c = lut.look([p[0], p[1], p[2]]);
                        if mix == 256 && p[3] == 255 {
                            p[..3].copy_from_slice(&c);
                            continue;
                        }
                        for k in 0..3 {
                            let v = (p[k] as u32 * (256 - mix) + c[k] as u32 * mix) >> 8;
                            p[k] = (v as u8).min(p[3]);
                        }
                    }
                });
            });
        }
        EffectKind::AlphaMask => {
            let Some(mask) = load_mask(&e.path, f.w, f.h) else { return };
            within_crop(f, e.crop, |out| {
                let w = out.w;
                out.data.par_chunks_mut(w * 4).zip(mask.par_chunks(w)).for_each(|(row, m)| {
                    for (p, a) in row.as_chunks_mut::<4>().0.iter_mut().zip(m) {
                        scale_px(p, *a as f32 / 255.0);
                    }
                });
            });
        }
    }
}

/// Which way a colour leaves its cell, from how the three fractions compare.
/// Bit 0: red above green, bit 1: green above blue, bit 2: red above blue.
/// Gives the channels from largest fraction to smallest.
const ORDER: [[usize; 3]; 8] = [
    [2, 1, 0], // none: blue, green, red
    [2, 0, 1], // red over green only
    [1, 2, 0], // green over blue only
    [0, 1, 2], // impossible, kept as a filler
    [0, 1, 2], // impossible, kept as a filler
    [0, 2, 1], // red over green and blue
    [1, 0, 2], // green over blue, red over blue
    [0, 1, 2], // all three in order
];

pub struct Lut {
    /// Three colours in 16-bit lanes of one number: blending a corner is then
    /// one multiplication instead of three.
    nodes: Vec<u64>,
    /// Per channel and input value: table offset of the cell, and how far
    /// along it the value sits, from 0 to 256.
    cells: [[(u32, u32); 256]; 3],
    /// Step from a node to the next along each channel.
    steps: [u32; 3],
}

impl Lut {
    /// Colour grades are smooth, so a big table loses nothing by being reduced
    /// to one that stays in the processor's fast memory: that alone is the
    /// difference between a cheap lookup and one waiting on memory all frame.
    fn new(size: usize, table: Vec<[f32; 3]>) -> Self {
        const MOST: usize = 33;
        let (kept, table) = if size <= MOST { (size, table) } else { (MOST, reduced(size, &table, MOST)) };
        let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u64;
        let nodes = table.iter().map(|c| byte(c[0]) | byte(c[1]) << 16 | byte(c[2]) << 32).collect();
        let steps = [1, kept as u32, (kept * kept) as u32];
        let n = kept as u32 - 1;
        let mut cells = [[(0, 0); 256]; 3];
        for (channel, list) in cells.iter_mut().enumerate() {
            for (v, cell) in list.iter_mut().enumerate() {
                let p = v as u32 * n;
                let i = (p / 255).min(n - 1);
                *cell = (i * steps[channel], ((p - i * 255) * 256 + 127) / 255);
            }
        }
        Self { nodes, cells, steps }
    }

    /// The table as the graphics card reads it, so both look up alike: per
    /// channel and value its cell (offset, and fraction above bit 20), then
    /// the three steps, then each node as red, green and blue bytes.
    pub fn packed(&self) -> Vec<u32> {
        let cells = self.cells.iter().flatten().map(|&(at, frac)| at | frac << 20);
        let nodes =
            self.nodes.iter().map(|n| (n & 0xff | (n >> 16 & 0xff) << 8 | (n >> 32 & 0xff) << 16) as u32);
        cells.chain(self.steps).chain(nodes).collect()
    }

    /// Tetrahedral interpolation without a single branch: this runs on every
    /// pixel of every frame, and pictures make branches unpredictable.
    #[inline]
    fn look(&self, c: [u8; 3]) -> [u8; 3] {
        let (r, g, b) =
            (self.cells[0][c[0] as usize], self.cells[1][c[1] as usize], self.cells[2][c[2] as usize]);
        let f = [r.1, g.1, b.1];
        let way = (f[0] > f[1]) as usize | ((f[1] > f[2]) as usize) << 1 | ((f[0] > f[2]) as usize) << 2;
        let [x, y, z] = ORDER[way];
        let (a, b2, c2) = (f[x], f[y], f[z]);
        let base = (r.0 + g.0 + b.0) as usize;
        let near = base + self.steps[x] as usize;
        let next = near + self.steps[y] as usize;
        let far = next + self.steps[z] as usize;
        let sum = self.nodes[base] * (256 - a) as u64
            + self.nodes[near] * (a - b2) as u64
            + self.nodes[next] * (b2 - c2) as u64
            + self.nodes[far] * c2 as u64;
        [(sum >> 8) as u8, (sum >> 24) as u8, (sum >> 40) as u8]
    }
}

/// The same cube on a coarser grid.
fn reduced(size: usize, table: &[[f32; 3]], to: usize) -> Vec<[f32; 3]> {
    let step = (size - 1) as f32 / (to - 1) as f32;
    let at = |n: usize| {
        let p = n as f32 * step;
        let i = (p as usize).min(size - 2);
        (i, p - i as f32)
    };
    let mut out = Vec::with_capacity(to * to * to);
    for b in 0..to {
        for g in 0..to {
            for r in 0..to {
                let ((ir, fr), (ig, fg), (ib, fb)) = (at(r), at(g), at(b));
                let mut o = [0f32; 3];
                for corner in 0..8usize {
                    let (dr, dg, db) = (corner & 1, corner >> 1 & 1, corner >> 2);
                    let weight = (if dr == 1 { fr } else { 1.0 - fr })
                        * (if dg == 1 { fg } else { 1.0 - fg })
                        * (if db == 1 { fb } else { 1.0 - fb });
                    let v = table[((ib + db) * size + ig + dg) * size + ir + dr];
                    for k in 0..3 {
                        o[k] += v[k] * weight;
                    }
                }
                out.push(o);
            }
        }
    }
    out
}

pub fn parse_cube(text: &str) -> Option<Lut> {
    let mut size = 0;
    let mut table = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(v) = line.strip_prefix("LUT_3D_SIZE") {
            size = v.trim().parse().ok()?;
            continue;
        }
        let mut parts = line.split_whitespace().map(str::parse::<f32>);
        if let (Some(Ok(r)), Some(Ok(g)), Some(Ok(b))) = (parts.next(), parts.next(), parts.next()) {
            table.push([r, g, b]);
        }
    }
    (size >= 2 && table.len() == size * size * size).then(|| Lut::new(size, table))
}

/// A PNG holding a cube: 33 tiles side by side, or a 512x512 grid of 8x8.
fn parse_png_lut(path: &str) -> Option<Lut> {
    let img = image::open(path).ok()?.to_rgb8();
    let n = img.height();
    if img.width() == n * n {
        let mut table = Vec::with_capacity((n * n * n) as usize);
        for b in 0..n {
            for g in 0..n {
                for r in 0..n {
                    let p = img.get_pixel(b * n + r, g);
                    table.push([p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0]);
                }
            }
        }
        return Some(Lut::new(n as usize, table));
    }
    if img.width() != 512 || img.height() != 512 {
        return None;
    }
    let mut table = Vec::with_capacity(64 * 64 * 64);
    for b in 0..64u32 {
        for g in 0..64u32 {
            for r in 0..64u32 {
                let p = img.get_pixel((b % 8) * 64 + r, (b / 8) * 64 + g);
                table.push([p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0]);
            }
        }
    }
    Some(Lut::new(64, table))
}

/// A file's last change: a file edited or replaced on disk is read again, and
/// one that was missing is picked up as soon as it appears.
fn changed_at(path: &str) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

type Stamped<T> = (std::time::SystemTime, Option<Arc<T>>);

pub fn load_lut(path: &str) -> Option<Arc<Lut>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Stamped<Lut>>>> = OnceLock::new();
    let stamp = changed_at(path)?;
    let mut map = CACHE.get_or_init(Default::default).lock().unwrap();
    if let Some((at, lut)) = map.get(path)
        && *at == stamp
    {
        return lut.clone();
    }
    let lut = if path.to_lowercase().ends_with(".png") {
        parse_png_lut(path)
    } else {
        std::fs::read_to_string(path).ok().and_then(|t| parse_cube(&t))
    };
    let lut = lut.map(Arc::new);
    map.insert(path.to_string(), (stamp, lut.clone()));
    lut
}

fn read_mask(path: &str, w: usize, h: usize) -> Option<Vec<u8>> {
    let img = image::open(path).ok()?.to_rgba8();
    let (iw, ih) = (img.width() as usize, img.height() as usize);
    let mut m = vec![0u8; w * h];
    for y in 0..h {
        for x in 0..w {
            let p = img.get_pixel((x * iw / w) as u32, (y * ih / h) as u32);
            m[y * w + x] = p[3];
        }
    }
    Some(m)
}

pub fn load_mask(path: &str, w: usize, h: usize) -> Option<Arc<Vec<u8>>> {
    type Masks = HashMap<(String, usize, usize), Stamped<Vec<u8>>>;
    static CACHE: OnceLock<Mutex<Masks>> = OnceLock::new();
    let stamp = changed_at(path)?;
    let mut map = CACHE.get_or_init(Default::default).lock().unwrap();
    let key = (path.to_string(), w, h);
    if let Some((at, mask)) = map.get(&key)
        && *at == stamp
    {
        return mask.clone();
    }
    let mask = read_mask(path, w, h).map(Arc::new);
    map.insert(key, (stamp, mask.clone()));
    mask
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossing_crops_are_empty_and_harmless() {
        let f = Frame::solid(16, 8, [9, 9, 9]);
        let (x0, x1, y0, y1) = crop_bounds(f.w, f.h, [0.8, 0.8, 0.9, 0.9]);
        assert!(x1 >= x0 && y1 >= y0);
        let mut g = f.clone();
        merge_in_crop(&mut g, &Frame::solid(16, 8, [1, 1, 1]), [0.8, 0.8, 0.9, 0.9]);
        assert_eq!(g.data, f.data);
    }

    #[test]
    fn colour_leaves_transparent_areas_alone() {
        let mut f = Frame::new(4, 4);
        let c = Colour { lift: [0.5; 3], ..Colour::default() };
        apply_colour(&mut f, &c);
        assert!(f.data.iter().all(|v| *v == 0));
    }

    #[test]
    fn a_lut_file_that_appears_later_is_used() {
        let dir = std::env::temp_dir().join(format!("livecraft-lut-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("late.cube");
        let name = path.to_string_lossy().into_owned();
        assert!(load_lut(&name).is_none());
        std::fs::write(&path, "LUT_3D_SIZE 2\n0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n")
            .unwrap();
        assert!(load_lut(&name).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn identity_transform_keeps_the_picture() {
        let f = Frame::solid(16, 9, [10, 200, 30]);
        let t = f.transform(&Position::default(), false);
        assert_eq!(t.data[(4 * 16 + 8) * 4 + 1], 200);
    }

    #[test]
    fn zoom_out_leaves_transparent_edges() {
        let f = Frame::solid(32, 18, [255, 0, 0]);
        let t = f.transform(&Position { zoom: 0.5, ..Position::default() }, false);
        assert_eq!(t.data[3], 0);
        assert_eq!(t.data[(9 * 32 + 16) * 4 + 3], 255);
    }

    #[test]
    fn crop_removes_a_side() {
        let f = Frame::solid(32, 18, [255, 0, 0]);
        let p = Position { crop: [0.5, 0.0, 0.0, 0.0], ..Position::default() };
        let t = f.transform(&p, false);
        assert_eq!(t.data[(9 * 32 + 2) * 4 + 3], 0);
        assert_eq!(t.data[(9 * 32 + 28) * 4 + 3], 255);
    }

    #[test]
    fn mirror_swaps_left_and_right() {
        let mut f = Frame::solid(10, 2, [0, 0, 0]);
        f.data[0] = 250;
        f.data[10 * 4] = 250;
        let t = f.transform(&Position::default(), true);
        assert!(t.data[9 * 4] > 150);
    }

    #[test]
    fn zero_saturation_is_grey() {
        let mut f = Frame::solid(2, 2, [200, 40, 40]);
        apply_colour(&mut f, &Colour { saturation: 0.0, ..Colour::default() });
        assert!((f.data[0] as i32 - f.data[1] as i32).abs() <= 2);
    }

    #[test]
    fn hue_turn_of_120_moves_red_to_green() {
        let mut f = Frame::solid(2, 2, [200, 0, 0]);
        apply_colour(&mut f, &Colour { hue: 120.0, ..Colour::default() });
        assert!(f.data[1] > 150 && f.data[0] < 50);
    }

    #[test]
    fn white_stretch_brightens() {
        let mut f = Frame::solid(2, 2, [100, 100, 100]);
        apply_colour(&mut f, &Colour { white: 0.5, ..Colour::default() });
        assert!(f.data[0] > 180);
    }

    #[test]
    fn luma_key_clears_black_keeps_white() {
        let mut f = Frame::solid(2, 1, [0, 0, 0]);
        f.data[4..8].copy_from_slice(&[255, 255, 255, 255]);
        let key = Key { mode: KeyMode::Luma, tolerance: 0.2, softness: 0.1, ..Key::default() };
        apply_key(&mut f, &key, None);
        assert_eq!(f.data[3], 0);
        assert_eq!(f.data[7], 255);
    }

    #[test]
    fn blur_spreads_a_bright_pixel() {
        let mut f = Frame::solid(21, 21, [0, 0, 0]);
        let c = (10 * 21 + 10) * 4;
        f.data[c] = 255;
        f.blur(2);
        assert!(f.data[c] < 255);
        assert!(f.data[(10 * 21 + 12) * 4] > 0);
    }

    #[test]
    fn cube_lut_is_read_and_inverts() {
        let text = "LUT_3D_SIZE 2\n1 1 1\n0 1 1\n1 0 1\n0 0 1\n1 1 0\n0 1 0\n1 0 0\n0 0 0\n";
        let lut = parse_cube(text).unwrap();
        let o = lut.look([255, 0, 0]);
        assert!(o[0] < 2 && o[1] > 250);
    }

    /// The fast lookup must not move colours: an identity table gives back
    /// what it was given, at any size, in every order of the three channels.
    #[test]
    fn an_identity_lut_changes_nothing() {
        for size in [2usize, 17, 33, 64] {
            let n = (size - 1) as f32;
            let mut table = Vec::new();
            for b in 0..size {
                for g in 0..size {
                    for r in 0..size {
                        table.push([r as f32 / n, g as f32 / n, b as f32 / n]);
                    }
                }
            }
            let lut = Lut::new(size, table);
            let colours = [
                [0, 0, 0],
                [255, 255, 255],
                [10, 200, 90],
                [200, 10, 90],
                [90, 10, 200],
                [90, 200, 10],
                [10, 90, 200],
                [200, 90, 10],
                [128, 128, 128],
            ];
            for c in colours {
                let o = lut.look(c);
                for k in 0..3 {
                    assert!(o[k].abs_diff(c[k]) <= 1, "{size} {c:?} {o:?}");
                }
            }
        }
    }

    #[test]
    fn region_blit_rounds_corners() {
        let mut d = Frame::solid(20, 20, [0, 0, 0]);
        let s = Frame::solid(4, 4, [255, 0, 0]);
        d.blit_region(&s, [0.0; 4], 0.0, 0.0, 20.0, 20.0, 255, 6.0);
        assert_eq!(d.data[0], 0);
        assert_eq!(d.data[(10 * 20 + 10) * 4], 255);
    }
}
