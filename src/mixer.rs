//! The picture thread. Each tick it works out what should be on screen and
//! recomposes only if something in it changed, so a still desk costs nothing.

use crate::engine::*;
use crate::frame::Frame;
use crate::fx;
use crate::model::*;
use crate::sets;
use crate::sources::InputRt;
use crate::text;
use crate::transitions;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

const MAX_DEPTH: u8 = 3;

enum Base {
    One(Option<u64>),
    Mix { from: Option<u64>, to: Option<u64>, kind: TransKind, t: f32 },
}

struct OverlayDraw {
    id: u64,
    t: f32,
    cfg: OverlayCfg,
}

struct Plan {
    snap: Arc<Snapshot>,
    base: Base,
    stinger: Option<(u64, bool)>,
    overlays: Vec<OverlayDraw>,
    ftb: f32,
    preview: Option<u64>,
    fades: HashMap<u64, (Option<u64>, f32)>,
    /// Virtual set cameras part way through a move.
    set_views: HashMap<u64, SetView>,
    /// Every input someone can see or hear right now, however deep.
    showing: HashSet<u64>,
}

impl State {
    fn plan(&mut self, now: Instant, watched: Vec<u64>) -> Plan {
        let mut stinger = None;
        let base = if let Some(tr) = self.trans {
            let t = now.saturating_duration_since(tr.started).as_secs_f32() * 1000.0 / tr.ms as f32;
            let t = t.min(1.0);
            // The stinger keeps the settings it started with.
            if let Some(cfg) = tr.stinger {
                stinger = cfg.input.map(|id| (id, cfg.under_overlays));
                let cut = cfg.cut_ms as f32 / cfg.duration_ms.max(1) as f32;
                Base::One(if t < cut { tr.from } else { tr.to })
            } else {
                Base::Mix { from: tr.from, to: tr.to, kind: tr.kind, t }
            }
        } else if self.tbar > 0.0 {
            Base::Mix {
                from: self.project.program,
                to: self.project.preview,
                kind: self.project.trans[0].kind,
                t: self.tbar,
            }
        } else {
            Base::One(self.project.program)
        };
        let overlays = (0..OVERLAYS)
            .filter_map(|i| {
                let id = self.overlay_shown[i]?;
                (self.overlay_t[i] > 0.0).then_some(OverlayDraw {
                    id,
                    t: self.overlay_t[i],
                    cfg: self.project.overlay_cfg[i],
                })
            })
            .collect();
        let fades = self
            .mix_fades
            .iter()
            .map(|(id, f)| {
                let t = now.saturating_duration_since(f.started).as_secs_f32() * 1000.0 / f.ms as f32;
                (*id, (f.from, t.min(1.0)))
            })
            .collect();
        let set_views =
            self.set_moves.keys().filter_map(|id| Some((*id, self.set_view(*id, now)?))).collect();
        let mut seen = self.on_air();
        seen.extend(self.project.preview);
        seen.extend(watched);
        let showing = self.with_layers(seen);
        Plan {
            showing,
            snap: self.snapshot(),
            base,
            stinger,
            overlays,
            ftb: self.ftb_level,
            preview: self.project.preview,
            fades,
            set_views,
        }
    }
}

struct Memo {
    sig: u64,
    pic: Option<Arc<Frame>>,
}

#[derive(Default)]
struct Mixer {
    pool: Vec<Arc<Frame>>,
    last_sig: u64,
    memo: HashMap<u64, Memo>,
    /// Card-sized pictures, which never touch the full-size compositing.
    small: HashMap<u64, Memo>,
    preview_sig: u64,
    route_sig: [u64; ROUTES],
    thumb_sig: HashMap<u64, u64>,
    black: Option<Arc<Frame>>,
    completions: HashMap<u64, u64>,
    positions: HashMap<u64, u64>,
    /// Built-in set decors, by look, back or front, and size.
    looks: HashMap<(SetLook, bool, usize, usize), Arc<Frame>>,
}

struct Ctx<'a> {
    plan: &'a Plan,
    rts: &'a HashMap<u64, Arc<InputRt>>,
}

impl Mixer {
    /// A retired buffer nobody reads any more, else a fresh one.
    fn take_buffer(&mut self, w: usize, h: usize) -> Frame {
        self.pool.retain(|a| a.w == w && a.h == h);
        if self.pool.len() > 12 {
            self.pool.remove(0);
        }
        self.pool
            .iter()
            .position(|a| Arc::strong_count(a) == 1)
            .and_then(|i| Arc::try_unwrap(self.pool.swap_remove(i)).ok())
            .unwrap_or_else(|| Frame::new(w, h))
    }

