//! Part of `--selftest`: a Video Delay of a real video, recorded side by side
//! with the video itself, the video's sound on bus A and the delay's on bus
//! B. A flash and a beep must come back on the delay as late as it was set,
//! within a frame, both through the graphics card and through the processor.
//! Then the video is closed: what the delay holds plays out, then it is black.

use crate::engine::{Compositor, Engine};
use crate::functions::Args;
use crate::model::*;
use crate::playtest::{make, until};
use crate::sources::{AUDIO_RATE, ffmpeg};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::sleep;
use std::time::{Duration, Instant};

const DELAY_MS: u32 = 500;
const FPS: u32 = 30;

/// Two seconds of black with a white flash and a beep, both at half a second.
fn flash_clip(dir: &str) -> Result<String, String> {
    let picture = "color=c=black:s=320x180:r=30:d=2,\
                   drawbox=x=0:y=0:w=iw:h=ih:color=white:t=fill:enable='between(t,0.49,0.59)'";
    let sound = "aevalsrc='if(between(t,0.5,0.6),0.5*sin(2*PI*1000*t),0)':s=48000:d=2";
    let args = ["-f", "lavfi", "-i", picture, "-f", "lavfi", "-i", sound, "-c:v", "libx264", "-preset"];
    let rest = ["ultrafast", "-pix_fmt", "yuv420p", "-g", "10", "-c:a", "pcm_s16le"];
    make(&[&args[..], &rest[..]].concat(), &format!("{dir}/retard-source.mkv"))
}

fn desk(dir: &str, clip: &str) -> Result<Project, String> {
    let mut p = Project::blank();
    (p.out_w, p.out_h, p.fps) = (640, 360, FPS);
    let video = Kind::Video { path: clip.into(), looped: true, mark_in: 0, mark_out: 0, rate: 1.0 };
    let mut source = Input::new(1, "Source", video);
    // Mirrored, so its finished picture is made by the compositor, and on
    // the card comes back from it, rather than being kept as decoded.
    source.mirror = true;
    source.audio.buses = 1 << 1;
    source.audio.follow = false;
    let mut delayed = Input::new(2, "Retard", Kind::VideoDelay { source: Some(1), ms: DELAY_MS });
    delayed.audio.buses = 1 << 2;
    delayed.audio.follow = false;
    let half = |x| Placement { x, y: 0.25, scale: 0.5 };
    let layers = vec![Layer::new(1, half(0.0)), Layer::new(2, half(0.5))];
    p.inputs = vec![source, delayed, Input::new(3, "Côte à côte", Kind::Scene).with_layers(layers)];
    p.program = Some(3);
    let r = &mut p.records[0];
    r.container = "mkv".into();
    r.folder = dir.into();
    r.audio = AudioPick::all().into_iter().find(|(n, _)| n == "AB").ok_or("pistes AB absentes")?.1;
    p.records[1].enabled = false;
    Ok(p)
}

/// Records the desk, then closes the video. Returns the file and what composed it.
fn record(engine: &Arc<Engine>, dir: &str, card: bool) -> Result<(String, String), String> {
    let source = engine.rt(1).ok_or("source absente")?;
    engine.call("Play", &Args::new().input("1"))?;
    until(10.0, "la vidéo source joue", || source.is_playing() && source.seq() > 5)?;
    let composer = || engine.compositor.lock().unwrap().clone();
    if card {
        until(10.0, "la carte graphique ouverte ou refusée", || composer() != Compositor::Starting)?;
    }
    // The delay fills, then two flashes are recorded.
    sleep(Duration::from_millis(DELAY_MS as u64 + 1000));
    let path = engine.start_recording(dir)?;
    sleep(Duration::from_millis(4500));
    engine.close_input(1);
    let delayed = engine.rt(2).ok_or("retard absent")?;
    let closed = Instant::now();
    until(3.0, "le retard noir une fois sa source fermée", || delayed.latest().is_none())?;
    if closed.elapsed() < Duration::from_millis(DELAY_MS as u64 - 100) {
        return Err("le retard a perdu ce qu'il gardait de sa source fermée".into());
    }
    if let Some(e) = engine.output_error.lock().unwrap().clone() {
        return Err(e);
    }
    let by = match composer() {
        Compositor::Gpu(name) => format!("la carte graphique {name}"),
        Compositor::Cpu(why) => format!("le processeur ({why})"),
        Compositor::Starting => "personne encore".into(),
    };
    Ok((path, by))
}

fn decode(path: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = ffmpeg()
        .args(["-v", "error", "-i", path])
        .args(args)
        .arg("pipe:1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if out.stdout.is_empty() {
        return Err(format!("{path} : rien à lire avec {args:?}"));
    }
    Ok(out.stdout)
}

