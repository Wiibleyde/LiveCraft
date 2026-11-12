//! `--selftest <folder>`: drives the real engine without a window. Plays a
//! generated video, runs transitions, records, streams, replays and answers
//! remote control, then checks the files it made. Last, it sends its program
//! out over NDI and OMT and takes it back in, where those runtimes are installed.

use crate::api::ApiConfig;
use crate::engine::Engine;
use crate::functions::Args;
use crate::model::*;
use crate::netvideo::Net;
use crate::sources::ffmpeg;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::thread::sleep;
use std::time::{Duration, Instant};

pub fn run(dir: &str) -> i32 {
    let all = crate::playtest::check(dir)
        .and_then(|media| Ok(format!("{media}\nOK: {}", check(dir)?)))
        .and_then(|done| Ok(format!("{done}\nOK: {}", net_loopback()?)));
    match all {
        Ok(msg) => {
            println!("OK: {msg}");
            0
        }
        Err(e) => {
            println!("ECHEC: {e}");
            1
        }
    }
}

fn probe(path: &str) -> Result<String, String> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_type,channels:format=duration"])
        .args(["-of", "csv=p=0"])
        .arg(path)
        .output()
        .map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&out.stdout).replace(['\r', '\n'], " "))
}

pub(crate) fn http_get(path: &str) -> Result<String, String> {
    let mut s = TcpStream::connect(("127.0.0.1", 8088)).map_err(|e| e.to_string())?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:8088\r\n\r\n").map_err(|e| e.to_string())?;
    let mut text = String::new();
    s.read_to_string(&mut text).map_err(|e| format!("réponse HTTP {path} : {e}"))?;
    Ok(text)
}

/// NDI and OMT out of LiveCraft and back into it: a red program with a tone
/// must come back red and audible. A runtime that is not installed is
/// reported, not failed, as LiveCraft never ships one.
fn net_loopback() -> Result<String, String> {
    let mut notes = Vec::new();
    for net in Net::ALL {
        match net.available() {
            Ok(()) => notes.push(loop_back(net)?),
            Err(e) => notes.push(format!("{} NON TESTÉ : {e}", net.label())),
        }
    }
    Ok(notes.join(" | "))
}

fn loop_back(net: Net) -> Result<String, String> {
    let label = net.label();
    let mut project = Project::blank();
    project.out_w = 640;
    project.out_h = 360;
    project.inputs =
        vec![Input::new(1, "Rouge", Kind::Solid { color: [220, 30, 30] }), Input::new(2, "Son", Kind::Tone)];
    project.program = Some(1);
    let name = format!("LiveCraft essai {}", std::process::id());
    let cfg = NetOutCfg { enabled: true, name: name.clone() };
    match net {
        Net::Ndi => project.ndi_out = cfg,
        Net::Omt => project.omt_out = cfg,
    }
    let engine = Engine::start(project);
    let result = (|| {
        engine.start_net()?;
        let suffix = format!("({name})");
        let deadline = Instant::now() + Duration::from_secs(15);
        let source = loop {
            if let Some(s) = net.sources()?.into_iter().find(|s| s.ends_with(&suffix)) {
                break s;
            }
            if Instant::now() > deadline {
                return Err(format!("{label} : la sortie « {name} » n'apparaît pas sur le réseau"));
            }
            sleep(Duration::from_millis(200));
        };
        let id = engine.new_id();
        let mut input = Input::new(id, "Retour", net.input(source.clone()));
        // Heard on the meter only: sent back out, it would loop forever.
        input.audio.muted = true;
        let asked = Instant::now();
        engine.add_input(input);
        let rt = engine.rt(id).ok_or("entrée absente")?;
        let colour = |f: &crate::frame::Frame, x: usize, y: usize| {
            let at = (y * f.w + x) * 4;
            [f.data[at], f.data[at + 1], f.data[at + 2], f.data[at + 3]]
        };
        let is_red = |p: [u8; 4]| p[0] > 180 && p[1] < 80 && p[2] < 80 && p[3] == 255;
        let mut seen_at = None;
        loop {
            let picture = rt.latest();
            let red = picture.as_ref().is_some_and(|f| {
                (f.w, f.h) == (640, 360) && is_red(colour(f, 320, 180)) && is_red(colour(f, 2, 2))
            });
            if red && seen_at.is_none() {
                seen_at = Some(asked.elapsed());
            }
            if red && rt.peak() > 0.03 {
                break;
            }
            if asked.elapsed() > Duration::from_secs(15) {
                let middle = picture.as_ref().map(|f| colour(f, f.w / 2, f.h / 2));
                return Err(format!(
                    "{label} : retour de « {source} » incorrect, image {middle:?}, crête son {:.3}, état {:?}",
                    rt.peak(),
                    rt.status()
                ));
            }
            sleep(Duration::from_millis(50));
        }
        sleep(Duration::from_millis(700));
        let connections = engine.net.lock().unwrap().first().map_or(0, |o| o.connections());
        if connections == 0 {
            return Err(format!("{label} : la sortie ne compte aucune connexion"));
        }
        Ok(format!(
            "{label} : « {source} » revient rouge en {} ms, crête son {:.2}, {connections} connexion(s)",
            seen_at.unwrap_or_default().as_millis(),
            rt.peak()
        ))
    })();
    engine.shutdown();
    result
}