    /// Closed inputs must not keep their last pictures in memory.
    fn forget_closed(&mut self, snap: &Snapshot) {
        let open = |id: &u64| snap.inputs.contains_key(id);
        self.memo.retain(|id, _| open(id));
        self.small.retain(|id, _| open(id));
        self.thumb_sig.retain(|id, _| open(id));
        self.completions.retain(|id, _| open(id));
        self.positions.retain(|id, _| open(id));
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

    /// Fingerprint of everything an input's picture depends on.
    fn sig(&self, id: u64, c: &Ctx, depth: u8) -> u64 {
        let mut h = DefaultHasher::new();
        id.hash(&mut h);
        if depth > MAX_DEPTH {
            return h.finish();
        }
        c.plan.snap.version.hash(&mut h);
        c.rts.get(&id).map_or(0, |r| r.seq()).hash(&mut h);
        let Some(input) = c.plan.snap.inputs.get(&id) else { return h.finish() };
        for l in input.layers.iter().filter(|l| l.visible) {
            self.sig(l.input, c, depth + 1).hash(&mut h);
        }
        if let Kind::Mix { output, .. } = input.kind {
            output.map(|o| self.sig(o, c, depth + 1)).hash(&mut h);
            if let Some((from, t)) = c.plan.fades.get(&id) {
                from.map(|o| self.sig(o, c, depth + 1)).hash(&mut h);
                t.to_bits().hash(&mut h);
            }
        }
        if let Kind::VirtualSet { background, foreground, .. } = input.kind {
            for part in [background, foreground].into_iter().flatten() {
                self.sig(part, c, depth + 1).hash(&mut h);
            }
            if let Some(v) = c.plan.set_views.get(&id) {
                [v.zoom, v.x, v.y].map(f32::to_bits).hash(&mut h);
            }
        }
        if let Some(fill) = input.key.fill_input.filter(|_| input.key.mode == KeyMode::Fill) {
            self.sig(fill, c, depth + 1).hash(&mut h);
        }
        h.finish()
    }

    /// The input's finished picture, reused until something in it changes.
    fn picture(&mut self, id: u64, c: &Ctx, depth: u8) -> Option<Arc<Frame>> {
        if depth > MAX_DEPTH {
            return None;
        }
        let sig = self.sig(id, c, depth);
        if let Some(m) = self.memo.get(&id)
            && m.sig == sig
        {
            return m.pic.clone();
        }
        let pic = self.render(id, c, depth);
        self.memo.insert(id, Memo { sig, pic: pic.clone() });
        pic
    }

    /// An input as its card shows it, made at card size: a scene off air costs
    /// a few thousand pixels instead of being composed in full six times a
    /// second for a picture a few centimetres wide.
    fn small_picture(&mut self, id: u64, c: &Ctx, depth: u8, size: (usize, usize)) -> Option<Arc<Frame>> {
        if depth > MAX_DEPTH {
            return None;
        }
        let sig = self.sig(id, c, depth);
        if let Some(m) = self.small.get(&id)
            && m.sig == sig
        {
            return m.pic.clone();
        }
        let pic = self.render_small(id, c, depth, size);
        self.small.insert(id, Memo { sig, pic: pic.clone() });
        pic
    }

    fn render_small(&mut self, id: u64, c: &Ctx, depth: u8, size: (usize, usize)) -> Option<Arc<Frame>> {
        let input = c.plan.snap.inputs.get(&id)?;
        let (w, h) = size;
        let mut base = match input.kind {
            Kind::Scene => None,
            Kind::Mix { output, .. } => output.and_then(|o| self.small_picture(o, c, depth + 1, size)),
            Kind::VirtualSet { .. } => self.set_picture(id, c, depth, Some(size)),
            _ => c.rts.get(&id).and_then(|r| r.latest()).map(|f| Arc::new(f.thumbnail(w, h))),
        };
        if let (true, Some(src)) = (input.needs_processing(), &base) {
            let fill = input
                .key
                .fill_input
                .filter(|_| input.key.mode == KeyMode::Fill)
                .and_then(|f| self.small_picture(f, c, depth + 1, size));
            base = Some(Arc::new(processed(src, input, fill.as_deref())));
        }
        if !draws_layers_on_top(input) {
            return base;
        }
        let mut out = base.map_or_else(|| Frame::new(w, h), |b| (*b).clone());
        for l in input.layers.iter().filter(|l| l.visible && l.input != id) {
            if let Some(p) = self.small_picture(l.input, c, depth + 1, size) {
                draw_layer(&mut out, &p, l, 255);
            }
        }
        if matches!(input.kind, Kind::Scene) && input.needs_processing() {
            return Some(Arc::new(processed(&out, input, None)));
        }
        Some(Arc::new(out))
    }

    fn base_picture(&mut self, id: u64, c: &Ctx, depth: u8) -> Option<Arc<Frame>> {
        let input = c.plan.snap.inputs.get(&id)?;
        let mut base = match input.kind {
            Kind::Scene => None,
            Kind::Mix { output, .. } => self.mix_picture(id, output, c, depth),
            Kind::VirtualSet { .. } => self.set_picture(id, c, depth, None),
            _ => c.rts.get(&id).and_then(|r| r.latest()),
        };
        if input.needs_processing()
            && let Some(src) = &base
        {
            let fill = input
                .key
                .fill_input
                .filter(|_| input.key.mode == KeyMode::Fill)
                .and_then(|f| self.picture(f, c, depth + 1));
            base = Some(Arc::new(processed(src, input, fill.as_deref())));
        }
        base
    }

    fn mix_picture(&mut self, id: u64, output: Option<u64>, c: &Ctx, depth: u8) -> Option<Arc<Frame>> {
        let to = output.and_then(|o| self.picture(o, c, depth + 1));
        let Some((from, t)) = c.plan.fades.get(&id).copied() else { return to };
        let from = from.and_then(|o| self.picture(o, c, depth + 1));
        let (w, h) = (c.plan.snap.w, c.plan.snap.h);
        let black = self.black(w, h);
        let usable =
            |f: Option<Arc<Frame>>| f.filter(|f| f.w == w && f.h == h).unwrap_or_else(|| black.clone());
        let (a, b) = (usable(from), usable(to));
        let mut out = Frame::new(w, h);
        out.mix(&a, &b, t);
        Some(Arc::new(out))
    }

    /// A virtual set through its camera: backdrop, presenters, then the desk
    /// in front, each moved and enlarged by the camera. `card` asks for it at
    /// card size, made from card-size pictures.
    fn set_picture(
        &mut self,
        id: u64,
        c: &Ctx,
        depth: u8,
        card: Option<(usize, usize)>,
    ) -> Option<Arc<Frame>> {
        let input = c.plan.snap.inputs.get(&id)?;
        let Kind::VirtualSet { look, background, foreground, .. } = input.kind else { return None };
        let view = c.plan.set_views.get(&id).copied().or_else(|| sets::resting_view(&input.kind))?;
        let (w, h) = card.unwrap_or((c.plan.snap.w, c.plan.snap.h));
        let draw = |out: &mut Frame, pic: &Frame, l: &Layer| {
            let mut seen = *l;
            seen.placement = view.place(l.placement);
            draw_layer(out, pic, &seen, 255);
        };
        let mut out = Frame::new(w, h);
        let back = match background {
            Some(b) => self.part(b, c, depth, card),
            None => self.look(look, false, w, h),
        };
        if let Some(p) = back {
            draw(&mut out, &p, &Layer::new(id, Placement::FULL));
        }
        for l in input.layers.iter().filter(|l| l.visible && l.input != id) {
            if let Some(p) = self.part(l.input, c, depth, card) {
                draw(&mut out, &p, l);
            }
        }
        let front = match foreground {
            Some(f) => self.part(f, c, depth, card),
            None => self.look(look, true, w, h),
        };
        if let Some(p) = front {
            draw(&mut out, &p, &Layer::new(id, Placement::FULL));
        }
        Some(Arc::new(out))
    }

    fn part(&mut self, id: u64, c: &Ctx, depth: u8, card: Option<(usize, usize)>) -> Option<Arc<Frame>> {
        match card {
            Some(size) => self.small_picture(id, c, depth + 1, size),
            None => self.picture(id, c, depth + 1),
        }
    }

    /// A built-in decor is drawn once per size, then reused.
    fn look(&mut self, look: SetLook, front: bool, w: usize, h: usize) -> Option<Arc<Frame>> {
        let key = (look, front, w, h);
        if let Some(f) = self.looks.get(&key) {
            return Some(f.clone());
        }
        let f = Arc::new(if front { sets::desk(look, w, h) } else { sets::backdrop(look, w, h) }?);
        // Sizes left behind by a change of format are not worth keeping.
        if self.looks.len() >= 8 {
            self.looks.clear();
        }
        self.looks.insert(key, f.clone());
        Some(f)
    }

    fn render(&mut self, id: u64, c: &Ctx, depth: u8) -> Option<Arc<Frame>> {
        let input = c.plan.snap.inputs.get(&id)?;
        let base = self.base_picture(id, c, depth);
        if !draws_layers_on_top(input) {
            return base;
        }
        let scene = matches!(input.kind, Kind::Scene);
        let (w, h) = (c.plan.snap.w, c.plan.snap.h);
        let mut out = match &base {
            Some(b) if b.w == w && b.h == h => (**b).clone(),
            _ => Frame::new(w, h),
        };
        for l in input.layers.iter().filter(|l| l.visible && l.input != id) {
            if let Some(p) = self.picture(l.input, c, depth + 1) {
                draw_layer(&mut out, &p, l, 255);
            }
        }
        // A scene has no picture of its own: its settings act on the result.
        if scene && input.needs_processing() {
            return Some(Arc::new(processed(&out, input, None)));
        }
        Some(Arc::new(out))
    }

    /// Layers matching on both sides travel to their new place; the rest
    /// fade, as the Merge transition does.
    fn merge(&mut self, out: &mut Frame, from: Option<u64>, to: Option<u64>, t: f32, c: &Ctx) {
        let items = |id: Option<u64>| -> Vec<Layer> {
            let Some(input) = id.and_then(|i| c.plan.snap.inputs.get(&i)) else { return Vec::new() };
            let mut v = Vec::new();
            if !matches!(input.kind, Kind::Scene) {
                v.push(Layer::new(input.id, Placement::FULL));
            }
            if !matches!(input.kind, Kind::VirtualSet { .. }) {
                v.extend(input.layers.iter().filter(|l| l.visible));
            }
            v
        };
        let (a, b) = (items(from), items(to));
        out.data.fill(0);
        for l in a.iter().filter(|l| !b.iter().any(|o| o.input == l.input)) {
            if let Some(p) = self.layer_source(l, from, c) {
                draw_layer(out, &p, l, ((1.0 - t) * 255.0) as u8);
            }
        }
        for l in &b {
            match a.iter().find(|o| o.input == l.input) {
                Some(start) => {
                    let mut moved = *l;
                    moved.placement = start.placement.lerp(l.placement, t);
                    for k in 0..4 {
                        moved.crop[k] = start.crop[k] + (l.crop[k] - start.crop[k]) * t;
                    }
                    if let Some(p) = self.layer_source(&moved, to, c) {
                        draw_layer(out, &p, &moved, 255);
                    }
                }
                None => {
                    if let Some(p) = self.layer_source(l, to, c) {
                        draw_layer(out, &p, l, (t * 255.0) as u8);
                    }
                }
            }
        }
    }

    /// An input's own base when it stands for itself, else the layer's input.
    fn layer_source(&mut self, l: &Layer, owner: Option<u64>, c: &Ctx) -> Option<Arc<Frame>> {
        if owner == Some(l.input) { self.base_picture(l.input, c, 1) } else { self.picture(l.input, c, 1) }
    }

    fn opt_sig(&self, id: Option<u64>, c: &Ctx) -> u64 {
        id.map_or(0, |i| self.sig(i, c, 0))
    }
}

/// One odd setting must never take the whole output down: the input shows
/// unprocessed instead.
fn processed(src: &Frame, input: &Input, fill: Option<&Frame>) -> Frame {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fx::process(src, input, fill)))
        .unwrap_or_else(|_| src.clone())
}

