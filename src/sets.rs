//! Virtual sets: a backdrop, presenters standing in it and a desk in front,
//! seen through a camera that glides between presets. The decors offered here
//! are drawn by code, so no picture has to ship with the program, and they
//! are drawn at whatever size is asked, so a card and the output agree.

use crate::frame::Frame;
use crate::model::*;

/// Closer than this, a 1080p backdrop is mostly blur.
pub const MAX_ZOOM: f32 = 4.0;

/// Where a new presenter stands: centred, the lower body behind the desk,
/// with later ones to either side.
pub fn presenter_spot(n: usize) -> Placement {
    let shift = [0.0, -0.2, 0.2][n % 3];
    Placement { x: 0.2 + shift, y: 0.16, scale: 0.6 }
}

pub fn new_set(look: SetLook) -> Kind {
    Kind::VirtualSet {
        look,
        background: None,
        foreground: None,
        cameras: default_cameras(),
        camera: 0,
        move_ms: 1200,
    }
}

/// Framed for the built-in decors and a first presenter where
/// [`presenter_spot`] puts it.
pub fn default_cameras() -> Vec<SetCamera> {
    let cam = |name: &str, zoom, x, y| SetCamera { name: name.into(), view: SetView { zoom, x, y } };
    vec![
        cam("Plan large", 1.0, 0.5, 0.5),
        cam("Plan moyen", 1.5, 0.5, 0.42),
        cam("Gros plan", 2.4, 0.5, 0.34),
        cam("Écran", 2.0, 0.73, 0.31),
    ]
}

/// Where the camera of a set rests: on its chosen preset.
pub fn resting_view(kind: &Kind) -> Option<SetView> {
    let Kind::VirtualSet { cameras, camera, .. } = kind else { return None };
    Some(cameras.get(*camera).map_or(SetView::default(), |c| c.view).clamped())
}

impl SetView {
    /// Keeps the view inside the set, so its edges never show.
    pub fn clamped(self) -> SetView {
        let zoom = if self.zoom.is_finite() { self.zoom.clamp(1.0, MAX_ZOOM) } else { 1.0 };
        let half = 0.5 / zoom;
        let inside = |v: f32| if v.is_finite() { v.clamp(half, 1.0 - half) } else { 0.5 };
        SetView { zoom, x: inside(self.x), y: inside(self.y) }
    }

    /// Part way from `self` to `to`, starting and stopping gently as a camera
    /// operator would. Zoom changes by equal ratios, so that going in and
    /// coming back out feel the same.
    pub fn toward(self, to: SetView, t: f32) -> SetView {
        let (a, b) = (self.clamped(), to.clamped());
        let t = t.clamp(0.0, 1.0);
        let e = t * t * (3.0 - 2.0 * t);
        let zoom = a.zoom * (b.zoom / a.zoom).powf(e);
        SetView { zoom, x: a.x + (b.x - a.x) * e, y: a.y + (b.y - a.y) * e }.clamped()
    }

    /// Where something placed in the set lands in the picture.
    pub fn place(self, p: Placement) -> Placement {
        let v = self.clamped();
        let (left, top) = (v.x - 0.5 / v.zoom, v.y - 0.5 / v.zoom);
        Placement { x: (p.x - left) * v.zoom, y: (p.y - top) * v.zoom, scale: p.scale * v.zoom }
    }
}

/// The built-in decor behind the presenters.
pub fn backdrop(look: SetLook, w: usize, h: usize) -> Option<Frame> {
    let mut f = Frame::new(w, h);
    match look {
        SetLook::Studio => {
            gradient(&mut f, (0.0, 0.0, 1.0, 0.64), [12, 20, 44], [30, 58, 104]);
            for k in 1..6 {
                gradient(
                    &mut f,
                    (k as f32 / 6.0, 0.0, k as f32 / 6.0 + 0.002, 0.64),
                    [10, 16, 36],
                    [16, 30, 60],
                );
            }
            gradient(&mut f, (0.0, 0.64, 1.0, 1.0), [44, 50, 66], [14, 16, 24]);
            gradient(&mut f, (0.0, 0.638, 1.0, 0.644), [90, 124, 176], [90, 124, 176]);
            for x in [0.07, 0.32, 0.93] {
                glow(&mut f, x, 0.03, (0.0, 0.64), [80, 170, 235], 0.9);
                glow(&mut f, x, 0.05, (0.64, 1.0), [80, 170, 235], 0.25);
            }
            // A wall screen, framed on the right, for the "Écran" camera.
            gradient(&mut f, (0.55, 0.10, 0.91, 0.52), [8, 10, 18], [8, 10, 18]);
            gradient(&mut f, (0.56, 0.115, 0.90, 0.505), [26, 76, 150], [10, 30, 72]);
            for k in 0..6 {
                let y = 0.16 + k as f32 * 0.06;
                gradient(&mut f, (0.58, y, 0.88, y + 0.003), [40, 100, 180], [40, 100, 180]);
            }
        }
        SetLook::Evening => {
            gradient(&mut f, (0.0, 0.0, 1.0, 0.66), [20, 14, 34], [44, 28, 60]);
            gradient(&mut f, (0.05, 0.07, 0.95, 0.60), [8, 6, 14], [8, 6, 14]);
            gradient(&mut f, (0.06, 0.08, 0.94, 0.59), [16, 22, 58], [96, 52, 92]);
            skyline(&mut f);
            for k in 1..4 {
                let x = 0.06 + 0.88 * k as f32 / 4.0;
                gradient(&mut f, (x - 0.003, 0.08, x + 0.003, 0.59), [8, 6, 14], [8, 6, 14]);
            }
            gradient(&mut f, (0.0, 0.66, 1.0, 1.0), [70, 44, 30], [26, 16, 12]);
            for x in [0.02, 0.98] {
                glow(&mut f, x, 0.05, (0.0, 1.0), [255, 170, 90], 0.5);
            }
        }
        SetLook::Empty => return None,
    }
    Some(f)
}