fn check(dir: &str) -> Result<String, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let clip = format!("{dir}/source.mp4");
    let made = ffmpeg()
        .args(["-y", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30:duration=4"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=4"])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-c:a", "aac"])
        .arg(&clip)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| e.to_string())?;
    if !made.success() {
        return Err("impossible de créer la vidéo de test".into());
    }

    let mut project = Project::sample();
    project.out_w = 1280;
    project.out_h = 720;
    project.records[1].enabled = true;
    project.records[1].container = "mkv".into();
    project.records[0].folder = dir.to_string();
    project.records[1].folder = dir.to_string();
    project.records[1].audio = AudioPick::all().into_iter().find(|(n, _)| n == "AB").unwrap().1;
    let engine = Engine::start(project);
    if let Some(e) = engine.start_api(ApiConfig::default()) {
        return Err(e);
    }
    let id = engine.new_id();
    let video = Kind::Video { path: clip.clone(), looped: true, mark_in: 0, mark_out: 0, rate: 1.0 };
    engine.add_input(Input::new(id, "Test", video));
    let rt = engine.rt(id).ok_or("source absente")?;
    // ffmpeg takes longer to start on a busy machine; once the first picture
    // is up, a paused video must not show a second one.
    let asked = Instant::now();
    while rt.seq() == 0 && rt.status().is_none() && asked.elapsed() < Duration::from_secs(10) {
        sleep(Duration::from_millis(20));
    }
    let first_ms = asked.elapsed().as_millis();
    sleep(Duration::from_millis(500));
    if let Some(e) = rt.status() {
        return Err(e);
    }
    if rt.seq() != 1 {
        return Err(format!(
            "une vidéo en pause doit montrer une image, pas {} (après {first_ms} ms)",
            rt.seq()
        ));
    }

    // A web page draws through the windowless browser.
    let web = engine.new_id();
    let page = "data:text/html,<body style='background:rgb(200,40,40)'><h1>LiveCraft</h1></body>";
    engine.add_input(Input::new(web, "Web", Kind::Browser { url: page.into(), alpha: false }));
    let web_rt = engine.rt(web).ok_or("page absente")?;
    for _ in 0..150 {
        if web_rt.seq() > 0 {
            break;
        }
        sleep(Duration::from_millis(100));
    }
    let picture = web_rt.latest().ok_or_else(|| format!("page web muette : {:?}", web_rt.status()))?;
    let middle = (picture.h / 2 * picture.w + picture.w / 2) * 4;
    if picture.data[middle] < 150 {
        return Err(format!("page web mal rendue : {:?}", &picture.data[middle..middle + 3]));
    }
    engine.close_input(web);

    // Remote control drives the desk and auto-play starts the video.
    let inputs = engine.state.lock().unwrap().project.inputs.len();
    let reply = http_get(&format!("/api/?Function=PreviewInput&Input={inputs}"))?;
    if !reply.contains("200 OK") {
        return Err(format!("API : {reply}"));
    }
    http_get("/api/?Function=Cube&Duration=300")?;
    sleep(Duration::from_millis(500));
    if engine.state.lock().unwrap().project.program != Some(id) {
        return Err("la transition n'a pas mis la vidéo au direct".into());
    }
    if !rt.is_playing() {
        return Err("la vidéo ne démarre pas en passant au direct".into());
    }
    let xml = http_get("/api/")?;
    if !xml.contains(&format!("<active>{inputs}</active>")) {
        return Err("état XML incorrect".into());
    }
    let mut tcp = TcpStream::connect(("127.0.0.1", 8099)).map_err(|e| e.to_string())?;
    tcp.write_all(b"TALLY\r\n").map_err(|e| e.to_string())?;
    let mut answer = [0u8; 64];
    let n = tcp.read(&mut answer).map_err(|e| format!("réponse TCP : {e}"))?;
    if !String::from_utf8_lossy(&answer[..n]).starts_with("TALLY OK") {
        return Err("port TCP muet".into());
    }

    // Two recorders, a stream, separate recordings and replay, all at once.
    let first = engine.start_recording(dir)?;
    let received = format!("{dir}/diffusion.flv");
    let mut server = ffmpeg()
        .args(["-y", "-listen", "1", "-timeout", "10", "-f", "flv", "-i"])
        .arg("rtmp://127.0.0.1:19350/live/test")
        .args(["-c", "copy"])
        .arg(&received)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    sleep(Duration::from_millis(700));
    {
        let mut s = engine.state.lock().unwrap();
        s.project.streams[0].enabled = true;
        s.project.streams[0].url = "rtmp://127.0.0.1:19350/live/test".into();
    }
    engine.start_streaming()?;
    let separate = engine.start_multi(dir, &[id], AudioPick::MASTER)?;
    if separate != 1 {
        return Err(format!("multi-enregistrement : {separate} au lieu de 1"));
    }
    engine.replay_start(&[id, 2], std::path::PathBuf::from(dir))?;

    // Overlay 8 and a countdown trigger through the same functions.
    engine.call("OverlayInput8In", &Args::new().input("11"))?;
    sleep(Duration::from_secs(4));
    engine.replay_mark_last(2.0);
    let replay_input = engine.replay_play(0).ok_or("ralenti impossible")?;
    sleep(Duration::from_millis(1500));
    let replay_rt = engine.rt(replay_input).ok_or("entrée ralenti absente")?;
    if replay_rt.seq() < 3 {
        return Err("le ralenti ne montre aucune image".into());
    }

    let peak = rt.peak();
    let dropped: u64 =
        engine.recorders.lock().unwrap().iter().map(|r| r.dropped.load(Ordering::Relaxed)).sum();
    let fps = engine.stats.ticks_per_sec.load(Ordering::Relaxed);
    let mix_us = engine.stats.mix_us.load(Ordering::Relaxed);
    if let Some(e) = engine.output_error.lock().unwrap().clone() {
        return Err(e);
    }
    engine.shutdown();
    let _ = server.wait();
    sleep(Duration::from_millis(500));
    if peak <= 0.0 {
        return Err("aucun son de la vidéo".into());
    }
    let one = probe(&first)?;
    let second = probe(&first.replace("-1.mp4", "-2.mkv"))?;
    let streamed = probe(&received)?;
    if !one.contains("video") || !one.contains("audio") {
        return Err(format!("enregistrement incomplet : {one}"));
    }
    if second.matches("audio").count() < 2 {
        return Err(format!("second enregistreur sans ses deux pistes : {second}"));
    }
    if !streamed.contains("video") || !streamed.contains("audio") {
        return Err(format!("diffusion incomplète : {streamed}"));
    }
    let multi = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .any(|e| e.file_name().to_string_lossy().ends_with("-Test.mp4"));
    if !multi {
        return Err("multi-enregistrement sans fichier".into());
    }
    Ok(format!(
        "enregistreurs {one}/ {second}| diffusion {streamed}| cadence {fps}/s, rendu {mix_us} µs, \
         images perdues {dropped}, crête son {peak:.2}"
    ))
}