/// A virtual set's layers are its presenters, already drawn inside it under
/// its camera; everywhere else layers go on top of the input's picture.
fn draws_layers_on_top(input: &Input) -> bool {
    !matches!(input.kind, Kind::VirtualSet { .. }) && input.layers.iter().any(|l| l.visible)
}

fn draw_layer(out: &mut Frame, pic: &Frame, l: &Layer, opacity: u8) {
    let p = l.placement;
    if p.is_full() && l.crop == [0.0; 4] && l.border.thickness <= 0.0 && pic.w == out.w && pic.h == out.h {
        out.blend_over(pic, opacity);
        return;
    }
    let (fw, fh) = (out.w as f32, out.h as f32);
    let (w, h) = (fw * p.scale, fh * p.scale);
    let x = fw * p.x + l.crop[0] * w;
    let y = fh * p.y + l.crop[2] * h;
    let cw = w * (1.0 - l.crop[0] - l.crop[1]);
    let ch = h * (1.0 - l.crop[2] - l.crop[3]);
    let (t, radius) = border_size(&l.border, p.scale, fw.max(fh));
    if t > 0.0 {
        let outer = if radius > 0.0 { radius + t } else { 0.0 };
        out.fill_round(x - t, y - t, cw + 2.0 * t, ch + 2.0 * t, outer, l.border.color, opacity);
    }
    out.blit_region(pic, l.crop, x, y, cw, ch, opacity, radius);
}

/// A border belongs to the picture it frames: its thickness and corners are
/// set at full size and shrink with it, and with the frame it is drawn on, so
/// a card shows the same proportions as the output.
fn border_size(b: &Border, scale: f32, longest_side: f32) -> (f32, f32) {
    let k = scale * longest_side / 1920.0;
    (b.thickness * k, b.radius * k)
}