/// The built-in desk, in front of the presenters; clear everywhere else.
pub fn desk(look: SetLook, w: usize, h: usize) -> Option<Frame> {
    let (top, front, stripe) = match look {
        SetLook::Studio => ([226, 230, 238], ([36, 48, 82], [18, 24, 44]), [240, 120, 40]),
        SetLook::Evening => ([168, 116, 70], ([86, 52, 34], [40, 24, 16]), [214, 170, 80]),
        SetLook::Empty => return None,
    };
    let mut f = Frame::new(w, h);
    let (fw, fh) = (w as f32, h as f32);
    gradient(&mut f, (0.20, 0.735, 0.80, 0.97), front.0, front.1);
    gradient(&mut f, (0.20, 0.79, 0.80, 0.798), stripe, stripe);
    f.fill_round(0.18 * fw, 0.70 * fh, 0.64 * fw, 0.035 * fh, 0.01 * fw, top, 255);
    Some(f)
}

/// Fills a rectangle, given in fractions of the frame, with an opaque
/// vertical gradient.
fn gradient(f: &mut Frame, (x0, y0, x1, y1): (f32, f32, f32, f32), top: [u8; 3], bottom: [u8; 3]) {
    let (fw, fh) = (f.w as f32, f.h as f32);
    let px = |v: f32, size: f32| (v * size).round().clamp(0.0, size) as usize;
    let (c0, c1) = (px(x0, fw), px(x1, fw).max(px(x0, fw) + 1).min(f.w));
    let (r0, r1) = (px(y0, fh), px(y1, fh).max(px(y0, fh) + 1).min(f.h));
    let rows = (r1 - r0).max(1) as f32;
    for row in r0..r1 {
        let t = (row - r0) as f32 / rows;
        let c = [0, 1, 2].map(|k| (top[k] as f32 + (bottom[k] as f32 - top[k] as f32) * t) as u8);
        for p in f.data[(row * f.w + c0) * 4..(row * f.w + c1) * 4].as_chunks_mut::<4>().0 {
            *p = [c[0], c[1], c[2], 255];
        }
    }
}

/// A soft vertical band of light centred on `x`, fading out over `half`.
fn glow(f: &mut Frame, x: f32, half: f32, (y0, y1): (f32, f32), rgb: [u8; 3], strength: f32) {
    let (fw, fh) = (f.w as f32, f.h as f32);
    let c0 = ((x - half) * fw).max(0.0) as usize;
    let c1 = (((x + half) * fw).ceil() as usize).min(f.w);
    let (r0, r1) = ((y0 * fh) as usize, ((y1 * fh) as usize).min(f.h));
    for col in c0..c1 {
        let d = ((col as f32 + 0.5) / fw - x).abs() / half;
        let a = (1.0 - d).max(0.0).powi(2) * strength;
        for row in r0..r1 {
            let p = &mut f.data[(row * f.w + col) * 4..][..3];
            for k in 0..3 {
                p[k] = (p[k] as f32 * (1.0 - a) + rgb[k] as f32 * a) as u8;
            }
        }
    }
}

