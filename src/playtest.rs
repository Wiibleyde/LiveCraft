//! Second half of `--selftest`: everything with a timeline, on real files
//! decoded by real ffmpeg. Each step states what a person would expect.

use crate::engine::Engine;
use crate::frame::Frame;
use crate::functions::Args;
use crate::model::*;
use crate::output::{Encoder, Spec, Target};
use crate::replay::{Camera, ReplayState};
use crate::sources::{InputRt, ffmpeg};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::sleep;
use std::time::{Duration, Instant};

pub(crate) fn make(args: &[&str], out: &str) -> Result<String, String> {
    let done = ffmpeg()
        .arg("-y")
        .args(args)
        .arg(out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| e.to_string())?;
    if done.success() { Ok(out.to_string()) } else { Err(format!("impossible de créer {out}")) }
}

fn clip(dir: &str, name: &str, seconds: u32) -> Result<String, String> {
    let picture = format!("testsrc2=size=320x180:rate=30:duration={seconds}");
    let sound = format!("sine=frequency=440:duration={seconds}");
    let args = [
        "-f",
        "lavfi",
        "-i",
        &picture,
        "-f",
        "lavfi",
        "-i",
        &sound,
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
    ];
    make(&args, &format!("{dir}/{name}.mp4"))
}

pub(crate) fn until(seconds: f32, what: &str, mut done: impl FnMut() -> bool) -> Result<(), String> {
    let end = Instant::now() + Duration::from_secs_f32(seconds);
    while !done() {
        if Instant::now() > end {
            return Err(format!("attendu sans succès : {what}"));
        }
        sleep(Duration::from_millis(15));
    }
    Ok(())
}

fn pos(rt: &InputRt) -> u64 {
    rt.pos_ms.load(Ordering::Relaxed)
}

fn done(rt: &InputRt) -> u64 {
    rt.completions.load(Ordering::Relaxed)
}

struct Desk {
    engine: Arc<Engine>,
}

impl Desk {
    fn add(&self, name: &str, kind: Kind) -> Result<(u64, Arc<InputRt>), String> {
        let id = self.engine.new_id();
        self.engine.add_input(Input::new(id, name, kind));
        let rt = self.engine.rt(id).ok_or("entrée absente")?;
        Ok((id, rt))
    }

    fn call(&self, function: &str, input: &str, value: &str) -> Result<(), String> {
        self.engine.call(function, &Args::new().input(input).value(value))
    }

    fn marks(&self, path: &str) -> Result<(), String> {
        let kind = Kind::Video { path: path.into(), looped: false, mark_in: 1000, mark_out: 2000, rate: 1.0 };
        let (_, rt) = self.add("Marques", kind)?;
        until(5.0, "première image avec marques", || rt.seq() >= 1)?;
        if pos(&rt) != 1000 || rt.is_playing() {
            return Err(format!("doit attendre au point d'entrée, pas à {}", pos(&rt)));
        }
        self.call("Play", "Marques", "")?;
        until(5.0, "fin au point de sortie", || done(&rt) == 1)?;
        if rt.is_playing() || !(1000..=2300).contains(&pos(&rt)) {
            return Err(format!("arrêt au mauvais endroit : {}", pos(&rt)));
        }
        self.call("Restart", "Marques", "")?;
        until(3.0, "retour au point d'entrée", || pos(&rt) == 1000)?;
        if rt.is_playing() {
            return Err("revenir au début ne doit pas relancer la lecture".into());
        }
        self.call("Play", "Marques", "")?;
        until(5.0, "seconde lecture", || done(&rt) == 2)
    }

    fn resume(&self, path: &str) -> Result<(), String> {
        let kind = Kind::Video { path: path.into(), looped: false, mark_in: 0, mark_out: 0, rate: 1.0 };
        let (id, rt) = self.add("Reprise", kind)?;
        until(5.0, "première image", || rt.seq() >= 1)?;
        self.call("Play", "Reprise", "")?;
        until(5.0, "lecture", || pos(&rt) >= 800)?;
        self.call("Pause", "Reprise", "")?;
        sleep(Duration::from_millis(100));
        let stood = pos(&rt);
        self.engine.restart_input(id, true);
        let rt = self.engine.rt(id).ok_or("entrée absente")?;
        until(5.0, "image après réglage", || rt.seq() >= 1)?;
        if rt.is_playing() || pos(&rt).abs_diff(stood) > 100 {
            return Err(format!("un réglage a déplacé la lecture : {stood} puis {}", pos(&rt)));
        }
        self.call("SetPosition", "Reprise", "3000")?;
        until(3.0, "déplacement en pause", || pos(&rt) == 3000)?;
        sleep(Duration::from_millis(200));
        if rt.is_playing() || pos(&rt) != 3000 {
            return Err("déplacer en pause doit rester en pause".into());
        }
        self.call("Play", "Reprise", "")?;
        until(4.0, "fin du fichier", || done(&rt) == 1)?;
        self.call("SetPosition", "Reprise", "500")?;
        until(3.0, "repérage après la fin", || pos(&rt) == 500)?;
        sleep(Duration::from_millis(200));
        if rt.is_playing() {
            return Err("repérer après la fin ne doit pas relancer".into());
        }
        Ok(())
    }

