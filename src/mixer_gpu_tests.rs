//! The graphics card must make the pictures the processor makes. Each test
//! composes the same desk both ways and compares, pixel for pixel; without a
//! card it says so and checks nothing.

use super::*;
use std::path::PathBuf;

/// Any card will do here, emulated ones included: only pictures are checked.
fn card() -> Option<gpu::Painter> {
    match gpu::Painter::open(true) {
        Ok(p) => Some(p),
        Err(why) => {
            eprintln!("comparaison sautée, pas de carte graphique : {why}");
            None
        }
    }
}

fn on_card(painter: gpu::Painter) -> Mixer {
    Mixer {
        gpu: Some(Box::new(OnGpu { painter, pics: Pictures::default(), program: None })),
        ..Mixer::default()
    }
}

/// Smooth gradients, sharp edges and see-through parts, premultiplied.
fn picture(w: usize, h: usize, seed: u32, alpha: bool) -> Frame {
    let mut f = Frame::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let r = ((x * 255 / w.max(1)) as u32 + seed * 40) % 256;
            let g = ((y * 255 / h.max(1)) as u32 + seed * 70) % 256;
            let b = (((x + y) / 7) as u32 % 2) * 200 + seed % 50;
            let a = if !alpha {
                255
            } else if y < h / 3 {
                0
            } else if x < w / 2 {
                ((x * 255) / (w / 2).max(1)) as u32
            } else {
                255
            };
            let i = (y * w + x) * 4;
            let px = [r * a / 255, g * a / 255, b.min(255) * a / 255, a];
            f.data[i..i + 4].copy_from_slice(&px.map(|v| v as u8));
        }
    }
    f
}

/// A person-like block on a green screen.
fn green_screen(w: usize, h: usize) -> Frame {
    let mut f = Frame::solid(w, h, [20, 200, 40]);
    for y in h / 4..h {
        for x in w / 3..w * 2 / 3 {
            let i = (y * w + x) * 4;
            let edge = (x == w / 3 || x + 1 == w * 2 / 3) as u8;
            f.data[i..i + 3].copy_from_slice(&[200 - edge * 80, 150 + edge * 60, 120]);
        }
    }
    f
}

struct Desk {
    state: State,
    rts: HashMap<u64, Arc<InputRt>>,
}

const SCENE: u64 = 20;
const ALL_SETTINGS: u64 = 21;
const SET: u64 = 22;