/// Where a light comes on after picture `after`: a picture bright after a dark one.
fn first_light(levels: &[u8], after: usize) -> Option<usize> {
    (after + 1..levels.len()).find(|&n| levels[n] > 128 && levels[n - 1] <= 128)
}

/// Where a beep starts after sample `after`: loud after a tenth of a second
/// without anything loud.
fn first_beep(samples: &[i16], after: usize) -> Option<usize> {
    let quiet = AUDIO_RATE / 10;
    let mut last_loud: Option<usize> = None;
    for (n, v) in samples.iter().enumerate() {
        if v.unsigned_abs() > 8000 {
            if n > after && n >= quiet && last_loud.is_none_or(|l| n - l > quiet) {
                return Some(n);
            }
            last_loud = Some(n);
        }
    }
    None
}

/// How late the delay's half of the picture and its bus are, in seconds.
fn measure(path: &str) -> Result<(f64, f64), String> {
    // The middle band, as one pixel per half: the video, then its delay.
    let halves = "crop=iw:ih/2:0:ih/4,scale=2:1:flags=area,format=gray";
    let pixels =
        decode(path, &["-map", "0:v:0", "-vf", halves, "-fps_mode", "passthrough", "-f", "rawvideo"])?;
    let (left, right): (Vec<u8>, Vec<u8>) = pixels.as_chunks::<2>().0.iter().map(|p| (p[0], p[1])).unzip();
    let lit = first_light(&left, 0).ok_or("aucun flash de la vidéo dans l'enregistrement")?;
    let lit_late = first_light(&right, lit).ok_or("aucun flash du retard dans l'enregistrement")?;
    let track = |n: usize| -> Result<Vec<i16>, String> {
        let map = format!("0:a:{n}");
        let bytes = decode(path, &["-map", &map, "-ac", "1", "-ar", "48000", "-f", "s16le"])?;
        Ok(bytes.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect())
    };
    let (a, b) = (track(0)?, track(1)?);
    let beep = first_beep(&a, 0).ok_or("aucun bip de la vidéo sur le bus A")?;
    let beep_late = first_beep(&b, beep).ok_or("aucun bip du retard sur le bus B")?;
    Ok(((lit_late - lit) as f64 / FPS as f64, (beep_late - beep) as f64 / AUDIO_RATE as f64))
}

fn once(dir: &str, clip: &str, card: bool) -> Result<String, String> {
    let engine = Engine::start(desk(dir, clip)?);
    engine.use_gpu.store(card, Ordering::Relaxed);
    let recorded = record(&engine, dir, card);
    engine.shutdown();
    let (path, by) = recorded?;
    let (picture, sound) = measure(&path)?;
    let (set, frame) = (DELAY_MS as f64 / 1000.0, 1.0 / FPS as f64);
    let ms = |s: f64| s * 1000.0;
    if (picture - set).abs() > frame + 1e-9 || (sound - set).abs() > frame + 1e-9 {
        return Err(format!(
            "retard de {DELAY_MS} ms par {by} : image en retard de {:.0} ms, son de {:.1} ms ({path})",
            ms(picture),
            ms(sound)
        ));
    }
    Ok(format!("image {:.0} ms, son {:.1} ms par {by}", ms(picture), ms(sound)))
}

pub(crate) fn check(dir: &str) -> Result<String, String> {
    let dir = format!("{dir}/retard");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let clip = flash_clip(&dir)?;
    let card = once(&dir, &clip, true)?;
    let processor = once(&dir, &clip, false)?;
    Ok(format!("retard vidéo de {DELAY_MS} ms : {card} | {processor}, noir une fois sa source fermée"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_light_and_a_beep_are_found_where_they_start() {
        let levels = [200, 10, 10, 220, 230, 12, 240];
        assert_eq!(first_light(&levels, 0), Some(3), "not lit at the start of the file");
        assert_eq!(first_light(&levels, 3), Some(6));
        let mut samples = vec![0i16; 48_000];
        samples[100] = 20_000;
        samples[10_000..10_050].fill(20_000);
        samples[10_100] = -20_000;
        samples[30_000] = 20_000;
        assert_eq!(first_beep(&samples, 0), Some(10_000), "a beep cut at the start is no start");
        assert_eq!(first_beep(&samples, 10_000), Some(30_000));
    }

    /// The `--selftest` step alone: `cargo test delay_comes_back -- --ignored`.
    #[test]
    #[ignore = "needs FFmpeg and a quarter of a minute"]
    fn delay_comes_back_late_by_its_time() {
        let dir = std::env::temp_dir().join(format!("livecraft-retard-{}", std::process::id()));
        let result = check(&dir.to_string_lossy());
        let _ = std::fs::remove_dir_all(&dir);
        println!("{}", result.unwrap());
    }
}