    fn looping(&self, path: &str) -> Result<(), String> {
        let kind = Kind::Video { path: path.into(), looped: true, mark_in: 0, mark_out: 0, rate: 1.0 };
        let (_, rt) = self.add("Boucle", kind)?;
        until(5.0, "durée connue", || rt.dur_ms.load(Ordering::Relaxed) > 0)?;
        self.call("Play", "Boucle", "")?;
        sleep(Duration::from_millis(2500));
        let dur = rt.dur_ms.load(Ordering::Relaxed);
        if !rt.is_playing() || done(&rt) != 0 || pos(&rt) > dur {
            return Err(format!("boucle : position {} sur {dur}", pos(&rt)));
        }
        Ok(())
    }

    fn list(&self, short: &str) -> Result<(), String> {
        let item = |path: &str| ListItem { path: path.into(), enabled: true };
        let items = vec![item(short), item("C:/introuvable/absent.mp4"), item(short)];
        let kind = Kind::List { items, auto_next: true, auto_first: false, looped: false, shuffle: false };
        let (_, rt) = self.add("Liste", kind)?;
        until(5.0, "première image de liste", || rt.seq() >= 1)?;
        self.call("Play", "Liste", "")?;
        until(8.0, "fin de liste malgré un fichier absent", || done(&rt) == 1)?;
        if rt.is_playing() {
            return Err("la liste doit s'arrêter à sa fin".into());
        }
        self.call("SelectIndex", "Liste", "3")?;
        until(3.0, "choix d'un élément après la fin", || rt.item.load(Ordering::Relaxed) == 2)
    }

    fn audio(&self, dir: &str) -> Result<(), String> {
        let wav = make(&["-f", "lavfi", "-i", "sine=frequency=440:duration=1"], &format!("{dir}/son.wav"))?;
        let (_, rt) = self.add("Son", Kind::AudioFile { path: wav, looped: false })?;
        sleep(Duration::from_millis(300));
        self.call("Play", "Son", "")?;
        until(2.0, "niveau du son", || rt.peak() > 0.0)?;
        until(4.0, "fin du son", || done(&rt) == 1)?;
        if rt.is_playing() {
            return Err("le son doit s'arrêter à sa fin".into());
        }
        Ok(())
    }

    fn countdown(&self) -> Result<(), String> {
        let kind = Kind::Title {
            text: String::new(),
            sub: String::new(),
            style: TitleStyle::Countdown,
            color: [255, 255, 255],
            accent: [0, 0, 0],
            ticker_speed: 4.0,
            countdown_ms: 800,
            font: String::new(),
            size: 0.0,
            presets: Vec::new(),
            data: None,
        };
        let (_, rt) = self.add("Décompte", kind)?;
        until(3.0, "compte à rebours affiché", || rt.seq() >= 1)?;
        if rt.is_playing() {
            return Err("un compte à rebours attend son départ".into());
        }
        self.call("StartCountdown", "Décompte", "")?;
        until(3.0, "fin du compte", || done(&rt) == 1)?;
        self.call("StartCountdown", "Décompte", "")?;
        until(3.0, "second compte", || done(&rt) == 2)
    }

    /// Two moving pages in one browser: both draw, and the one nobody sees
    /// only keeps its card fed.
    fn web_pages(&self) -> Result<(), String> {
        let page = "data:text/html,<body style='margin:0'><div id=a style='width:100vw;height:100vh'></div>\
            <script>let n=0;setInterval(()=>{a.style.background='rgb('+(n++%25200+40)+',40,40)'},33)</script>";
        let kind = || Kind::Browser { url: page.into(), alpha: false };
        let (_, seen) = self.add("Page vue", kind())?;
        let (_, hidden) = self.add("Page cachée", kind())?;
        self.call("CutDirect", "Page vue", "")?;
        until(20.0, "les deux pages", || seen.seq() >= 2 && hidden.seq() >= 2)?;
        let (a, b) = (seen.seq(), hidden.seq());
        sleep(Duration::from_secs(3));
        let (a, b) = (seen.seq() - a, hidden.seq() - b);
        if a < 15 {
            return Err(format!("page à l'antenne trop lente : {a} images en 3 s"));
        }
        if !(2..=10).contains(&b) {
            return Err(format!("page cachée : {b} images en 3 s, attendu environ 6"));
        }
        Ok(())
    }