/// Files the settings read, written once per size.
fn files(w: usize) -> (String, String) {
    let dir = std::env::temp_dir().join(format!("livecraft-gpu-{}-{w}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cube: PathBuf = dir.join("chaud.cube");
    let mut text = String::from("LUT_3D_SIZE 5\n");
    for b in 0..5 {
        for g in 0..5 {
            for r in 0..5 {
                let (r, g, b) = (r as f32 / 4.0, g as f32 / 4.0, b as f32 / 4.0);
                text += &format!("{} {} {}\n", r.powf(0.8), g * 0.95 + 0.03, b * 0.85);
            }
        }
    }
    std::fs::write(&cube, text).unwrap();
    let mask = dir.join("masque.png");
    let img = image::RgbaImage::from_fn(64, 36, |x, y| image::Rgba([0, 0, 0, ((x * 4 + y * 2) % 256) as u8]));
    img.save(&mask).unwrap();
    (cube.to_string_lossy().into_owned(), mask.to_string_lossy().into_owned())
}

/// Every kind of setting the card does itself, plus one it leaves to the
/// processor (blur).
fn desk(w: usize, h: usize) -> Desk {
    let (cube, mask) = files(w);
    let input = |id, name: &str| Input::new(id, name, Kind::Bars);
    let mut inputs = vec![input(1, "A"), input(2, "B"), input(3, "Titre"), input(4, "Vert")];
    let mut add = |id: u64, edit: &dyn Fn(&mut Input)| {
        let mut i = input(id, &format!("Réglé {id}"));
        i.key.fill_input = Some(1);
        edit(&mut i);
        inputs.push(i);
    };
    add(10, &|i| {
        i.position = Position { zoom: 1.3, pan_x: 0.2, pan_y: -0.1, ..Position::default() };
        i.colour = Colour {
            saturation: 1.4,
            hue: 25.0,
            lift: [0.05, 0.0, 0.02],
            gamma: [1.2, 1.0, 0.9],
            ..Colour::default()
        };
    });
    add(11, &|i| {
        i.position = Position { zoom: 0.8, rotate: 0.4, crop: [0.1, 0.05, 0.0, 0.1], ..Position::default() };
        i.mirror = true;
    });
    add(12, &|i| i.position = Position { quarter_turns: 1, ..Position::default() });
    add(13, &|i| i.key = Key { mode: KeyMode::Chroma, color: [20, 200, 40], ..Key::default() });
    add(14, &|i| i.key = Key { mode: KeyMode::Colour, color: [200, 0, 0], tolerance: 0.2, ..Key::default() });
    add(15, &|i| i.key = Key { mode: KeyMode::Luma, tolerance: 0.3, softness: 0.2, ..Key::default() });
    add(16, &|i| i.key = Key { mode: KeyMode::Fill, fill_input: Some(1), ..Key::default() });
    add(17, &|i| {
        i.colour = Colour { white: 0.8, black: 0.05, alpha: 0.7, rgb: [1.0, 0.9, 1.1], ..Colour::default() };
        i.effects = vec![Effect {
            kind: EffectKind::Lut,
            enabled: true,
            strength: 0.7,
            path: cube.clone(),
            crop: [0.1, 0.2, 0.0, 0.3],
        }];
    });
    add(18, &|i| {
        let lut = Effect {
            kind: EffectKind::Lut,
            enabled: true,
            strength: 1.0,
            path: cube.clone(),
            ..Effect::default()
        };
        let cut =
            Effect { kind: EffectKind::AlphaMask, enabled: true, path: mask.clone(), ..Effect::default() };
        i.effects = vec![lut, cut];
    });
    add(19, &|i| {
        i.effects =
            vec![Effect { kind: EffectKind::Blur, enabled: true, strength: 0.3, ..Effect::default() }];
    });
    let mut pip = Layer::new(2, Placement { x: 0.55, y: 0.08, scale: 0.37 });
    pip.crop = [0.05, 0.1, 0.0, 0.07];
    pip.border = Border { color: [250, 200, 10], thickness: 9.0, radius: 24.0 };
    let mut keyed = Layer::new(13, Placement { x: 0.02, y: 0.4, scale: 0.5 });
    keyed.border.radius = 30.0;
    let layers = vec![Layer::new(1, Placement::FULL), pip, keyed];
    inputs.push(Input::new(SCENE, "Scène", Kind::Scene).with_layers(layers));
    let layers =
        vec![Layer::new(11, Placement::FULL), Layer::new(3, Placement { x: 0.1, y: 0.1, scale: 0.8 })];
    let mut graded = Input::new(ALL_SETTINGS, "Scène réglée", Kind::Scene).with_layers(layers);
    graded.colour = Colour { saturation: 0.6, ..Colour::default() };
    inputs.push(graded);
    // The keyed presenter in a virtual set, its camera on the close-up.
    let mut kind = sets::new_set(SetLook::Studio);
    if let Kind::VirtualSet { camera, .. } = &mut kind {
        *camera = 2;
    }
    let presenter = vec![Layer::new(13, sets::presenter_spot(0))];
    inputs.push(Input::new(SET, "Plateau", kind).with_layers(presenter));

    let mut p = Project::blank();
    p.out_w = w;
    p.out_h = h;
    p.inputs = inputs;
    p.program = Some(SCENE);
    p.preview = Some(ALL_SETTINGS);
    let rts = p
        .inputs
        .iter()
        .filter(|i| !matches!(i.kind, Kind::Scene | Kind::VirtualSet { .. }))
        .map(|i| {
            let rt = InputRt::new(false, true);
            let frame = match i.id {
                1 => picture(w, h, 1, false),
                // Not at the output size: drawn scaled.
                2 => picture(w * 2 / 3, h * 2 / 3, 2, false),
                3 => picture(w, h, 3, true),
                4 | 13 => green_screen(w, h),
                id => picture(w, h, id as u32, id % 2 == 0),
            };
            rt.publish(frame);
            (i.id, rt)
        })
        .collect();
    Desk { state: State::new(p), rts }
}

/// How far apart two pictures are: the share of pixels off by more than
/// `step` in any channel, the mean difference, and the worst.
fn distance(cpu: &Frame, gpu: &Frame, step: u8) -> (f64, f64, u8) {
    assert_eq!((cpu.w, cpu.h), (gpu.w, gpu.h));
    let (mut off, mut sum, mut worst) = (0usize, 0u64, 0u8);
    for (a, b) in cpu.data.as_chunks::<4>().0.iter().zip(gpu.data.as_chunks::<4>().0) {
        let d = (0..4).map(|k| a[k].abs_diff(b[k])).max().unwrap();
        off += (d > step) as usize;
        sum += (0..4).map(|k| a[k].abs_diff(b[k]) as u64).sum::<u64>();
        worst = worst.max(d);
    }
    let n = (cpu.w * cpu.h) as f64;
    (off as f64 / n, sum as f64 / (n * 4.0), worst)
}

/// The card rounds where the processor truncates, a step per blend at most,
/// so a few steps where blends stack up; an edge a hair's breadth from a
/// pixel centre may fall either way. Anything more is a real difference.
fn assert_close(what: &str, cpu: &Frame, gpu: &Frame) {
    let (off, mean, worst) = distance(cpu, gpu, 3);
    let pct = off * 100.0;
    assert!(
        off < 0.001 && mean < 0.5,
        "{what} : {pct:.3} % des pixels diffèrent, écart moyen {mean:.3}, pire {worst}"
    );
}

fn program(m: &mut Mixer, c: &Ctx) -> Frame {
    m.compose_program(c).or_else(|| m.collect_program(c)).unwrap()
}

#[test]
fn every_input_and_its_settings_look_the_same_on_the_card() {
    let Some(painter) = card() else { return };
    // An odd size: rows do not line up with the card's own.
    let mut d = desk(334, 190);
    let plan = d.state.plan(Instant::now(), Vec::new());
    let c = Ctx { plan: &plan, rts: &d.rts };
    let (mut cpu, mut gpu) = (Mixer::default(), on_card(painter));
    let ids: Vec<u64> = plan.snap.order.clone();
    assert!(ids.len() >= 16);
    for id in ids {
        let name = &plan.snap.inputs[&id].name;
        let (a, b) = (cpu.picture(id, &c, 0).unwrap(), gpu.picture(id, &c, 0).unwrap());
        assert_close(name, &a, &b);
    }
}

#[test]
fn the_output_looks_the_same_on_the_card_with_overlays_and_fade_to_black() {
    let Some(painter) = card() else { return };
    let mut d = desk(1280, 720);
    let effects = [TransKind::Fade, TransKind::Zoom, TransKind::Wipe, TransKind::Slide];
    for (n, effect) in effects.into_iter().enumerate() {
        d.state.project.overlay_cfg[n] = OverlayCfg {
            pip: n % 2 == 1,
            effect,
            border: Border { thickness: 12.0, radius: 20.0, ..Border::default() },
            ..OverlayCfg::default()
        };
        d.state.overlay_shown[n] = Some([3, 10, 13, 18][n]);
        d.state.overlay_t[n] = [0.5, 0.7, 0.35, 1.0][n];
    }
    d.state.ftb_level = 0.3;
    let plan = d.state.plan(Instant::now(), Vec::new());
    let c = Ctx { plan: &plan, rts: &d.rts };
    let (mut cpu, mut gpu) = (Mixer::default(), on_card(painter));
    assert_close("sortie", &program(&mut cpu, &c), &program(&mut gpu, &c));
}

#[test]
fn every_transition_looks_the_same_on_the_card() {
    let Some(painter) = card() else { return };
    let mut d = desk(334, 190);
    let (mut cpu, mut gpu) = (Mixer::default(), on_card(painter));
    for kind in TransKind::ALL {
        for t in [0.2, 0.5, 0.85] {
            d.state.project.trans[0].kind = kind;
            d.state.tbar = t;
            let plan = d.state.plan(Instant::now(), Vec::new());
            let c = Ctx { plan: &plan, rts: &d.rts };
            assert_close(&format!("{kind:?} à {t}"), &program(&mut cpu, &c), &program(&mut gpu, &c));
        }
    }
}

/// Opening the card takes up to a second, and the picture must not wait for
/// it: the processor composes until it is open, then the card takes over.
#[test]
fn the_processor_composes_until_the_card_is_open() {
    let mut d = desk(334, 190);
    let plan = d.state.plan(Instant::now(), Vec::new());
    let c = Ctx { plan: &plan, rts: &d.rts };
    let mut m = Mixer::default();
    assert_eq!(m.choose_painter(true, || None), Some(Compositor::Starting));
    assert!(m.gpu.is_none() && !m.gpu_refused, "the card is still awaited");
    let meanwhile = m.compose_program(&c).expect("composed at once by the processor");
    let Some(painter) = card() else { return };
    assert!(matches!(m.choose_painter(true, || Some(Ok(painter))), Some(Compositor::Gpu(_))));
    assert!(m.gpu.is_some());
    assert_close("une fois la carte ouverte", &meanwhile, &program(&mut m, &c));
}

/// A source frame stays on the card only while something drawn from it is
/// kept. Held any longer, every frame of a corrected live preview stayed in
/// memory and on the card until the output next changed: under a still output
/// the card ran out of memory, and decoders never got their buffers back.
#[test]
fn a_live_preview_under_a_still_output_lets_its_frames_go() {
    let Some(painter) = card() else { return };
    let (w, h) = (64, 36);
    let mut d = desk(w, h);
    let mut m = on_card(painter);
    // Zoomed and corrected: drawn from its source frame, then processed.
    let rt = d.rts[&10].clone();
    for n in 0..10 {
        let plan = d.state.plan(Instant::now(), Vec::new());
        let c = Ctx { plan: &plan, rts: &d.rts };
        m.picture(10, &c, 0).expect("the preview");
        let given_back = rt.publish(picture(w, h, n, false));
        assert!(given_back.is_some(), "frame {n} still held once drawn");
    }
}

/// Every engine in a process shares one card, and the thread that runs a
/// picture's "ready" callback may be another one polling the card, a moment
/// after this one stopped waiting. Each must still get its own picture back,
/// and the card must not count as broken.
#[test]
fn painters_on_several_threads_each_get_their_own_pictures_back() {
    if card().is_none() {
        return;
    }
    let threads: Vec<_> = (0..8u32)
        .map(|n| {
            thread::spawn(move || {
                let mut p = gpu::Painter::open(true).expect("the card");
                for k in 0..40 {
                    let sent = Arc::new(picture(96, 54, n * 100 + k, k % 2 == 0));
                    let mut c = p.canvas(96, 54);
                    c.copy_from(&p.source(sent.clone()));
                    let made = p.done(c);
                    let back = p.frame(&made);
                    assert!(back.data == sent.data, "painter {n}, picture {k}: {:?}", p.failed());
                }
                assert_eq!(p.failed(), None);
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
}

/// A card given up is left at once: the rest of the tick is made by the
/// processor, nothing waits on the card again, the next tick says why, and
/// ticking the setting again tries a card anew.
#[test]
fn a_card_given_up_leaves_the_rest_of_the_tick_to_the_processor() {
    if card().is_none() {
        return;
    }
    let mut d = desk(334, 190);
    let plan = d.state.plan(Instant::now(), Vec::new());
    let c = Ctx { plan: &plan, rts: &d.rts };
    let mut cpu = Mixer::default();
    let (out, preview) = (program(&mut cpu, &c), cpu.picture(ALL_SETTINGS, &c, 0).unwrap());
    let mut m = on_card(gpu::Painter::open_alone(true).expect("a card of its own"));
    assert!(m.compose_program(&c).is_none(), "on its way back from the card");
    m.gpu.as_ref().unwrap().painter.give_up("ne répond plus");
    let back = m.collect_program(&c).unwrap();
    assert!(m.gpu.is_none(), "the card is left at once");
    assert!(back.data == out.data, "the output is made again by the processor");
    assert!(m.picture(ALL_SETTINGS, &c, 0).unwrap().data == preview.data);
    let said = m.choose_painter(true, || panic!("not asked again until the setting changes"));
    assert_eq!(said, Some(Compositor::Cpu("ne répond plus".into())));
    assert_eq!(m.choose_painter(true, || panic!("still not asked")), None);
    assert_eq!(m.choose_painter(false, || None), Some(Compositor::Cpu(t!("mixer.cpu_chosen").into())));
    let again = card().unwrap();
    assert!(matches!(m.choose_painter(true, || Some(Ok(again))), Some(Compositor::Gpu(_))));

    // Found gone by the preview while the output is on its way back: the
    // output still comes, made by the processor.
    let mut m = on_card(gpu::Painter::open_alone(true).expect("a card of its own"));
    assert!(m.compose_program(&c).is_none());
    m.gpu.as_ref().unwrap().painter.give_up("perdue");
    assert!(m.picture(ALL_SETTINGS, &c, 0).unwrap().data == preview.data);
    assert!(m.collect_program(&c).expect("the output of this tick").data == out.data);
    assert!(m.collect_program(&c).is_none(), "owed once only");
}

/// Once the card is given up, a picture asked of it comes back black at
/// once, never the leftovers of the buffer it lands in.
#[test]
fn a_picture_asked_of_a_card_given_up_is_black() {
    if card().is_none() {
        return;
    }
    let mut p = gpu::Painter::open_alone(true).expect("a card of its own");
    let mut c = p.canvas(640, 480);
    c.fill_round(0.0, 0.0, 640.0, 480.0, 0.0, [200, 30, 30], 255);
    let pic = p.done(c);
    p.give_up("ne répond plus");
    // Leftovers of that size, ready to be taken again.
    drop(Frame::solid(640, 480, [255, 0, 255]));
    let f = p.frame(&pic);
    assert!(f.data.iter().all(|&v| v == 0), "black");
}

/// Pictures of several sizes on their way back at once each arrive whole,
/// however the buffers they travel through are reshuffled meanwhile.
#[test]
fn pictures_of_several_sizes_on_their_way_back_each_arrive() {
    fn solid(p: &mut gpu::Painter, w: usize, h: usize, rgb: [u8; 3]) -> gpu::Readback {
        let mut c = p.canvas(w, h);
        c.fill_round(0.0, 0.0, w as f32, h as f32, 0.0, rgb, 255);
        p.start_readback(c)
    }
    if card().is_none() {
        return;
    }
    let mut p = gpu::Painter::open_alone(true).expect("a card of its own");
    let first = solid(&mut p, 32, 32, [9, 9, 9]);
    let red = solid(&mut p, 48, 27, [200, 0, 0]);
    p.finish_readback(first, &mut Frame::new(1, 1));
    // A third size: the buffer of the first, free again, is let go.
    let blue = solid(&mut p, 64, 36, [0, 0, 200]);
    let (mut fr, mut fb) = (Frame::new(1, 1), Frame::new(1, 1));
    p.finish_readback(red, &mut fr);
    p.finish_readback(blue, &mut fb);
    assert_eq!(p.failed(), None);
    assert!((fr.w, fr.h) == (48, 27) && fr.data.chunks(4).all(|px| px == [200, 0, 0, 255]));
    assert!((fb.w, fb.h) == (64, 36) && fb.data.chunks(4).all(|px| px == [0, 0, 200, 255]));
}

/// Idle pictures and upload buffers are let go beyond the card's budget: a
/// burst of large pictures does not keep the card's memory afterwards.
#[test]
fn idle_pictures_on_the_card_stay_within_its_budget() {
    if card().is_none() {
        return;
    }
    let mut p = gpu::Painter::open_alone(true).expect("a card of its own");
    let budget = 1 << 20;
    p.set_spare_bytes(budget);
    // Twelve 256 x 256 sources and pictures alive at once, 256 KB each.
    let frames: Vec<_> = (0..12).map(|n| Arc::new(picture(256, 256, n, false))).collect();
    let mut held = Vec::new();
    for f in &frames {
        let mut c = p.canvas(256, 256);
        c.copy_from(&p.source(f.clone()));
        held.push(p.done(c));
    }
    p.frame(&held[0]);
    drop((held, frames));
    // Quieter frames follow.
    for _ in 0..6 {
        let c = p.canvas(16, 16);
        let pic = p.done(c);
        p.frame(&pic);
    }
    assert!(p.idle_bytes() <= budget, "{} bytes idle", p.idle_bytes());
}

/// A frame larger than the card takes is made smaller on the way, and the
/// picture has the size of its copy on the card.
#[test]
fn a_frame_too_large_for_the_card_has_the_size_of_its_copy() {
    if card().is_none() {
        return;
    }
    let mut p = gpu::Painter::open_alone(true).expect("a card of its own");
    let max = p.max_side();
    let pic = p.source(Arc::new(Frame::solid(max + 64, 2, [200, 0, 0])));
    let (w, h) = gpu::Canvas::pic_size(&pic);
    assert!(w <= max && h >= 1, "{w} x {h}");
    let mut c = p.canvas(w, h);
    c.copy_from(&pic);
    let made = p.done(c);
    let back = p.frame(&made);
    assert_eq!(p.failed(), None);
    assert_eq!((back.w, back.h), (w, h));
}

/// The comparison itself must see a wrong picture: one that always passes
/// would let the card show anything.
#[test]
fn the_comparison_catches_a_shifted_picture() {
    let a = picture(334, 190, 1, false);
    let mut b = Frame::new(334, 190);
    b.data[8..].copy_from_slice(&a.data[..a.data.len() - 8]);
    assert!(distance(&a, &b, 2).0 > 0.1);
}

/// `cargo test mixer_cost -- --ignored --nocapture`: what composing
/// costs the processor at 1080p and 4K, on the processor and on the card.
#[cfg(windows)]
#[test]
#[ignore = "measure: run alone"]
fn mixer_cost() {
    // As many threads as the program itself starts.
    let cores = thread::available_parallelism().map_or(4, |n| n.get());
    let pool = rayon::ThreadPoolBuilder::new().num_threads((cores / 2).max(2)).build().unwrap();
    pool.install(|| {
        for (w, h) in [(1920, 1080), (3840, 2160)] {
            for on_gpu in [false, true] {
                let mut m = if on_gpu {
                    match gpu::Painter::open(false) {
                        Ok(p) => on_card(p),
                        Err(why) => {
                            println!("{w}x{h} carte graphique : {why}");
                            continue;
                        }
                    }
                } else {
                    Mixer::default()
                };
                let mut d = desk(w, h);
                // Three cameras sending a new picture every frame, one of
                // them corrected and zoomed in the preview.
                let live = [1u64, 2, 10];
                // Pictures go round as a decoder's buffers do: one nobody
                // reads any more is filled again, nothing is copied.
                let mut spare: Vec<Vec<Frame>> =
                    live.iter().map(|&id| (0..4).map(|n| picture(w, h, id as u32 * 10 + n, false)).collect()).collect();
                let mut retired: Vec<Vec<Arc<Frame>>> = vec![Vec::new(); live.len()];
                let template = picture(w, h, 99, false);
                d.state.project.preview = Some(10);
                d.state.overlay_shown[0] = Some(3);
                d.state.overlay_t[0] = 1.0;
                let phases = [
                    ("scène + titre", None),
                    ("fondu", Some(TransKind::Fade)),
                    ("cube", Some(TransKind::Cube)),
                    ("fusion", Some(TransKind::Merge)),
                ];
                for (name, kind) in phases {
                    let (warm, ticks) = (10, 120);
                    let (mut c0, mut t0) = (process_cpu(), Instant::now());
                    for n in 0..warm + ticks {
                        // The first ticks fill the pools.
                        if n == warm {
                            (c0, t0) = (process_cpu(), Instant::now());
                        }
                        for (k, id) in live.iter().enumerate() {
                            let free = retired[k].iter().position(|a| Arc::strong_count(a) == 1);
                            let next = free
                                .and_then(|i| Arc::try_unwrap(retired[k].swap_remove(i)).ok())
                                .or_else(|| spare[k].pop())
                                .unwrap_or_else(|| template.clone());
                            retired[k].extend(d.rts[id].latest());
                            d.rts[id].publish(next);
                        }
                        if let Some(kind) = kind {
                            d.state.project.trans[0].kind = kind;
                            d.state.tbar = (n as f32 / (warm + ticks) as f32).clamp(0.01, 0.99);
                        }
                        let plan = d.state.plan(Instant::now(), Vec::new());
                        let c = Ctx { plan: &plan, rts: &d.rts };
                        let out = m.compose_program(&c);
                        let preview = m.picture(10, &c, 0);
                        let out = out.or_else(|| m.collect_program(&c));
                        assert!(out.is_some() && preview.is_some());
                        m.pool.extend(out.map(Arc::new));
                    }
                    let per = |d: Duration| d.as_secs_f64() * 1000.0 / ticks as f64;
                    let side = if on_gpu { "carte" } else { "processeur" };
                    let (cpu, wall) = (per(process_cpu() - c0), per(t0.elapsed()));
                    println!("{w}x{h} {side:<10} {name:<14} processeur {cpu:6.2} ms/image, durée {wall:6.2} ms/image");
                }
            }
        }
    });
}

/// `cargo test engine_cost -- --ignored --nocapture`: the whole
/// program running a real desk, two looping videos in a scene with a title
/// on top and a corrected preview, on the processor then on the card.
#[cfg(windows)]
#[test]
#[ignore = "measure: run alone, needs FFmpeg"]
fn engine_cost() {
    use crate::engine::Engine;
    use std::process::Stdio;
    for (w, h) in [(1920, 1080), (3840, 2160)] {
        let dir = std::env::temp_dir().join(format!("livecraft-cost-{w}"));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = |name: &str, filter: &str| {
            let path = dir.join(name).to_string_lossy().into_owned();
            if !std::path::Path::new(&path).exists() {
                let made = crate::sources::ffmpeg()
                    .args(["-y", "-f", "lavfi", "-i", &format!("testsrc2=size={w}x{h}:rate=30:duration=8")])
                    .args(["-vf", filter, "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p"])
                    .arg(&path)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .unwrap();
                assert!(made.success());
            }
            Kind::Video { path, looped: true, mark_in: 0, mark_out: 0, rate: 1.0 }
        };
        let mut p = Project::blank();
        p.out_w = w;
        p.out_h = h;
        p.fps = 30;
        let mut b = Input::new(2, "Caméra 2", clip("b.mp4", "hue=h=120,vflip"));
        b.colour = Colour { saturation: 1.2, gamma: [1.1; 3], ..Colour::default() };
        let title = Kind::Title {
            text: "Titre de l'émission".into(),
            sub: "Sous-titre".into(),
            style: TitleStyle::LowerThird,
            color: [255, 255, 255],
            accent: [240, 120, 40],
            ticker_speed: 4.0,
            countdown_ms: 0,
            font: String::new(),
            size: 0.0,
            presets: Vec::new(),
            data: None,
        };
        let mut pip = Layer::new(2, Placement { x: 0.62, y: 0.06, scale: 0.33 });
        pip.border = Border { thickness: 8.0, radius: 16.0, ..Border::default() };
        p.inputs = vec![
            Input::new(1, "Caméra 1", clip("a.mp4", "null")),
            b,
            Input::new(3, "Titre", title),
            Input::new(4, "Scène", Kind::Scene).with_layers(vec![Layer::new(1, Placement::FULL), pip]),
        ];
        p.program = Some(4);
        p.preview = Some(2);
        p.overlays[0] = Some(3);
        let engine = Engine::start(p);
        for id in [1, 2] {
            engine.rt(id).unwrap().playing.store(true, Ordering::Relaxed);
        }
        for on_gpu in [false, true] {
            engine.use_gpu.store(on_gpu, Ordering::Relaxed);
            thread::sleep(Duration::from_secs(4));
            let (c0, t0) = (process_cpu(), Instant::now());
            let (mut mix, mut samples) = (0u64, 0u64);
            while t0.elapsed() < Duration::from_secs(12) {
                thread::sleep(Duration::from_millis(250));
                mix += engine.stats.mix_us.load(Ordering::Relaxed);
                samples += 1;
            }
            let cores = (process_cpu() - c0).as_secs_f64() / t0.elapsed().as_secs_f64();
            let rate = engine.stats.ticks_per_sec.load(Ordering::Relaxed);
            let made = engine.stats.composed_per_sec.load(Ordering::Relaxed);
            let by = engine.compositor.lock().unwrap().clone();
            println!(
                "{w}x{h} {by:?}: LiveCraft {:.2} cœurs, rendu {:.2} ms, {rate} i/s, {made} images composées/s",
                cores,
                mix as f64 / samples as f64 / 1000.0
            );
        }
        engine.shutdown();
        thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(windows)]
fn process_cpu() -> Duration {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut a, mut b, mut k, mut u) = (zero, zero, zero, zero);
    // SAFETY: four valid FILETIME out-pointers for the current process.
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut a, &mut b, &mut k, &mut u) };
    let n = |f: &FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    Duration::from_nanos((n(&k) + n(&u)) * 100)
}