/// Places an overlay at its stage of coming on, per its effect.
fn draw_overlay(out: &mut Frame, pic: &Frame, o: &OverlayDraw) {
    let (fw, fh) = (out.w as f32, out.h as f32);
    let p = if o.cfg.pip { o.cfg.placement } else { Placement::FULL };
    let (mut x, mut y) = (fw * p.x, fh * p.y);
    let (mut w, mut h) = (fw * p.scale, fh * p.scale);
    let mut crop = [0.0; 4];
    let mut opacity = 255.0;
    let t = o.t;
    match o.cfg.effect {
        TransKind::Cut => {}
        TransKind::Zoom | TransKind::CrossZoom | TransKind::Fly | TransKind::FlyRotate => {
            x += w * (1.0 - t) / 2.0;
            y += h * (1.0 - t) / 2.0;
            w *= t;
            h *= t;
        }
        TransKind::Wipe | TransKind::WipeReverse => {
            crop[1] = 1.0 - t;
            w *= t;
        }
        TransKind::VerticalWipe | TransKind::VerticalWipeReverse | TransKind::RollerDoor => {
            crop[3] = 1.0 - t;
            h *= t;
        }
        TransKind::Slide | TransKind::SlideReverse => x -= (x + w) * (1.0 - t),
        TransKind::VerticalSlide | TransKind::VerticalSlideReverse => y += (fh - y) * (1.0 - t),
        _ => opacity *= t,
    }
    let (th, round) = border_size(&o.cfg.border, p.scale, fw.max(fh));
    if o.cfg.pip && th > 0.0 {
        let outer = if round > 0.0 { round + th } else { 0.0 };
        out.fill_round(x - th, y - th, w + 2.0 * th, h + 2.0 * th, outer, o.cfg.border.color, opacity as u8);
    }
    let radius = if o.cfg.pip { round } else { 0.0 };
    if t >= 1.0 && !o.cfg.pip && pic.w == out.w && pic.h == out.h {
        out.blend_over(pic, 255);
    } else {
        out.blit_region(pic, crop, x, y, w, h, opacity as u8, radius);
    }
}

impl Engine {
    pub(crate) fn video_loop(self: Arc<Self>) {
        let mut m = Mixer::default();
        let mut last = Instant::now();
        let mut next = Instant::now();
        let mut tick_n: u32 = 0;
        let mut window = Instant::now();
        let (mut ticks, mut composed) = (0u32, 0u32);
        let mut behind_for = 0u32;
        while !self.quit.load(Ordering::Relaxed) {
            let now = Instant::now();
            let dt = now.duration_since(last).as_secs_f32();
            last = now;
            let (watched, everything) = self.watched_by_outputs();
            let (plan, notes) = {
                let mut s = self.state.lock().unwrap();
                s.advance(now, dt);
                let notes = std::mem::take(&mut s.notes);
                (s.plan(now, watched), notes)
            };
            let fps = plan.snap.fps;
            let rts: HashMap<u64, Arc<InputRt>> = self.rts.lock().unwrap().clone();
            for (id, rt) in &rts {
                rt.showing.store(everything || plan.showing.contains(id), Ordering::Relaxed);
            }
            self.carry_out(&notes, &rts);
            self.watch_timelines(&mut m, &plan.snap, &rts);
            self.playlist_tick(now);

            let c = Ctx { plan: &plan, rts: &rts };
            let t0 = Instant::now();
            if self.compose(&mut m, &c) {
                composed += 1;
                self.stats.mix_us.store(t0.elapsed().as_micros() as u64, Ordering::Relaxed);
                self.ping();
            }
            self.update_preview(&mut m, &c);
            let refresh_all = tick_n.is_multiple_of((fps / 6).max(1));
            self.update_routes(&mut m, &c, refresh_all);
            self.feed_outputs(&mut m, &c);
            if refresh_all {
                self.update_thumbs(&mut m, &c);
            }
            tick_n = tick_n.wrapping_add(1);
            ticks += 1;
            if window.elapsed() >= Duration::from_secs(1) {
                self.stats.ticks_per_sec.store(ticks, Ordering::Relaxed);
                self.stats.composed_per_sec.store(composed, Ordering::Relaxed);
                // A moment of strain is normal, when something starts or a
                // file opens; lasting several seconds is worth saying.
                behind_for = if ticks + ticks / 10 < fps { behind_for + 1 } else { 0 };
                self.stats.struggling.store(behind_for >= 4, Ordering::Relaxed);
                (ticks, composed) = (0, 0);
                window = Instant::now();
                self.check_outputs();
                m.forget_closed(&plan.snap);
            }

            let period = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
            next += period;
            let now = Instant::now();
            if next > now {
                thread::sleep(next - now);
            } else {
                self.stats.late_ticks.fetch_add(1, Ordering::Relaxed);
                // Fell behind. The picture skips ticks, but a recording must
                // stay as long as its sound, so it repeats the frame it missed.
                let missed = (now - next).as_secs_f64() / period.as_secs_f64();
                for _ in 0..(missed as u32).min(fps) {
                    self.feed_outputs(&mut m, &c);
                }
                next = now;
            }
        }
    }

    /// Inputs going somewhere other than the air, and whether a multiview
    /// shows every input at once.
    fn watched_by_outputs(&self) -> (Vec<u64>, bool) {
        let mut ids = Vec::new();
        let mut everything = false;
        let mut route = |r: &Route| match r {
            Route::Input(id) => ids.push(*id),
            Route::MultiView | Route::MultiView2 => everything = true,
            _ => {}
        };
        self.routes.lock().unwrap().iter().flatten().for_each(&mut route);
        self.srt.lock().unwrap().iter().for_each(|(r, _)| route(r));
        ids.extend(self.multi.lock().unwrap().iter().map(|(id, _)| *id));
        ids.extend(self.replay.lock().unwrap().cams.iter().map(|c| c.input));
        (ids, everything)
    }

    fn carry_out(self: &Arc<Self>, notes: &[Note], rts: &HashMap<u64, Arc<InputRt>>) {
        for note in notes {
            match *note {
                Note::Trigger(id, event) => self.fire(id, event),
                Note::Play(id) => {
                    if let Some(rt) = rts.get(&id) {
                        rt.playing.store(true, Ordering::Relaxed);
                    }
                }
                Note::Pause(id) => {
                    if let Some(rt) = rts.get(&id) {
                        rt.playing.store(false, Ordering::Relaxed);
                    }
                }
                Note::Restart(id) => {
                    if let Some(rt) = rts.get(&id) {
                        rt.seek_ms.store(0, Ordering::Relaxed);
                    }
                }
                Note::FirstItem(id) => {
                    if let Some(rt) = rts.get(&id) {
                        rt.select.store(0, Ordering::Relaxed);
                    }
                }
            }
        }
    }