    /// The bug that started this: off, then on, must be the same input.
    fn overlay(&self) -> Result<(), String> {
        let id = self.engine.state.lock().unwrap().project.inputs.first().map(|i| i.id).ok_or("vide")?;
        self.call("OverlayInput1In", "1", "")?;
        let on = || self.engine.state.lock().unwrap().project.overlays[0];
        until(2.0, "incrustation visible", || on() == Some(id))?;
        self.call("OverlayInput1Out", "", "")?;
        until(2.0, "incrustation retirée", || on().is_none())?;
        self.call("PreviewInput", "2", "")?;
        self.call("OverlayInput1Last", "", "")?;
        until(2.0, "incrustation rappelée", || on() == Some(id))
    }

    /// A presenter filmed on green, keyed into a set on air, then the camera
    /// sent to the close-up by name: the output glides there over many
    /// frames, and the green never shows.
    fn virtual_set(&self, dir: &str) -> Result<(), String> {
        let args = [
            "-f",
            "lavfi",
            "-i",
            "color=c=0x00ff00:size=640x360:rate=30:duration=2",
            "-vf",
            "drawbox=x=256:y=72:w=128:h=288:color=0xC81E1E:t=fill",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
        ];
        let path = make(&args, &format!("{dir}/fond-vert.mp4"))?;
        let kind = Kind::Video { path, looped: true, mark_in: 0, mark_out: 0, rate: 1.0 };
        let (person, _) = self.add("Présentateur", kind)?;
        if let Some(i) = self.engine.state.lock().unwrap().input_mut(person) {
            i.key.mode = KeyMode::Chroma;
        }
        let set = self.engine.new_id();
        let layers = vec![Layer::new(person, crate::sets::presenter_spot(0))];
        let input = Input::new(set, "Plateau", crate::sets::new_set(SetLook::Studio)).with_layers(layers);
        self.engine.add_input(input);
        self.call("CutDirect", "Plateau", "")?;
        let at = |f: &crate::frame::Frame, x: f32, y: f32| {
            let i = (((y * f.h as f32) as usize) * f.w + (x * f.w as f32) as usize) * 4;
            [f.data[i], f.data[i + 1], f.data[i + 2]]
        };
        let red = |p: [u8; 3]| p[0] > 150 && p[1] < 90 && p[2] < 90;
        let width = || {
            let f = self.engine.program.lock().unwrap().clone();
            (0..f.w).filter(|&x| red(at(&f, (x as f32 + 0.5) / f.w as f32, 0.5))).count()
        };
        // The overlay step before leaves a full-frame overlay on air.
        self.call("OverlayInputAllOff", "", "")?;
        until(5.0, "présentateur incrusté au direct", || (60..=90).contains(&width()))
            .map_err(|e| format!("{e}, largeur {}", width()))?;
        let f = self.engine.program.lock().unwrap().clone();
        for x in [0.3, 0.4, 0.6, 0.7] {
            let p = at(&f, x, 0.5);
            if p[1] > 200 && p[0] < 80 {
                return Err(format!("fond vert visible autour du présentateur : {p:?}"));
            }
        }
        let wide = width();
        self.call("VirtualSet", "Plateau", "Gros plan")?;
        let mut seen = vec![wide];
        until(4.0, "gros plan au direct", || {
            let w = width();
            if seen.last() != Some(&w) {
                seen.push(w);
            }
            w >= wide * 2
        })?;
        if seen.len() < 8 {
            return Err(format!("mouvement de caméra saccadé : {seen:?}"));
        }
        Ok(())
    }
}

/// A tick's number in its colour: six levels per channel survive encoding.
fn numbered(n: u32, w: usize, h: usize) -> Arc<Frame> {
    let level = |k: u32| (20 + 40 * (k % 6)) as u8;
    Arc::new(Frame::solid(w, h, [level(n), level(n / 6), level(n / 36)]))
}

fn number_of(f: &Frame) -> u32 {
    let at = (f.h / 2 * f.w + f.w / 2) * 4;
    let level = |v: u8| ((v as f32 - 20.0) / 40.0).round().clamp(0.0, 5.0) as u32;
    level(f.data[at]) + 6 * level(f.data[at + 1]) + 36 * level(f.data[at + 2])
}

/// Plays a marked moment to its end and returns the tick shown by each
/// picture, in order.
fn watch(player: &InputRt, r: &ReplayState, index: usize) -> Result<Vec<u32>, String> {
    let play = r.clip_of(index).ok_or("moment de ralenti absent")?;
    let (before, mut seq) = (done(player), player.seq());
    let mut seen = Vec::new();
    *player.replay.lock().unwrap() = Some(play);
    player.playing.store(true, Ordering::Relaxed);
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        let finished = done(player) != before;
        if player.seq() != seq {
            seq = player.seq();
            if let Some(f) = player.latest() {
                seen.push(number_of(&f));
            }
        }
        if finished {
            return Ok(seen);
        }
        if Instant::now() > end {
            return Err("le ralenti ne finit pas".into());
        }
        sleep(Duration::from_millis(3));
    }
}

