//! Second half of `--selftest`: everything with a timeline, on real files
//! decoded by real ffmpeg. Each step states what a person would expect.

use crate::engine::Engine;
use crate::functions::Args;
use crate::model::*;
use crate::sources::{InputRt, ffmpeg};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::sleep;
use std::time::{Duration, Instant};

fn make(args: &[&str], out: &str) -> Result<String, String> {
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

fn until(seconds: f32, what: &str, mut done: impl FnMut() -> bool) -> Result<(), String> {
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
        desk.web_pages()
    })();
    desk.engine.shutdown();
    steps.map(|()| {
        "marques, reprise, boucle, liste, son, compte à rebours, incrustation, pages web".to_string()
    })
}
