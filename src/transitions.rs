//! Picture transitions between two full frames. Merge and stingers need the
//! layer structure, so the mixer handles those itself.

use crate::frame::Frame;
use crate::model::{Position, TransKind};
use rayon::prelude::*;

fn ease(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

/// Writes into `out` the state of a transition from `a` to `b` at `t` (0..1).
pub fn apply(out: &mut Frame, a: &Frame, b: &Frame, kind: TransKind, t: f32) {
    let t = t.clamp(0.0, 1.0);
    let (w, h) = (out.w as f32, out.h as f32);
    match kind {
        TransKind::Cut => out.copy_from(if t < 1.0 { a } else { b }),
        TransKind::Fade | TransKind::Merge | TransKind::Stinger(_) => out.mix(a, b, t),
        TransKind::AlphaFade => {
            out.data.fill(0);
            out.blend_over(a, ((1.0 - t) * 255.0) as u8);
            out.blend_over(b, (t * 255.0) as u8);
        }
        TransKind::Wipe => out.wipe(a, b, t),
        TransKind::WipeReverse => out.wipe(b, a, 1.0 - t),
        TransKind::VerticalWipe => out.roller(a, b, t),
        TransKind::VerticalWipeReverse => out.roller(b, a, 1.0 - t),
        TransKind::RollerDoor => {
            // The new picture comes down whole, like a shutter.
            out.copy_from(a);
            let y = -h * (1.0 - ease(t));
            out.blit_region(b, [0.0; 4], 0.0, y, w, h, 255, 0.0);
        }
        TransKind::Slide => out.slide(a, b, t),
        TransKind::SlideReverse => out.slide(b, a, 1.0 - t),
        TransKind::VerticalSlide => vertical_slide(out, a, b, t),
        TransKind::VerticalSlideReverse => vertical_slide(out, b, a, 1.0 - t),
        TransKind::Zoom => {
            out.copy_from(a);
            let s = ease(t);
            let (zw, zh) = (w * s, h * s);
            out.blit_region(b, [0.0; 4], (w - zw) / 2.0, (h - zh) / 2.0, zw, zh, 255, 0.0);
        }
        TransKind::Fly => {
            out.copy_from(a);
            let s = ease(t);
            let (zw, zh) = (w * (0.2 + 0.8 * s), h * (0.2 + 0.8 * s));
            let (x, y) = (-zw * (1.0 - s), -zh * (1.0 - s));
            out.blit_region(b, [0.0; 4], x, y, zw, zh, 255, 0.0);
        }
        TransKind::FlyRotate => {
            out.copy_from(a);
            let s = ease(t);
            let p = Position {
                zoom: s.max(0.02),
                rotate: (1.0 - s) * std::f32::consts::TAU,
                ..Position::default()
            };
            out.blend_over(&b.transform(&p, false), 255);
        }
        TransKind::CrossZoom => {
            let s = ease(t);
            let za = Position { zoom: 1.0 + 2.0 * s, ..Position::default() };
            let zb = Position { zoom: 0.33 + 0.67 * s, ..Position::default() };
            out.data.fill(0);
            out.blend_over(&a.transform(&za, false), ((1.0 - s) * 255.0) as u8);
            out.blend_over(&b.transform(&zb, false), (s * 255.0) as u8);
        }
        TransKind::Cube => cube(out, a, b, t, 1.0),
        TransKind::CubeZoom => {
            let z = 1.0 - 0.35 * (t * std::f32::consts::PI).sin();
            cube(out, a, b, t, z);
        }
        TransKind::BarnDoor => {
            // The old picture opens from the middle like two doors.
            out.copy_from(b);
            let s = ease(t);
            let half = (out.w / 2) as f32;
            let shift = half * s;
            out.blit_region(a, [0.0, 0.5, 0.0, 0.0], -shift, 0.0, half, h, 255, 0.0);
            out.blit_region(a, [0.5, 0.0, 0.0, 0.0], half + shift, 0.0, half, h, 255, 0.0);
        }
    }
}

fn vertical_slide(out: &mut Frame, a: &Frame, b: &Frame, t: f32) {
    let rows = (t * out.h as f32) as usize;
    let stride = out.w * 4;
    let keep = (out.h - rows) * stride;
    out.data[..keep].copy_from_slice(&a.data[rows * stride..]);
    out.data[keep..].copy_from_slice(&b.data[..rows * stride]);
}

#[derive(Clone, Copy)]
struct Column {
    /// false shows the outgoing face, true the incoming one.
    incoming: bool,
    u: f32,
    scale: f32,
}

/// Solves, for one screen column, which face of the turning cube is seen
/// there and where on that face. `c` runs from -0.5 to 0.5.
fn cube_column(c: f32, theta: f32) -> Option<Column> {
    const F: f32 = 1.5; // distance from the eye to the front face
    let (s, co) = theta.sin_cos();
    let den_a = F * co + c * s;
    if den_a.abs() > 1e-5 {
        let x = (c * (F + 0.5 - 0.5 * co) + 0.5 * F * s) / den_a;
        if (-0.5..=0.5).contains(&x) {
            let depth = F + 0.5 - x * s - 0.5 * co;
            return Some(Column { incoming: false, u: x + 0.5, scale: F / depth });
        }
    }
    let den_b = F * s - c * co;
    if den_b.abs() > 1e-5 {
        let v = (c * (F + 0.5 - 0.5 * s) - 0.5 * F * co) / den_b;
        if (-0.5..=0.5).contains(&v) {
            let depth = F + 0.5 - 0.5 * s + v * co;
            return Some(Column { incoming: true, u: v + 0.5, scale: F / depth });
        }
    }
    None
}

fn cube(out: &mut Frame, a: &Frame, b: &Frame, t: f32, zoom: f32) {
    let (w, h) = (out.w, out.h);
    let theta = ease(t) * std::f32::consts::FRAC_PI_2;
    let cols: Vec<Option<Column>> = (0..w)
        .map(|x| {
            let c = ((x as f32 + 0.5) / w as f32 - 0.5) / zoom;
            if c.abs() > 0.5 {
                return None;
            }
            cube_column(c, theta)
        })
        .collect();
    out.data.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        let v = (y as f32 + 0.5) / h as f32 - 0.5;
        for (x, col) in cols.iter().enumerate() {
            let d = &mut row[x * 4..x * 4 + 4];
            let Some(c) = col else {
                d.copy_from_slice(&[0, 0, 0, 255]);
                continue;
            };
            let sv = v / (c.scale * zoom) + 0.5;
            if !(0.0..1.0).contains(&sv) {
                d.copy_from_slice(&[0, 0, 0, 255]);
                continue;
            }
            let src = if c.incoming { b } else { a };
            let sx = ((c.u * src.w as f32) as usize).min(src.w - 1);
            let sy = ((sv * src.h as f32) as usize).min(src.h - 1);
            let i = (sy * src.w + sx) * 4;
            d.copy_from_slice(&src.data[i..i + 4]);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames() -> (Frame, Frame) {
        (Frame::solid(64, 36, [200, 0, 0]), Frame::solid(64, 36, [0, 0, 200]))
    }

    /// Every transition starts on A and ends on B.
    #[test]
    fn every_transition_runs_from_a_to_b() {
        let (a, b) = frames();
        let mut out = Frame::new(64, 36);
        let middle = (18 * 64 + 32) * 4;
        for kind in TransKind::ALL {
            apply(&mut out, &a, &b, kind, 1.0);
            assert!(out.data[middle + 2] > 150, "{kind:?} ends on B");
            if kind != TransKind::Cut {
                apply(&mut out, &a, &b, kind, 0.0);
                assert!(out.data[middle] > 150, "{kind:?} starts on A");
            }
            apply(&mut out, &a, &b, kind, 0.5); // must not panic halfway
        }
    }

    #[test]
    fn cube_shows_both_faces_halfway() {
        let (a, b) = frames();
        let mut out = Frame::new(64, 36);
        apply(&mut out, &a, &b, TransKind::Cube, 0.5);
        let row = 18 * 64 * 4;
        assert!(out.data[row + 10 * 4] > 150);
        assert!(out.data[row + 54 * 4 + 2] > 150);
    }

    #[test]
    fn stinger_names_round_trip() {
        assert_eq!(TransKind::from_api("Stinger3"), Some(TransKind::Stinger(2)));
        assert_eq!(TransKind::from_api("cubezoom"), Some(TransKind::CubeZoom));
        assert_eq!(TransKind::from_api("Nope"), None);
    }
}
