//! `--selftest <folder>`: drives the real engine without a window. Plays a
//! generated video, runs transitions, records, streams, replays and answers
//! remote control, then checks the files it made. Last, it sends its program
//! out over NDI and OMT and takes it back in, where those runtimes are installed.

use crate::api::ApiConfig;
use crate::engine::{Compositor, Engine};
use crate::functions::Args;
use crate::model::*;
use crate::netvideo::Net;
use crate::output::Padding;
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
    let out = Command::new(crate::ffmpeg::program("ffprobe"))
        .args(["-v", "error", "-show_entries", "stream=codec_type,channels:format=duration"])
        .args(["-of", "csv=p=0"])
        .arg(path)
        .output()
        .map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&out.stdout).replace(['\r', '\n'], " "))
}

/// Where the picture and the sound of a file end, from their packets. Both
/// start together, so a recording whose sound and picture drifted apart ends
/// one before the other.
fn ends(path: &str) -> Result<(f64, f64), String> {
    let out = Command::new(crate::ffmpeg::program("ffprobe"))
        .args(["-v", "error", "-show_entries", "packet=codec_type,pts_time,duration_time", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .map_err(|e| e.to_string())?;
    let (mut picture, mut sound) = (0f64, 0f64);
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut field = line.split(',');
        let kind = field.next();
        let mut number = || field.next().and_then(|v| v.parse::<f64>().ok());
        let Some(pts) = number() else { continue };
        let end = pts + number().unwrap_or(0.0);
        match kind {
            Some("video") => picture = picture.max(end),
            Some("audio") => sound = sound.max(end),
            _ => {}
        }
    }
    if picture == 0.0 || sound == 0.0 {
        return Err(format!("{path} : image ou son absent"));
    }
    Ok((picture, sound))
}

type Pad = std::sync::Arc<std::sync::Mutex<Option<Padding>>>;

/// The end padding of each recorder, to read once they are finished.
fn paddings(engine: &Engine) -> Vec<(String, Pad)> {
    engine.recorders.lock().unwrap().iter().map(|e| (e.label.clone(), e.padded.clone())).collect()
}

/// The longest end padding in seconds, and where each recording's picture
/// and sound ended before it. Picture and sound follow one clock, so they
/// end together and the files end together by construction: what had to be
/// padded shows whether they drifted apart on the way.
fn most_padded(pads: &[(String, Pad)], fps: u32) -> Result<(f64, String), String> {
    let mut most = 0f64;
    let mut each = Vec::new();
    for (label, pad) in pads {
        let p =
            (*pad.lock().unwrap()).ok_or_else(|| format!("{label} : fin d'enregistrement non égalisée"))?;
        most = most.max(p.pictures as f64 / fps.max(1) as f64).max(p.samples as f64 / 48_000.0);
        each.push(format!(
            "{label} : image jusqu'à {:.3} s, son jusqu'à {:.3} s, + {} image(s) et {:.0} ms de son",
            p.picture_end,
            p.sound_end,
            p.pictures,
            p.samples as f64 * 1000.0 / 48_000.0
        ));
    }
    Ok((most, each.join(" ; ")))
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

    // A web page draws through the windowless browser, every time it starts:
    // closing the last page ends the browser, so each start launches one. A
    // page shown only after being opened twice has failed too.
    let page = "data:text/html,<body style='background:rgb(200,40,40)'><h1>LiveCraft</h1></body>";
    for start in 1..=5 {
        let web = engine.new_id();
        engine.add_input(Input::new(web, "Web", Kind::Browser { url: page.into(), alpha: false }));
        let web_rt = engine.rt(web).ok_or("page absente")?;
        for _ in 0..150 {
            if web_rt.seq() > 0 {
                break;
            }
            if let Some(status) = web_rt.status() {
                return Err(format!("page web, départ {start} : {status}"));
            }
            sleep(Duration::from_millis(100));
        }
        let picture = web_rt
            .latest()
            .ok_or_else(|| format!("page web muette, départ {start} : {:?}", web_rt.status()))?;
        let middle = (picture.h / 2 * picture.w + picture.w / 2) * 4;
        if picture.data[middle] < 150 {
            return Err(format!("page web mal rendue : {:?}", &picture.data[middle..middle + 3]));
        }
        engine.close_input(web);
        // Long enough for the browser to be gone before the next start.
        sleep(Duration::from_millis(500));
    }
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
    let (dropped, repeated) = engine.recorders.lock().unwrap().iter().fold((0, 0), |(d, r), e| {
        (d + e.dropped.load(Ordering::Relaxed), r + e.repeated.load(Ordering::Relaxed))
    });
    let cut: u64 = engine.streamers.lock().unwrap().iter().map(|e| e.cut.load(Ordering::Relaxed)).sum();
    let fps = engine.stats.ticks_per_sec.load(Ordering::Relaxed);
    let project_fps = engine.state.lock().unwrap().project.fps;
    let frame_s = 1.0 / project_fps.max(1) as f64;
    let mix_us = engine.stats.mix_us.load(Ordering::Relaxed);
    // Says which side this run has checked: the card when there is one.
    let by = match engine.compositor.lock().unwrap().clone() {
        Compositor::Gpu(name) => format!("la carte graphique {name}"),
        Compositor::Cpu(why) => format!("le processeur ({why})"),
        Compositor::Starting => "personne encore".into(),
    };
    if let Some(e) = engine.output_error.lock().unwrap().clone() {
        return Err(e);
    }
    let pads = paddings(&engine);
    engine.shutdown();
    let (padded, endings) = most_padded(&pads, project_fps)?;
    if padded > frame_s + 1e-9 {
        return Err(format!("{:.0} ms ajoutées à la fin d'un enregistrement ({endings})", padded * 1000.0));
    }
    let _ = server.wait();
    sleep(Duration::from_millis(500));
    if peak <= 0.0 {
        return Err("aucun son de la vidéo".into());
    }
    let one = probe(&first)?;
    let second_path = first.replace("-1.mp4", "-2.mkv");
    let second = probe(&second_path)?;
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
    // Late ticks and full queues happen in a run this busy: whatever they
    // cost, sound and picture must still end together.
    let mut apart = Vec::new();
    for file in [&first, &second_path] {
        let (picture, sound) = ends(file)?;
        if (picture - sound).abs() > frame_s {
            return Err(format!("{file} : image jusqu'à {picture:.3} s, son jusqu'à {sound:.3} s"));
        }
        apart.push(format!("{:.0} ms", (sound - picture) * 1000.0));
    }
    Ok(format!(
        "enregistreurs {one}/ {second}| diffusion {streamed}(images coupées {cut}) | cadence {fps}/s, rendu {mix_us} µs \
         par {by}, images perdues {dropped}, répétées {repeated}, écart son-image {}, fin égalisée de {:.0} ms \
         ({endings}), crête son {peak:.2}| {call}",
        apart.join(" / "),
        padded * 1000.0
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A long recording at 1080p while a network source's sender runs 0.05 %
    /// fast, as a device clock can. Takes as long as it records:
    /// `cargo test --release long_recording -- --ignored --nocapture`, with
    /// `LIVECRAFT_MINUTES` for its length, ten by default, five at least: the
    /// last minute is judged after the input's speed has been learnt.
    #[test]
    #[ignore = "needs FFmpeg and takes minutes"]
    fn a_long_recording_keeps_sound_and_picture_together() {
        let minutes: u64 = std::env::var("LIVECRAFT_MINUTES").ok().and_then(|m| m.parse().ok()).unwrap_or(10);
        assert!(minutes >= 5, "LIVECRAFT_MINUTES must be 5 or more, not {minutes}");
        let dir = std::env::temp_dir().join(format!("livecraft-long-{}", std::process::id()));
        let dir = dir.to_string_lossy().into_owned();
        std::fs::create_dir_all(&dir).unwrap();
        let clip = format!("{dir}/clip.mp4");
        let made = ffmpeg()
            .args(["-y", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30:duration=20"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=20"])
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-c:a", "aac"])
            .arg(&clip)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(made.success());
        let port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        // 48 000 samples relabelled as 47 976 a second, then brought back to
        // 48 000: the sender delivers 24 samples a second too many.
        let mut sender = ffmpeg()
            .args(["-re", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30"])
            .args(["-re", "-f", "lavfi", "-i", "sine=frequency=880:sample_rate=48000"])
            .args(["-af", "asetrate=47976,aresample=48000"])
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency", "-c:a", "aac"])
            .args(["-f", "mpegts"])
            .arg(format!("udp://127.0.0.1:{port}?pkt_size=1316"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let mut project = Project::sample();
        (project.out_w, project.out_h) = (1920, 1080);
        project.records[0].folder = dir.clone();
        project.records[1].enabled = true;
        project.records[1].container = "mkv".into();
        project.records[1].folder = dir.clone();
        let engine = Engine::start(project);
        let id = engine.new_id();
        engine.add_input(Input::new(
            id,
            "Clip",
            Kind::Video { path: clip, looped: true, mark_in: 0, mark_out: 0, rate: 1.0 },
        ));
        let net = engine.new_id();
        let url = format!("udp://127.0.0.1:{port}");
        engine.add_input(Input::new(net, "Réseau", Kind::Network { url, buffer_ms: 0 }));
        engine.call("CutDirect", &Args::new().input("Clip")).unwrap();
        engine.call("Play", &Args::new().input("Clip")).unwrap();
        sleep(Duration::from_secs(3));

        let first = engine.start_recording(&dir).unwrap();
        let t0 = Instant::now();
        // The sender's sound comes in pieces of a third of a second, so what
        // waits peaks as each one lands. Sound piling up raises the least that
        // waits in every second, which is what Drift watches: that is checked,
        // and the peak is shown.
        let waiting = || engine.rt(net).and_then(|r| r.audio.as_ref().map(|a| a.queued())).unwrap_or(0);
        let last_minute = Duration::from_secs(minutes * 60 - 60);
        let (mut floor_late, mut peak_late) = (0, 0);
        while t0.elapsed() < Duration::from_secs(minutes * 60) {
            let second = Instant::now();
            let mut floor = usize::MAX;
            while second.elapsed() < Duration::from_secs(1) {
                let now = waiting();
                floor = floor.min(now);
                if t0.elapsed() > last_minute {
                    peak_late = peak_late.max(now);
                }
                sleep(Duration::from_millis(10));
            }
            if t0.elapsed() > last_minute {
                floor_late = floor_late.max(floor);
            }
        }
        let late = engine.stats.late_ticks.load(Ordering::Relaxed);
        let (sent, dropped, repeated) = engine.recorders.lock().unwrap().iter().fold((0, 0, 0), |t, e| {
            let get = |c: &std::sync::atomic::AtomicU64| c.load(Ordering::Relaxed);
            (t.0 + get(&e.sent), t.1 + get(&e.dropped), t.2 + get(&e.repeated))
        });
        let pads = paddings(&engine);
        engine.shutdown();
        let _ = sender.kill();
        let _ = sender.wait();
        let frame = 1.0 / 30.0;
        let (padded, endings) = most_padded(&pads, 30).unwrap();
        println!("ends evened by at most {:.1} ms: {endings}", padded * 1000.0);
        assert!(padded <= frame + 1e-9, "sound and picture drifted apart in the file");
        for file in [first.clone(), first.replace("-1.mp4", "-2.mkv")] {
            let (picture, sound) = ends(&file).unwrap();
            println!(
                "{file}: picture ends at {picture:.4} s, sound at {sound:.4} s, apart {:.1} ms",
                (sound - picture) * 1000.0
            );
            assert!((picture - sound).abs() <= frame, "{file}");
        }
        let ms = |frames: usize| frames as f64 * 1000.0 / 48_000.0;
        let (floor_ms, peak_ms) = (ms(floor_late), ms(peak_late));
        println!(
            "{minutes} min: {late} late ticks; pictures sent {sent}, dropped {dropped}, repeated {repeated}; \
             fast network sound waiting in the last minute: least each second at most {floor_ms:.0} ms, \
             peak {peak_ms:.0} ms"
        );
        assert!(floor_ms < 100.0, "the fast sender's sound piles up");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