/// Towers with lit windows behind the evening set's window. The same
/// sequence every time, laid out in fractions, so every size matches.
fn skyline(f: &mut Frame) {
    let mut seed = 0x2545_f491_u32;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        (seed % 1000) as f32 / 1000.0
    };
    let (left, right, ground) = (0.06, 0.94, 0.59);
    let mut x = left;
    while x < right {
        let width = (0.03 + next() * 0.05).min(right - x);
        let top = ground - (0.10 + next() * 0.28);
        gradient(f, (x, top, x + width - 0.004, ground), [14, 14, 30], [10, 10, 22]);
        let mut y = top + 0.02;
        while y < ground - 0.03 {
            let mut wx = x + 0.006;
            while wx < x + width - 0.012 {
                if next() < 0.35 {
                    gradient(f, (wx, y, wx + 0.006, y + 0.012), [255, 206, 120], [230, 170, 90]);
                }
                wx += 0.012;
            }
            y += 0.03;
        }
        x += width;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide() -> SetView {
        SetView::default()
    }

    fn close() -> SetView {
        SetView { zoom: 2.4, x: 0.5, y: 0.34 }
    }

    #[test]
    fn a_move_starts_and_ends_exactly_on_its_presets() {
        assert_eq!(wide().toward(close(), 0.0), wide());
        let end = wide().toward(close(), 1.0);
        assert!((end.zoom - 2.4).abs() < 1e-5 && (end.y - 0.34).abs() < 1e-5);
    }

    /// Smooth means no jump between two frames and a gentle start and stop:
    /// at 50 frames a second, a 1.2 s move never leaps more than an even
    /// share would, and its first and last frames barely move.
    #[test]
    fn a_move_glides_without_jumps() {
        let frames = 60;
        let views: Vec<SetView> =
            (0..=frames).map(|n| wide().toward(close(), n as f32 / frames as f32)).collect();
        let steps: Vec<f32> = views.windows(2).map(|w| (w[1].zoom / w[0].zoom).ln()).collect();
        let even = (2.4f32).ln() / frames as f32;
        for (n, s) in steps.iter().enumerate() {
            assert!(*s >= 0.0, "zoom goes back at frame {n}");
            assert!(*s <= even * 1.6, "jump at frame {n}: {s} against {even}");
        }
        assert!(steps[0] < even * 0.1 && steps[frames - 1] < even * 0.1, "starts and stops gently");
        for w in views.windows(2) {
            assert!(w[1].y <= w[0].y + 1e-6, "the view rises steadily toward the face");
        }
    }

    #[test]
    fn the_view_never_shows_past_the_edge_of_the_set() {
        let corner = SetView { zoom: 3.0, x: 0.0, y: 1.0 }.clamped();
        let p = corner.place(Placement::FULL);
        assert!(p.x <= 0.0 && p.y <= 0.0, "{p:?}");
        assert!(p.x + p.scale >= 1.0 && p.y + p.scale >= 1.0, "{p:?}");
        for n in 0..=20 {
            let v = SetView { zoom: 1.0, x: 0.1, y: 0.9 }
                .toward(SetView { zoom: 4.0, x: 0.95, y: 0.05 }, n as f32 / 20.0);
            let p = v.place(Placement::FULL);
            assert!(p.x <= 1e-4 && p.y <= 1e-4 && p.x + p.scale >= 1.0 - 1e-4 && p.y + p.scale >= 1.0 - 1e-4);
        }
    }

    #[test]
    fn a_zoomed_camera_enlarges_around_its_centre() {
        let v = SetView { zoom: 2.0, x: 0.5, y: 0.5 };
        let p = v.place(Placement { x: 0.4, y: 0.4, scale: 0.2 });
        assert!((p.x - 0.3).abs() < 1e-6 && (p.y - 0.3).abs() < 1e-6 && (p.scale - 0.4).abs() < 1e-6);
    }

    #[test]
    fn built_in_decors_fill_the_back_and_leave_the_front_clear_above_the_desk() {
        for look in [SetLook::Studio, SetLook::Evening] {
            let back = backdrop(look, 320, 180).unwrap();
            assert!(back.data.chunks(4).all(|p| p[3] == 255), "{look:?} backdrop has holes");
            let front = desk(look, 320, 180).unwrap();
            let alpha = |x: usize, y: usize| front.data[(y * 320 + x) * 4 + 3];
            assert_eq!(alpha(160, 60), 0, "{look:?}: the face must show");
            assert_eq!(alpha(160, 150), 255, "{look:?}: the desk hides the lower body");
            assert_eq!(alpha(10, 150), 0, "{look:?}: beside the desk");
        }
        assert!(backdrop(SetLook::Empty, 32, 18).is_none() && desk(SetLook::Empty, 32, 18).is_none());
    }

    /// A card is drawn at its own small size: it must look like the output.
    #[test]
    fn a_decor_looks_the_same_at_card_size() {
        for look in [SetLook::Studio, SetLook::Evening] {
            let big = backdrop(look, 1280, 720).unwrap().thumbnail(160, 90);
            let small = backdrop(look, 160, 90).unwrap();
            let diff: u64 = big.data.iter().zip(&small.data).map(|(a, b)| a.abs_diff(*b) as u64).sum();
            let mean = diff as f32 / big.data.len() as f32;
            assert!(mean < 8.0, "{look:?}: the card differs from the output by {mean}");
        }
    }
}