    /// Runs an input's triggers for an event, each after its own delay.
    pub(crate) fn fire(self: &Arc<Self>, id: u64, event: TriggerEvent) {
        let list: Vec<Trigger> = {
            let s = self.state.lock().unwrap();
            let Some(input) = s.input(id) else { return };
            input.triggers.iter().filter(|t| t.event == event).cloned().collect()
        };
        if list.is_empty() {
            return;
        }
        let engine = self.clone();
        // In list order, one after another, as tasks of the same event run.
        thread::spawn(move || {
            for t in list {
                thread::sleep(Duration::from_millis(t.delay_ms.min(30_000) as u64));
                let input = if t.input.is_empty() { id.to_string() } else { t.input.clone() };
                let args = crate::functions::Args::new().input(&input).value(&t.value);
                let _ = engine.call(&t.function, &args);
            }
        });
    }

    /// Fires the triggers that depend on playing position.
    fn watch_timelines(self: &Arc<Self>, m: &mut Mixer, snap: &Snapshot, rts: &HashMap<u64, Arc<InputRt>>) {
        for input in snap.inputs.values().filter(|i| !i.triggers.is_empty()) {
            let Some(rt) = rts.get(&input.id) else { continue };
            let done = rt.completions.load(Ordering::Relaxed);
            let seen = m.completions.insert(input.id, done).unwrap_or(done);
            if done > seen {
                self.fire(input.id, TriggerEvent::OnCompletion);
                self.fire(input.id, TriggerEvent::OnCountdownCompleted);
            }
            let pos = rt.pos_ms.load(Ordering::Relaxed);
            let dur = pos + rt.remaining_ms();
            let was = m.positions.insert(input.id, pos).unwrap_or(pos);
            for t in &input.triggers {
                let crossed = match t.event {
                    TriggerEvent::OnPlaybackTime | TriggerEvent::OnCountdownTime => {
                        was < t.time_ms && pos >= t.time_ms
                    }
                    TriggerEvent::OnPlaybackRemaining | TriggerEvent::OnCountdownRemaining => {
                        let edge = dur.saturating_sub(t.time_ms);
                        dur > 0 && was < edge && pos >= edge
                    }
                    _ => false,
                };
                if crossed {
                    self.fire(input.id, t.event);
                }
            }
        }
    }

    fn playlist_tick(self: &Arc<Self>, now: Instant) {
        let due = {
            let s = self.state.lock().unwrap();
            let manual = s.project.playlist.manual;
            s.playlist_on && !manual && s.playlist_next.is_none_or(|n| now >= n)
        };
        if due {
            self.playlist_step();
        }
    }

    /// Moves to the next playlist item.
    pub fn playlist_step(&self) {
        let mut s = self.state.lock().unwrap();
        let list = s.project.playlist.clone();
        if list.items.is_empty() {
            s.playlist_on = false;
            return;
        }
        if s.playlist_index >= list.items.len() {
            if !list.looped {
                s.playlist_on = false;
                s.playlist_index = 0;
                return;
            }
            s.playlist_index = 0;
        }
        if s.playlist_index == 0 && list.clear_overlays {
            s.all_overlays_off();
        }
        let item = list.items[s.playlist_index].clone();
        s.playlist_index += 1;
        if item.overlay {
            s.set_overlay(0, Some(item.input));
        } else {
            s.set_preview(item.input);
            s.run_transition(item.trans, item.trans_ms);
        }
        let rt = self.rt(item.input);
        if let (Some(rt), Some(at)) = (&rt, item.start_ms) {
            rt.seek_ms.store(at as i64, Ordering::Relaxed);
        }
        let own = rt.map_or(0, |r| r.remaining_ms());
        let wait = if item.duration_ms > 0 { item.duration_ms } else { own.max(1000) };
        s.playlist_next = Some(Instant::now() + Duration::from_millis(wait));
    }

    /// Returns true when a new picture was published.
    fn compose(&self, m: &mut Mixer, c: &Ctx) -> bool {
        let (w, h) = (c.plan.snap.w, c.plan.snap.h);
        let mut sig = DefaultHasher::new();
        c.plan.snap.version.hash(&mut sig);
        (w, h, c.plan.ftb.to_bits()).hash(&mut sig);
        match c.plan.base {
            Base::One(id) => m.opt_sig(id, c).hash(&mut sig),
            Base::Mix { from, to, t, .. } => {
                (m.opt_sig(from, c), m.opt_sig(to, c), t.to_bits()).hash(&mut sig);
            }
        }
        if let Some((id, _)) = c.plan.stinger {
            m.sig(id, c, 0).hash(&mut sig);
        }
        for o in &c.plan.overlays {
            (m.sig(o.id, c, 0), o.t.to_bits()).hash(&mut sig);
        }
        let sig = sig.finish();
        if sig == m.last_sig {
            return false;
        }
        m.last_sig = sig;

        let mut out = m.take_buffer(w, h);
        match c.plan.base {
            Base::One(id) => match id.and_then(|i| m.picture(i, c, 0)) {
                Some(f) if f.w == w && f.h == h => out.copy_from(&f),
                _ => out.data.fill(0),
            },
            Base::Mix { from, to, kind: TransKind::Merge, t } => m.merge(&mut out, from, to, t, c),
            Base::Mix { from, to, kind, t } => {
                let black = m.black(w, h);
                let usable = |f: Option<Arc<Frame>>| {
                    f.filter(|f| f.w == w && f.h == h).unwrap_or_else(|| black.clone())
                };
                let a = usable(from.and_then(|i| m.picture(i, c, 0)));
                let b = usable(to.and_then(|i| m.picture(i, c, 0)));
                transitions::apply(&mut out, &a, &b, kind, t);
            }
        }
        let stinger = c.plan.stinger.and_then(|(id, under)| Some((m.picture(id, c, 0)?, under)));
        if let Some((p, true)) = &stinger {
            out.blit_region(p, [0.0; 4], 0.0, 0.0, w as f32, h as f32, 255, 0.0);
        }
        for o in &c.plan.overlays {
            if let Some(p) = m.picture(o.id, c, 0) {
                draw_overlay(&mut out, &p, o);
            }
        }
        if let Some((p, false)) = &stinger {
            out.blit_region(p, [0.0; 4], 0.0, 0.0, w as f32, h as f32, 255, 0.0);
        }
        if c.plan.ftb > 0.0 {
            out.dim(1.0 - c.plan.ftb);
        }
        out.make_opaque();
        let out = Arc::new(out);
        let old = std::mem::replace(&mut *self.program.lock().unwrap(), out);
        m.pool.push(old);
        self.program_seq.fetch_add(1, Ordering::Release);
        true
    }

