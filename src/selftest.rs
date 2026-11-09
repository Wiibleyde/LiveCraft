//! `--selftest <folder>`: drives the real engine without a window. Plays a
//! generated video, runs transitions, records, streams, replays and answers
//! remote control, then checks the files it made.

use crate::api::ApiConfig;
use crate::engine::Engine;
use crate::functions::Args;
use crate::model::*;
use crate::sources::ffmpeg;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::thread::sleep;
use std::time::Duration;

pub fn run(dir: &str) -> i32 {
    match crate::playtest::check(dir).and_then(|media| Ok(format!("{media}\nOK: {}", check(dir)?))) {
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

fn http_get(path: &str) -> Result<String, String> {
    let mut s = TcpStream::connect(("127.0.0.1", 8088)).map_err(|e| e.to_string())?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:8088\r\n\r\n").map_err(|e| e.to_string())?;
    let mut text = String::new();
    s.read_to_string(&mut text).map_err(|e| format!("réponse HTTP {path} : {e}"))?;
    Ok(text)
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
    sleep(Duration::from_millis(800));
    let rt = engine.rt(id).ok_or("source absente")?;
    if let Some(e) = rt.status() {
        return Err(e);
    }
    if rt.seq() != 1 {
        return Err(format!("une vidéo en pause doit montrer une image, pas {}", rt.seq()));
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
    let call = crate::calltest::check(&engine)?;

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
         images perdues {dropped}, crête son {peak:.2}| {call}"
    ))
}
