//! Image buffers and the CPU compositing primitives. Pixels are RGBA with
//! premultiplied alpha, so blending is one multiply-add per channel.

use rayon::prelude::*;
use std::sync::Mutex;

/// Rows per parallel task: fewer, bigger tasks wake fewer threads for nothing.
pub const ROWS: usize = 24;

/// Big buffers are kept for the next frame. Fresh memory has to be handed over
/// by the system page by page, which costs more than the work done on it.
static SPARE: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());
const SPARE_MOST: usize = 24;
const SPARE_FROM: usize = 1 << 20;

fn spare(len: usize) -> Option<Vec<u8>> {
    if len < SPARE_FROM {
        return None;
    }
    let mut list = SPARE.lock().unwrap();
    let at = list.iter().position(|b| b.len() == len)?;
    Some(list.swap_remove(at))
}

pub struct Frame {
    pub w: usize,
    pub h: usize,
    pub data: Vec<u8>,
}

impl Clone for Frame {
    fn clone(&self) -> Self {
        let data = match spare(self.data.len()) {
            Some(mut b) => {
                b.copy_from_slice(&self.data);
                b
            }
            None => self.data.clone(),
        };
        Self { w: self.w, h: self.h, data }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        if self.data.len() >= SPARE_FROM {
            let mut list = SPARE.lock().unwrap();
            if list.len() >= SPARE_MOST {
                list.remove(0);
            }
            list.push(std::mem::take(&mut self.data));
        }
    }
}

impl Frame {
    pub fn new(w: usize, h: usize) -> Self {
        let data = match spare(w * h * 4) {
            Some(mut b) => {
                b.fill(0);
                b
            }
            None => vec![0; w * h * 4],
        };
        Self { w, h, data }
    }

    /// For a picture about to be overwritten whole: a spare buffer is not
    /// cleared first.
    pub fn for_overwrite(w: usize, h: usize) -> Self {
        let data = spare(w * h * 4).unwrap_or_else(|| vec![0; w * h * 4]);
        Self { w, h, data }
    }