    fn update_preview(&self, m: &mut Mixer, c: &Ctx) {
        let sig = m.opt_sig(c.plan.preview, c);
        if sig == m.preview_sig {
            return;
        }
        m.preview_sig = sig;
        *self.preview.lock().unwrap() = c.plan.preview.and_then(|i| m.picture(i, c, 0));
        self.preview_seq.fetch_add(1, Ordering::Release);
        self.ping();
    }

    fn route_frame(&self, route: Route, m: &mut Mixer, c: &Ctx) -> Option<Arc<Frame>> {
        match route {
            Route::Output => Some(self.program.lock().unwrap().clone()),
            Route::Preview => self.preview.lock().unwrap().clone(),
            Route::Input(id) => m.picture(id, c, 0),
            Route::MultiView => Some(Arc::new(multiview(m, c, false))),
            Route::MultiView2 => Some(Arc::new(multiview(m, c, true))),
        }
    }

    fn update_routes(&self, m: &mut Mixer, c: &Ctx, refresh_multiview: bool) {
        let routes = *self.routes.lock().unwrap();
        let mut changed = false;
        for (n, route) in routes.iter().enumerate() {
            let Some(route) = *route else { continue };
            let multi = matches!(route, Route::MultiView | Route::MultiView2);
            if multi && !refresh_multiview {
                continue;
            }
            let frame = self.route_frame(route, m, c);
            let sig = frame.as_ref().map_or(0, |f| Arc::as_ptr(f) as u64);
            if sig != m.route_sig[n] {
                m.route_sig[n] = sig;
                self.route_frames.lock().unwrap()[n] = frame;
                changed = true;
            }
        }
        if changed {
            self.route_seq.fetch_add(1, Ordering::Release);
            self.ping();
        }
    }

    fn feed_outputs(&self, m: &mut Mixer, c: &Ctx) {
        let out = self.program.lock().unwrap().clone();
        for enc in self.recorders.lock().unwrap().iter() {
            enc.push_video(out.clone());
        }
        for enc in self.streamers.lock().unwrap().iter() {
            enc.push_video(out.clone());
        }
        drop(out);
        for (id, enc) in self.multi.lock().unwrap().iter() {
            if let Some(f) = m.picture(*id, c, 0) {
                enc.push_video(f);
            }
        }
        let srt: Vec<Route> = self.srt.lock().unwrap().iter().map(|(r, _)| *r).collect();
        for (n, route) in srt.into_iter().enumerate() {
            if let Some(f) = self.route_frame(route, m, c)
                && let Some((_, enc)) = self.srt.lock().unwrap().get(n)
            {
                enc.push_video(f);
            }
        }
        let cams: Vec<u64> = self.replay.lock().unwrap().cams.iter().map(|c| c.input).collect();
        for (n, id) in cams.into_iter().enumerate() {
            if let Some(f) = m.picture(id, c, 0)
                && let Some(cam) = self.replay.lock().unwrap().cams.get(n)
            {
                cam.encoder.push_video(f);
            }
        }
    }

    fn update_thumbs(&self, m: &mut Mixer, c: &Ctx) {
        let mut any = false;
        for &id in &c.plan.snap.order {
            let sig = m.sig(id, c, 0);
            if m.thumb_sig.get(&id) == Some(&sig) {
                continue;
            }
            // Keeps the output's shape, wide or vertical.
            let (w, h) = (c.plan.snap.w, c.plan.snap.h);
            let th = (THUMB_W * h / w.max(1)).clamp(1, THUMB_H);
            let size = ((th * w / h.max(1)).max(1), th);
            if let Some(small) = m.small_picture(id, c, 0, size) {
                let stamp = self.thumb_clock.fetch_add(1, Ordering::Relaxed) + 1;
                self.thumbs.lock().unwrap().insert(id, (stamp, small));
                m.thumb_sig.insert(id, sig);
                any = true;
            }
        }
        if any {
            self.ping();
        }
    }
}

