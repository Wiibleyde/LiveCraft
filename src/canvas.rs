//! What composing a picture needs from the surface it draws on. The mixer and
//! the transitions are written once against this, so the processor and the
//! graphics card follow exactly the same steps and cannot drift apart.

use crate::frame::Frame;
use crate::model::{Input, Position};
use std::sync::Arc;

pub trait Canvas {
    /// A finished picture this surface can draw from.
    type Pic: Clone;

    fn size(&self) -> (usize, usize);
    fn pic_size(p: &Self::Pic) -> (usize, usize);

    /// Fully transparent.
    fn clear(&mut self);
    /// Same size only.
    fn copy_from(&mut self, p: &Self::Pic);
    /// Same size only: `p` over this, with `opacity` applied to `p`.
    fn blend_over(&mut self, p: &Self::Pic, opacity: u8);
    /// Part of `p` (crop: left, right, top, bottom fractions) scaled into a
    /// rectangle that may stick out, with optional rounded corners.
    fn blit_region(
        &mut self,
        p: &Self::Pic,
        crop: [f32; 4],
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        opacity: u8,
        radius: f32,
    );
    fn fill_round(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, rgb: [u8; 3], opacity: u8);
    /// `a * (1 - t) + b * t`, all three the same size.
    fn mix(&mut self, a: &Self::Pic, b: &Self::Pic, t: f32);
    /// `b` revealed from the left behind a moving edge.
    fn wipe(&mut self, a: &Self::Pic, b: &Self::Pic, t: f32);
    /// `b` pushes `a` out to the left.
    fn slide(&mut self, a: &Self::Pic, b: &Self::Pic, t: f32);
    /// `b` revealed from the top.
    fn roller(&mut self, a: &Self::Pic, b: &Self::Pic, t: f32);
    /// `b` pushes `a` out to the top.
    fn vertical_slide(&mut self, a: &Self::Pic, b: &Self::Pic, t: f32);
    /// `p` moved, zoomed and turned as an input's position does, over this.
    fn blend_transformed(&mut self, p: &Self::Pic, pos: &Position, opacity: u8);
    /// A cube turning from `a` to `b`, seen from the front.
    fn cube(&mut self, a: &Self::Pic, b: &Self::Pic, t: f32, zoom: f32);
    /// The last step of the output: darkened by `level` (1 leaves it, 0 is
    /// black), then made opaque.
    fn finish(&mut self, level: f32);
}

/// Where pictures come from and go to: the canvases of one kind, and the
/// per-input processing that comes before any composing.
pub trait Painter: Sized {
    type Canvas: Canvas;

    /// Wraps a source's latest frame.
    fn source(&mut self, f: Arc<Frame>) -> Pic<Self>;
    /// A transparent canvas.
    fn canvas(&mut self, w: usize, h: usize) -> Self::Canvas;
    /// A canvas starting as a copy of `p`.
    fn canvas_from(&mut self, p: &Pic<Self>) -> Self::Canvas;
    fn done(&mut self, c: Self::Canvas) -> Pic<Self>;
    /// An empty picture (transparent black, black once on air), kept for reuse.
    fn black(&mut self, w: usize, h: usize) -> Pic<Self>;
    /// Applies an input's settings: geometry, colour, key, then effects.
    fn process(&mut self, src: &Pic<Self>, input: &Input, fill: Option<&Pic<Self>>) -> Pic<Self>;
}

pub type Pic<P> = <<P as Painter>::Canvas as Canvas>::Pic;

impl Canvas for Frame {
    type Pic = Arc<Frame>;

    fn size(&self) -> (usize, usize) {
        (self.w, self.h)
    }

    fn pic_size(p: &Arc<Frame>) -> (usize, usize) {
        (p.w, p.h)
    }

    fn clear(&mut self) {
        self.data.fill(0);
    }

    fn copy_from(&mut self, p: &Arc<Frame>) {
        Frame::copy_from(self, p);
    }

    fn blend_over(&mut self, p: &Arc<Frame>, opacity: u8) {
        Frame::blend_over(self, p, opacity);
    }

    fn blit_region(
        &mut self,
        p: &Arc<Frame>,
        crop: [f32; 4],
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        opacity: u8,
        radius: f32,
    ) {
        Frame::blit_region(self, p, crop, x, y, w, h, opacity, radius);
    }

    fn fill_round(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, rgb: [u8; 3], opacity: u8) {
        Frame::fill_round(self, x, y, w, h, radius, rgb, opacity);
    }

    fn mix(&mut self, a: &Arc<Frame>, b: &Arc<Frame>, t: f32) {
        Frame::mix(self, a, b, t);
    }

    fn wipe(&mut self, a: &Arc<Frame>, b: &Arc<Frame>, t: f32) {
        Frame::wipe(self, a, b, t);
    }

    fn slide(&mut self, a: &Arc<Frame>, b: &Arc<Frame>, t: f32) {
        Frame::slide(self, a, b, t);
    }

    fn roller(&mut self, a: &Arc<Frame>, b: &Arc<Frame>, t: f32) {
        Frame::roller(self, a, b, t);
    }

    fn vertical_slide(&mut self, a: &Arc<Frame>, b: &Arc<Frame>, t: f32) {
        Frame::vertical_slide(self, a, b, t);
    }

    fn blend_transformed(&mut self, p: &Arc<Frame>, pos: &Position, opacity: u8) {
        Frame::blend_over(self, &p.transform(pos, false), opacity);
    }

    fn cube(&mut self, a: &Arc<Frame>, b: &Arc<Frame>, t: f32, zoom: f32) {
        crate::transitions::cube(self, a, b, t, zoom);
    }

    fn finish(&mut self, level: f32) {
        if level < 1.0 {
            self.dim(level);
        }
        self.make_opaque();
    }
}

/// The processor's way: frames in memory.
#[derive(Default)]
pub struct Cpu {
    black: Option<Arc<Frame>>,
}

impl Painter for Cpu {
    type Canvas = Frame;

    fn source(&mut self, f: Arc<Frame>) -> Arc<Frame> {
        f
    }

    fn canvas(&mut self, w: usize, h: usize) -> Frame {
        Frame::new(w, h)
    }

    fn canvas_from(&mut self, p: &Arc<Frame>) -> Frame {
        (**p).clone()
    }

    fn done(&mut self, c: Frame) -> Arc<Frame> {
        Arc::new(c)
    }

    fn black(&mut self, w: usize, h: usize) -> Arc<Frame> {
        match &self.black {
            Some(b) if b.w == w && b.h == h => b.clone(),
            _ => {
                let b = Arc::new(Frame::new(w, h));
                self.black = Some(b.clone());
                b
            }
        }
    }

    fn process(&mut self, src: &Arc<Frame>, input: &Input, fill: Option<&Arc<Frame>>) -> Arc<Frame> {
        Arc::new(processed(src, input, fill.map(|f| &**f)))
    }
}

/// One odd setting must never take the whole output down: the input shows
/// unprocessed instead.
pub fn processed(src: &Frame, input: &Input, fill: Option<&Frame>) -> Frame {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::fx::process(src, input, fill)))
        .unwrap_or_else(|_| src.clone())
}