    pub fn solid(w: usize, h: usize, rgb: [u8; 3]) -> Self {
        let mut f = Self::new(w, h);
        for p in f.data.as_chunks_mut::<4>().0 {
            p.copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
        f
    }

    fn stride(&self) -> usize {
        self.w * 4
    }

    /// Premultiplied pixels over black are their own colour, so "flatten" is
    /// only forcing alpha to opaque.
    pub fn make_opaque(&mut self) {
        let stride = self.stride();
        self.data.par_chunks_mut(stride).with_min_len(ROWS).for_each(|row| {
            for p in row.as_chunks_mut::<4>().0 {
                p[3] = 255;
            }
        });
    }

    pub fn copy_from(&mut self, src: &Frame) {
        debug_assert_eq!(self.data.len(), src.data.len());
        self.data.copy_from_slice(&src.data);
    }

    /// `self` = `src` over `self`, with `opacity` 0..=255 applied to `src`.
    pub fn blend_over(&mut self, src: &Frame, opacity: u8) {
        if opacity == 0 {
            return;
        }
        let stride = self.stride();
        self.data
            .par_chunks_mut(stride)
            .zip(src.data.par_chunks(stride))
            .with_min_len(ROWS)
            .for_each(|(d, s)| over_row(d, s, opacity as u32));
    }

    /// `self` = a * (1 - t) + b * t
    pub fn mix(&mut self, a: &Frame, b: &Frame, t: f32) {
        let tb = (t.clamp(0.0, 1.0) * 256.0) as u32;
        let ta = 256 - tb;
        let stride = self.stride();
        self.data
            .par_chunks_mut(stride)
            .zip(a.data.par_chunks(stride).zip(b.data.par_chunks(stride)))
            .for_each(|(d, (ra, rb))| {
                for ((o, &x), &y) in d.iter_mut().zip(ra).zip(rb) {
                    *o = ((x as u32 * ta + y as u32 * tb) >> 8) as u8;
                }
            });
    }

    /// `b` is revealed from the left, behind a moving vertical edge.
    pub fn wipe(&mut self, a: &Frame, b: &Frame, t: f32) {
        let edge = (t.clamp(0.0, 1.0) * self.w as f32) as usize * 4;
        let stride = self.stride();
        self.data
            .par_chunks_mut(stride)
            .zip(a.data.par_chunks(stride).zip(b.data.par_chunks(stride)))
            .for_each(|(d, (ra, rb))| {
                d[..edge].copy_from_slice(&rb[..edge]);
                d[edge..].copy_from_slice(&ra[edge..]);
            });
    }

    /// `b` pushes `a` out to the left.
    pub fn slide(&mut self, a: &Frame, b: &Frame, t: f32) {
        let off = (t.clamp(0.0, 1.0) * self.w as f32) as usize * 4;
        let stride = self.stride();
        self.data
            .par_chunks_mut(stride)
            .zip(a.data.par_chunks(stride).zip(b.data.par_chunks(stride)))
            .for_each(|(d, (ra, rb))| {
                let keep = stride - off;
                d[..keep].copy_from_slice(&ra[off..]);
                d[keep..].copy_from_slice(&rb[..off]);
            });
    }

    /// `b` rolls down over `a` from the top.
    pub fn roller(&mut self, a: &Frame, b: &Frame, t: f32) {
        let edge = (t.clamp(0.0, 1.0) * self.h as f32) as usize;
        let stride = self.stride();
        let split = edge * stride;
        self.data[..split].copy_from_slice(&b.data[..split]);
        self.data[split..].copy_from_slice(&a.data[split..]);
    }

    /// Darkens everything; `level` 1 leaves it, 0 is black.
    pub fn dim(&mut self, level: f32) {
        let k = (level.clamp(0.0, 1.0) * 256.0) as u32;
        let stride = self.stride();
        self.data.par_chunks_mut(stride).for_each(|row| {
            for p in row.as_chunks_mut::<4>().0 {
                for c in &mut p[..3] {
                    *c = ((*c as u32 * k) >> 8) as u8;
                }
            }
        });
    }

    /// Bilinear scaled draw of `src` into the rectangle at (x, y) sized (w, h).
    /// The rectangle may stick out of `self`; it is clipped.
    pub fn blit_scaled(&mut self, src: &Frame, x: i32, y: i32, w: usize, h: usize, opacity: u8) {
        if w == 0 || h == 0 || opacity == 0 || src.w == 0 || src.h == 0 {
            return;
        }
        let (dw, dh) = (self.w as i32, self.h as i32);
        let x0 = x.max(0);
        let x1 = (x + w as i32).min(dw);
        let y0 = y.max(0);
        let y1 = (y + h as i32).min(dh);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let step_x = ((src.w as u64) << 16) / w as u64;
        let step_y = ((src.h as u64) << 16) / h as u64;
        let stride = self.stride();
        let op = opacity as u32;
        self.data.par_chunks_mut(stride).enumerate().skip(y0 as usize).take((y1 - y0) as usize).for_each(
            |(row, d)| {
                let fy = (row as i64 - y as i64) as u64 * step_y;
                let sy = ((fy >> 16) as usize).min(src.h - 1);
                let sy1 = (sy + 1).min(src.h - 1);
                let wy = ((fy >> 8) & 0xff) as u32;
                let r0 = &src.data[sy * src.w * 4..(sy + 1) * src.w * 4];
                let r1 = &src.data[sy1 * src.w * 4..(sy1 + 1) * src.w * 4];
                for px in x0..x1 {
                    let fx = (px as i64 - x as i64) as u64 * step_x;
                    let sx = ((fx >> 16) as usize).min(src.w - 1);
                    let sx1 = (sx + 1).min(src.w - 1);
                    let wx = ((fx >> 8) & 0xff) as u32;
                    let mut s = [0u32; 4];
                    for (c, v) in s.iter_mut().enumerate() {
                        let top = r0[sx * 4 + c] as u32 * (256 - wx) + r0[sx1 * 4 + c] as u32 * wx;
                        let bot = r1[sx * 4 + c] as u32 * (256 - wx) + r1[sx1 * 4 + c] as u32 * wx;
                        *v = (top * (256 - wy) + bot * wy) >> 16;
                    }
                    let o = px as usize * 4;
                    blend_px(&mut d[o..o + 4], s, op);
                }
            },
        );
    }

    /// Small copy for the input cards: 2x2 samples per output pixel. Too small
    /// a job to be worth waking other threads.
    pub fn thumbnail(&self, tw: usize, th: usize) -> Frame {
        let mut out = Frame::new(tw, th);
        let (sw, sh) = (self.w, self.h);
        out.data.chunks_mut(tw * 4).enumerate().for_each(|(y, d)| {
            let sy = (y * sh / th).min(sh - 1);
            let sy1 = (sy + sh / th / 2).min(sh - 1);
            for x in 0..tw {
                let sx = (x * sw / tw).min(sw - 1);
                let sx1 = (sx + sw / tw / 2).min(sw - 1);
                for c in 0..4 {
                    let at = |px: usize, py: usize| self.data[(py * sw + px) * 4 + c] as u32;
                    d[x * 4 + c] = ((at(sx, sy) + at(sx1, sy) + at(sx, sy1) + at(sx1, sy1)) / 4) as u8;
                }
            }
        });
        out
    }

    /// Keyed copy: pixels close to `key` in chroma become transparent.
    /// `tolerance` and `softness` are 0..1.
    pub fn chroma_key(&self, key: [u8; 3], tolerance: f32, softness: f32) -> Frame {
        let mut out = self.clone();
        let (kb, kr) = chroma(key[0] as f32, key[1] as f32, key[2] as f32);
        let lo = tolerance.max(0.0) * 180.0;
        let span = (softness.max(0.01)) * 180.0;
        out.data.par_chunks_mut(self.w * 4).for_each(|row| {
            for p in row.as_chunks_mut::<4>().0 {
                let (cb, cr) = chroma(p[0] as f32, p[1] as f32, p[2] as f32);
                let d = ((cb - kb).powi(2) + (cr - kr).powi(2)).sqrt();
                let a = ((d - lo) / span).clamp(0.0, 1.0);
                if a < 1.0 {
                    for v in p.iter_mut() {
                        *v = (*v as f32 * a) as u8;
                    }
                }
            }
        });
        out
    }
}

pub fn chroma(r: f32, g: f32, b: f32) -> (f32, f32) {
    (-0.169 * r - 0.331 * g + 0.5 * b, 0.5 * r - 0.419 * g - 0.081 * b)
}

#[inline]
pub(crate) fn blend_px(d: &mut [u8], s: [u32; 4], op: u32) {
    let (s0, s1, s2, s3) = if op == 255 {
        (s[0], s[1], s[2], s[3])
    } else {
        (s[0] * op / 255, s[1] * op / 255, s[2] * op / 255, s[3] * op / 255)
    };
    let inv = 255 - s3.min(255);
    d[0] = (s0 + d[0] as u32 * inv / 255).min(255) as u8;
    d[1] = (s1 + d[1] as u32 * inv / 255).min(255) as u8;
    d[2] = (s2 + d[2] as u32 * inv / 255).min(255) as u8;
    d[3] = (s3 + d[3] as u32 * inv / 255).min(255) as u8;
}

#[inline]
fn over_row(d: &mut [u8], s: &[u8], op: u32) {
    for (dp, sp) in d.as_chunks_mut::<4>().0.iter_mut().zip(s.as_chunks::<4>().0) {
        match (sp[3], op) {
            (0, _) => {}
            (255, 255) => dp.copy_from_slice(sp),
            _ => blend_px(dp, [sp[0] as u32, sp[1] as u32, sp[2] as u32, sp[3] as u32], op),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_endpoints_and_midpoint() {
        let a = Frame::solid(4, 2, [0, 0, 0]);
        let b = Frame::solid(4, 2, [200, 100, 50]);
        let mut d = Frame::new(4, 2);
        d.mix(&a, &b, 0.0);
        assert_eq!(&d.data[..3], &[0, 0, 0]);
        d.mix(&a, &b, 1.0);
        assert_eq!(&d.data[..3], &[200, 100, 50]);
        d.mix(&a, &b, 0.5);
        assert_eq!(&d.data[..3], &[100, 50, 25]);
    }

    #[test]
    fn wipe_and_slide_put_b_on_the_expected_side() {
        let a = Frame::solid(8, 1, [10, 0, 0]);
        let b = Frame::solid(8, 1, [20, 0, 0]);
        let mut d = Frame::new(8, 1);
        d.wipe(&a, &b, 0.5);
        assert_eq!(d.data[0], 20);
        assert_eq!(d.data[7 * 4], 10);
        d.slide(&a, &b, 0.25);
        assert_eq!(d.data[0], 10);
        assert_eq!(d.data[7 * 4], 20);
    }

    #[test]
    fn over_respects_alpha_and_opacity() {
        let mut d = Frame::solid(2, 1, [100, 100, 100]);
        let clear = Frame::new(2, 1);
        d.blend_over(&clear, 255);
        assert_eq!(d.data[0], 100);
        let white = Frame::solid(2, 1, [255, 255, 255]);
        d.blend_over(&white, 255);
        assert_eq!(d.data[0], 255);
        let mut e = Frame::solid(2, 1, [0, 0, 0]);
        e.blend_over(&white, 128);
        assert!((e.data[0] as i32 - 128).abs() <= 1);
    }

    #[test]
    fn blit_clips_outside_the_frame() {
        let mut d = Frame::solid(10, 10, [0, 0, 0]);
        let s = Frame::solid(4, 4, [255, 0, 0]);
        d.blit_scaled(&s, 8, 8, 4, 4, 255);
        assert_eq!(d.data[(9 * 10 + 9) * 4], 255);
        assert_eq!(d.data[0], 0);
        d.blit_scaled(&s, 20, 20, 4, 4, 255); // fully outside: no panic
    }

    #[test]
    fn chroma_key_removes_green_keeps_red() {
        let mut f = Frame::solid(2, 1, [0, 255, 0]);
        f.data[4..8].copy_from_slice(&[255, 0, 0, 255]);
        let k = f.chroma_key([0, 255, 0], 0.3, 0.1);
        assert_eq!(k.data[3], 0);
        assert_eq!(k.data[7], 255);
    }
}