/// Pictures lost while recording must not move a replay: a moment plays from
/// the picture on screen at its in mark to the one on screen at its out mark,
/// and a loss holds the picture before it for as long as the show did.
fn replay_marks(dir: &str) -> Result<(), String> {
    let (w, h, fps) = (320, 180, 30);
    let path = format!("{dir}/ralenti-pertes.ts");
    let target = Target::Growing(path.clone());
    let (label, codec, audio) = ("Ralenti".to_string(), "libx264".to_string(), AudioPick::MASTER);
    let encoder =
        Encoder::start(Spec { label, target, codec, w, h, fps, kbps: 4000, audio_kbps: 128, audio })?;
    let mut r = ReplayState::default();
    r.speed = 0.5;
    r.begin(vec![Camera { input: 0, name: "Pertes".into(), path, encoder }]);
    let player = crate::sources::start(&Input::new(1, "Ralenti", Kind::Replay), w, h, fps);
    let steps = (|| {
        let cam = &r.cams[0].encoder;
        let beat = Duration::from_secs_f64(1.0 / fps as f64);
        let silence = vec![0i16; 2 * 48_000 / fps as usize];
        // ffmpeg keeps nothing sent before it is ready.
        let ready = Instant::now() + Duration::from_secs(15);
        while cam.pictures().lock().unwrap().first().is_none() {
            if Instant::now() > ready {
                return Err("le ralenti n'enregistre rien".to_string());
            }
            cam.push_video(numbered(0, w, h), Instant::now());
            cam.push_audio(silence.clone());
            sleep(beat);
        }
        // Ticks 30 to 49 lose their pictures, tick 50 comes 0.3 s late and
        // repeats its picture for the ticks it missed, as the mixer does.
        let mut taken = vec![Instant::now(); 141];
        let mut next = Instant::now();
        for n in 1..=140u32 {
            next += if n == 50 { beat + Duration::from_millis(300) } else { beat };
            sleep(next.saturating_duration_since(Instant::now()));
            let tick = Instant::now();
            taken[n as usize] = tick;
            let copies = match n {
                30..50 => 0,
                50 => 10,
                _ => 1,
            };
            for _ in 0..copies {
                cam.push_video(numbered(n, w, h), tick);
            }
            cam.push_audio(silence.clone());
        }
        let between = |n: usize| taken[n] + (taken[n + 1] - taken[n]) / 2;
        r.mark(between(70), between(100));
        r.mark(between(20), between(60));
        until(5.0, "lecteur de ralenti prêt", || player.seq() >= 1)?;

        let quiet = watch(&player, &r, 0)?;
        if quiet.first() != Some(&70) || quiet.last() != Some(&100) {
            return Err(format!(
                "ralenti marqué de 70 à 100, montre {:?} à {:?}",
                quiet.first(),
                quiet.last()
            ));
        }
        let lossy = watch(&player, &r, 1)?;
        if lossy.first() != Some(&20) || lossy.last() != Some(&60) {
            return Err(format!(
                "ralenti marqué de 20 à 60, montre {:?} à {:?}",
                lossy.first(),
                lossy.last()
            ));
        }
        // 20 lost ticks and 0.3 s late, at half speed: about 58 pictures.
        let held = lossy.iter().filter(|n| **n == 29).count();
        if held < 30 {
            return Err(format!("les images perdues ne durent que {held} images au ralenti"));
        }
        Ok(())
    })();
    player.shutdown();
    for c in std::mem::take(&mut r.cams) {
        c.encoder.finish(Duration::from_secs(5));
    }
    steps
}

pub fn check(dir: &str) -> Result<String, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let long = clip(dir, "long", 4)?;
    let short = clip(dir, "court", 1)?;
    let mut project = Project::blank();
    project.out_w = 640;
    project.out_h = 360;
    let desk = Desk { engine: Engine::start(project) };
    let steps = (|| {
        desk.marks(&long)?;
        desk.resume(&long)?;
        desk.looping(&short)?;
        desk.list(&short)?;
        desk.audio(dir)?;
        desk.countdown()?;
        desk.overlay()?;
        desk.virtual_set(dir)?;
        desk.web_pages()?;
        replay_marks(dir)
    })();
    desk.engine.shutdown();
    steps.map(|()| {
        "marques, reprise, boucle, liste, son, compte à rebours, incrustation, plateau virtuel, pages web, ralenti malgré les pertes"
            .to_string()
    })
}