/// Preview and output above, inputs below, each with its title.
fn multiview(m: &mut Mixer, c: &Ctx, inputs_only: bool) -> Frame {
    let (w, h) = (c.plan.snap.w, c.plan.snap.h);
    let mut out = Frame::solid(w, h, [8, 8, 10]);
    let (fw, fh) = (w as f32, h as f32);
    let orange = [242, 140, 0];
    let green = [10, 160, 30];
    let mut cell =
        |m: &mut Mixer, id: Option<u64>, x: f32, y: f32, cw: f32, ch: f32, tally: Option<[u8; 3]>| {
            let Some(id) = id else { return };
            if let Some(p) = m.picture(id, c, 0) {
                out.blit_region(&p, [0.0; 4], x + 2.0, y + 2.0, cw - 4.0, ch - 4.0, 255, 0.0);
            }
            if let Some(col) = tally {
                for (bx, by, bw, bh) in
                    [(x, y, cw, 3.0), (x, y + ch - 3.0, cw, 3.0), (x, y, 3.0, ch), (x + cw - 3.0, y, 3.0, ch)]
                {
                    out.fill_round(bx, by, bw, bh, 0.0, col, 255);
                }
            }
            let name = c.plan.snap.inputs.get(&id).map_or("", |i| i.name.as_str());
            let px = fh * 0.024;
            out.fill_round(x + 2.0, y + ch - px * 1.5, cw - 4.0, px * 1.4, 0.0, [0, 0, 0], 170);
            text::draw(&mut out, name, x + 8.0, y + ch - px * 1.45, px, [255, 255, 255], false);
        };
    let program = match c.plan.base {
        Base::One(id) => id,
        Base::Mix { to, .. } => to,
    };
    let order = &c.plan.snap.order;
    if inputs_only {
        for (n, id) in order.iter().take(16).enumerate() {
            let (x, y) = ((n % 4) as f32 * fw / 4.0, (n / 4) as f32 * fh / 4.0);
            let tally = if Some(*id) == program {
                Some(green)
            } else if Some(*id) == c.plan.preview {
                Some(orange)
            } else {
                None
            };
            cell(m, Some(*id), x, y, fw / 4.0, fh / 4.0, tally);
        }
    } else {
        cell(m, c.plan.preview, 0.0, 0.0, fw / 2.0, fh / 2.0, Some(orange));
        cell(m, program, fw / 2.0, 0.0, fw / 2.0, fh / 2.0, Some(green));
        for (n, id) in order.iter().take(8).enumerate() {
            let (x, y) = ((n % 4) as f32 * fw / 4.0, fh / 2.0 + (n / 4) as f32 * fh / 4.0);
            cell(m, Some(*id), x, y, fw / 4.0, fh / 4.0, None);
        }
    }
    out.make_opaque();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_fixture() -> (Plan, HashMap<u64, Arc<InputRt>>) {
        let mut p = Project::blank();
        p.out_w = 64;
        p.out_h = 36;
        p.inputs = vec![
            Input::new(1, "A", Kind::Scene),
            Input::new(2, "B", Kind::Scene)
                .with_layers(vec![Layer::new(1, Placement { x: 0.5, y: 0.5, scale: 0.5 })]),
        ];
        p.program = Some(2);
        let mut s = State::new(p);
        (s.plan(Instant::now(), Vec::new()), HashMap::new())
    }

    /// A thickness of 20 on a picture at 30 % is about 6 pixels at 1080p, with
    /// square corners, as saved projects expect.
    #[test]
    fn a_border_shrinks_with_its_picture_and_keeps_square_corners() {
        let mut out = Frame::solid(1920, 1080, [0, 0, 0]);
        let pic = Frame::solid(1920, 1080, [0, 0, 255]);
        let mut l = Layer::new(1, Placement { x: 0.5, y: 0.5, scale: 0.3 });
        l.border.thickness = 20.0;
        draw_layer(&mut out, &pic, &l, 255);
        let red = |x: usize, y: usize| out.data[(y * 1920 + x) * 4];
        let (x0, y0) = (960, 540);
        let width = (1..30).take_while(|n| red(x0 - n, y0 + 100) > 200).count();
        assert!((5..=7).contains(&width), "{width} pixels");
        assert!(red(x0 - 5, y0 - 5) > 200, "the outer corner is square");
    }

    #[test]
    fn a_border_keeps_its_proportions_on_a_card() {
        let border = Border { thickness: 20.0, ..Border::default() };
        let full = border_size(&border, 0.3, 1920.0).0;
        let card = border_size(&border, 0.3, 214.0).0;
        assert!((full / card - 1920.0 / 214.0).abs() < 0.01);
    }

    /// Off air costs little only if the sources are told: an input inside a
    /// scene on air counts, one nobody sees does not.
    #[test]
    fn only_what_can_be_seen_counts_as_showing() {
        let mut p = Project::blank();
        p.inputs = vec![
            Input::new(1, "Dedans", Kind::Bars),
            Input::new(2, "Scène", Kind::Scene).with_layers(vec![Layer::new(1, Placement::FULL)]),
            Input::new(3, "Aperçu", Kind::Bars),
            Input::new(4, "Rien", Kind::Bars),
            Input::new(5, "Vers une sortie", Kind::Bars),
        ];
        p.program = Some(2);
        p.preview = Some(3);
        let plan = State::new(p).plan(Instant::now(), vec![5]);
        assert_eq!(plan.showing, HashSet::from([1, 2, 3, 5]));
    }

    #[test]
    fn a_card_is_made_at_card_size_with_its_layers() {
        let mut p = Project::blank();
        p.out_w = 640;
        p.out_h = 360;
        p.inputs = vec![
            Input::new(1, "Source", Kind::Bars),
            Input::new(2, "Scène", Kind::Scene)
                .with_layers(vec![Layer::new(1, Placement { x: 0.5, y: 0.0, scale: 0.5 })]),
        ];
        let plan = State::new(p).plan(Instant::now(), Vec::new());
        let rt = InputRt::new(false, true);
        rt.publish(Frame::solid(640, 360, [255, 0, 0]));
        let rts = HashMap::from([(1, rt)]);
        let c = Ctx { plan: &plan, rts: &rts };
        let mut m = Mixer::default();
        let f = m.small_picture(2, &c, 0, (64, 36)).unwrap();
        assert_eq!((f.w, f.h), (64, 36));
        assert_eq!(f.data[(5 * 64 + 48) * 4], 255, "inside the layer");
        assert_eq!(f.data[(30 * 64 + 10) * 4 + 3], 0, "outside it");
        assert!(m.memo.is_empty(), "nothing composed at full size");
    }

    #[test]
    fn a_scene_is_transparent_where_it_has_no_layer() {
        let (plan, rts) = ctx_fixture();
        let c = Ctx { plan: &plan, rts: &rts };
        let mut m = Mixer::default();
        let f = m.picture(2, &c, 0).unwrap();
        assert_eq!(f.data[3], 0);
    }

    #[test]
    fn a_picture_is_reused_until_it_changes() {
        let (plan, rts) = ctx_fixture();
        let c = Ctx { plan: &plan, rts: &rts };
        let mut m = Mixer::default();
        let a = m.picture(2, &c, 0).unwrap();
        let b = m.picture(2, &c, 0).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn layer_loops_stop_instead_of_hanging() {
        let mut p = Project::blank();
        p.out_w = 32;
        p.out_h = 18;
        p.inputs = vec![
            Input::new(1, "A", Kind::Scene).with_layers(vec![Layer::new(2, Placement::FULL)]),
            Input::new(2, "B", Kind::Scene).with_layers(vec![Layer::new(1, Placement::FULL)]),
        ];
        let mut s = State::new(p);
        let plan = s.plan(Instant::now(), Vec::new());
        let rts = HashMap::new();
        let c = Ctx { plan: &plan, rts: &rts };
        assert!(Mixer::default().picture(1, &c, 0).is_some());
    }

    #[test]
    fn layer_crop_keeps_its_place() {
        let mut out = Frame::solid(100, 100, [0, 0, 0]);
        let pic = Frame::solid(100, 100, [255, 0, 0]);
        let mut l = Layer::new(1, Placement { x: 0.0, y: 0.0, scale: 1.0 });
        l.crop = [0.5, 0.0, 0.0, 0.0];
        draw_layer(&mut out, &pic, &l, 255);
        assert_eq!(out.data[(50 * 100 + 10) * 4], 0);
        assert_eq!(out.data[(50 * 100 + 80) * 4], 255);
    }

    #[test]
    fn overlay_picture_in_picture_stays_in_its_box() {
        let mut out = Frame::solid(100, 100, [0, 0, 0]);
        let pic = Frame::solid(100, 100, [0, 255, 0]);
        let cfg = OverlayCfg {
            pip: true,
            placement: Placement { x: 0.5, y: 0.5, scale: 0.25 },
            ..OverlayCfg::default()
        };
        draw_overlay(&mut out, &pic, &OverlayDraw { id: 1, t: 1.0, cfg });
        assert_eq!(out.data[(60 * 100 + 60) * 4 + 1], 255);
        assert_eq!(out.data[(10 * 100 + 10) * 4 + 1], 0);
    }

    /// Someone in front of a green screen, standing in the middle.
    fn presenter(w: usize, h: usize) -> Frame {
        let mut f = Frame::solid(w, h, [0, 255, 0]);
        f.fill_round(0.4 * w as f32, 0.2 * h as f32, 0.2 * w as f32, 0.8 * h as f32, 0.0, [200, 30, 30], 255);
        f
    }

    fn set_desk(camera: usize) -> (State, HashMap<u64, Arc<InputRt>>) {
        let mut p = Project::blank();
        p.out_w = 320;
        p.out_h = 180;
        let mut person = Input::new(1, "Présentatrice", Kind::Bars);
        person.key.mode = KeyMode::Chroma;
        let mut kind = sets::new_set(SetLook::Studio);
        if let Kind::VirtualSet { camera: c, .. } = &mut kind {
            *c = camera;
        }
        let set = Input::new(2, "Plateau", kind).with_layers(vec![Layer::new(1, sets::presenter_spot(0))]);
        p.inputs = vec![person, set];
        let rt = InputRt::new(false, true);
        rt.publish(presenter(320, 180));
        (State::new(p), HashMap::from([(1, rt)]))
    }

    fn px(f: &Frame, x: f32, y: f32) -> [u8; 4] {
        let i = (((y * f.h as f32) as usize) * f.w + (x * f.w as f32) as usize) * 4;
        [f.data[i], f.data[i + 1], f.data[i + 2], f.data[i + 3]]
    }

    fn red(p: [u8; 4]) -> bool {
        p[0] > 150 && p[1] < 80 && p[2] < 80
    }

    /// Width of the presenter across the middle row, in pixels.
    fn body_width(f: &Frame) -> usize {
        (0..f.w).filter(|&x| red(px(f, (x as f32 + 0.5) / f.w as f32, 0.5))).count()
    }

    fn set_picture_at(s: &mut State, rts: &HashMap<u64, Arc<InputRt>>, now: Instant) -> Arc<Frame> {
        let plan = s.plan(now, Vec::new());
        let c = Ctx { plan: &plan, rts };
        Mixer::default().picture(2, &c, 0).unwrap()
    }

    #[test]
    fn a_keyed_presenter_stands_between_the_decor_and_the_desk() {
        let (mut s, rts) = set_desk(0);
        let f = set_picture_at(&mut s, &rts, Instant::now());
        let back = sets::backdrop(SetLook::Studio, 320, 180).unwrap();
        assert!(red(px(&f, 0.5, 0.5)), "the presenter shows: {:?}", px(&f, 0.5, 0.5));
        assert_eq!(px(&f, 0.3, 0.5), px(&back, 0.3, 0.5), "the green around the presenter is gone");
        assert_eq!(&px(&f, 0.5, 0.72)[..3], &[226, 230, 238], "the desk hides the presenter's legs");
        assert!(f.data.chunks(4).all(|p| p[3] == 255), "a set is opaque");
        let width = body_width(&f);
        assert!((36..=40).contains(&width), "at full width the body is 12 % of the set: {width}");

        let plan = s.plan(Instant::now(), Vec::new());
        let c = Ctx { plan: &plan, rts: &rts };
        let card = Mixer::default().small_picture(2, &c, 0, (64, 36)).unwrap();
        assert!(red(px(&card, 0.5, 0.5)) && !red(px(&card, 0.3, 0.5)), "the card shows the same set");
    }

    #[test]
    fn a_close_preset_enlarges_the_set_around_its_centre() {
        let (mut wide, rts) = set_desk(0);
        let (mut close, _) = set_desk(2);
        let a = set_picture_at(&mut wide, &rts, Instant::now());
        let b = set_picture_at(&mut close, &rts, Instant::now());
        let ratio = body_width(&b) as f32 / body_width(&a) as f32;
        assert!((ratio - 2.4).abs() < 0.15, "the close-up zooms 2.4 times: {ratio}");
        assert!(red(px(&b, 0.38, 0.45)) && !red(px(&a, 0.38, 0.45)));
        assert!(b.data.chunks(4).all(|p| p[3] == 255), "no edge of the set shows when zoomed");
    }

    /// Each frame of a move is a little closer than the last, with no jump,
    /// and the last one is the preset itself.
    #[test]
    fn a_camera_move_passes_smoothly_through_the_views_between() {
        let (mut s, rts) = set_desk(0);
        let (mut close, _) = set_desk(2);
        let target = body_width(&set_picture_at(&mut close, &rts, Instant::now()));
        let t0 = Instant::now();
        s.set_camera(2, 2, Some(1000), t0);
        let mut widths = Vec::new();
        for n in 0..=20 {
            let now = t0 + Duration::from_millis(n * 50);
            s.advance(now, 0.05);
            widths.push(body_width(&set_picture_at(&mut s, &rts, now)));
        }
        assert!(s.set_moves.is_empty(), "the move is over");
        assert_eq!(widths[20], target, "{widths:?}");
        for w in widths.windows(2) {
            assert!(w[1] >= w[0], "the camera goes back: {widths:?}");
            assert!(w[1] - w[0] <= 10, "a jump of {} pixels: {widths:?}", w[1] - w[0]);
        }
        // The body grows two pixels at a time, one on each side.
        let largest = widths.windows(2).map(|w| w[1] - w[0]).max().unwrap();
        let (first, last) = (widths[1] - widths[0], widths[20] - widths[19]);
        assert!(first <= 2 && last <= 2 && largest >= 4, "starts and stops gently: {widths:?}");
    }
}
