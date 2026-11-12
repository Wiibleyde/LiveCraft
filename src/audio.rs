//! The sound thread: treats each input, sums the Master and A to G buses,
//! feeds headphones and output devices, and hands sound to recordings.

use crate::dsp::Chain;
use crate::engine::Engine;
use crate::model::{AudioCfg, AudioPick, Kind};
use crate::sources::{AUDIO_RATE, Ring};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const FRAMES: usize = AUDIO_RATE / 50; // 20 ms per block
const BLOCK: usize = FRAMES * 2;

/// Name meaning the system's default output.
/// Saved in projects and compared at runtime: keep this exact value; show it with `device_label`.
pub const DEFAULT_DEVICE: &str = "Sortie par défaut";

/// What to show for an output device name: the default one in the interface language.
pub fn device_label(name: &str) -> &str {
    if name == DEFAULT_DEVICE { t!("audio.default_output") } else { name }
}

/// How long a device that failed is left alone before trying it again.
const RETRY: Duration = Duration::from_secs(2);

struct DeviceOut {
    ring: Arc<Ring>,
    /// Set when the device goes away, such as headphones being unplugged.
    broken: Arc<AtomicBool>,
    _stream: cpal::Stream,
}

struct Slot {
    out: Option<DeviceOut>,
    retry_at: Instant,
}

fn find_device(name: &str) -> Option<cpal::Device> {
    let host = cpal::default_host();
    let default = host.default_output_device();
    if name == DEFAULT_DEVICE || default.as_ref().and_then(device_name).as_deref() == Some(name) {
        return default;
    }
    host.output_devices().ok()?.find(|d| device_name(d).as_deref() == Some(name))
}

/// The same device can be asked for by name and as the default. Headphones
/// keep a device they share, and what is left is summed per device.
fn effective_routes(routes: &[(String, u8)], default_name: &str) -> Vec<(String, u8)> {
    let real = |d: &String| {
        if d == DEFAULT_DEVICE && !default_name.is_empty() { default_name.to_string() } else { d.clone() }
    };
    let list: Vec<(String, u8)> = routes.iter().map(|(d, what)| (real(d), *what)).collect();
    let phones = list.iter().find(|(_, what)| *what == 8).map(|(d, _)| d.clone());
    list.into_iter().filter(|(d, what)| *what == 8 || Some(d) != phones.as_ref()).collect()
}

fn device_name(d: &cpal::Device) -> Option<String> {
    d.description().ok().map(|x| x.name().to_string())
}

/// Plays a ring on one device. Sound keeps flowing to outputs without it.
fn open_device(name: &str) -> Option<DeviceOut> {
    let device = find_device(name)?;
    let config = device.default_output_config().ok()?;
    if config.sample_format() != cpal::SampleFormat::F32 {
        return None;
    }
    let config: cpal::StreamConfig = config.into();
    let channels = config.channels as usize;
    let step = AUDIO_RATE as f64 / config.sample_rate as f64;
    let ring = Arc::new(Ring::new(0.25));
    let feed = ring.clone();
    let mut phase = 0f64;
    let mut cur = [0f32; 2];
    let mut nxt = [0f32; 2];
    let broken = Arc::new(AtomicBool::new(false));
    let flag = broken.clone();
    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                for out in data.chunks_mut(channels) {
                    phase += step;
                    while phase >= 1.0 {
                        phase -= 1.0;
                        cur = nxt;
                        feed.pull(&mut nxt);
                    }
                    let t = phase as f32;
                    let l = cur[0] + (nxt[0] - cur[0]) * t;
                    let r = cur[1] + (nxt[1] - cur[1]) * t;
                    // Stereo goes to the front pair only: on a surround or
                    // multi-channel card the rest stays silent.
                    match out {
                        [mono] => *mono = (l + r) / 2.0,
                        [left, right, rest @ ..] => {
                            *left = l;
                            *right = r;
                            rest.fill(0.0);
                        }
                        [] => {}
                    }
                }
            },
            move |_| flag.store(true, Ordering::Relaxed),
            None,
        )
        .ok()?;
    stream.play().ok()?;
    Some(DeviceOut { ring, broken, _stream: stream })
}

struct Strip {
    id: u64,
    cfg: AudioCfg,
    audible: bool,
    solo: bool,
    always_on: bool,
}

struct Mix {
    strips: Vec<Strip>,
    master: f32,
    buses: Vec<(f32, bool)>,
    headphones: f32,
    /// Which device carries what: 0 Master, 1 to 7 buses, 8 headphones.
    routes: Vec<(String, u8)>,
    any_solo: bool,
}

impl Engine {
    fn read_mix(&self) -> Mix {
        let s = self.state.lock().unwrap();
        let p = &s.project;
        let air = s.on_air();
        let strips = p
            .inputs
            .iter()
            .filter(|i| i.kind.has_audio())
            .map(|i| Strip {
                id: i.id,
                cfg: i.audio.clone(),
                audible: !i.audio.muted && (!i.audio.follow || air.contains(&i.id)),
                solo: s.solo.contains(&i.id),
                always_on: is_always_on(&i.kind),
            })
            .collect();
        let fade = if p.ftb_audio { 1.0 - s.ftb_level } else { 1.0 };
        let master = if p.master_muted { 0.0 } else { p.master_level * p.master_level * fade };
        let buses = p
            .buses
            .iter()
            .map(|b| (if b.muted { 0.0 } else { b.level * b.level }, b.send_to_master))
            .collect();
        let mut routes = Vec::new();
        if !p.master_device.is_empty() {
            routes.push((p.master_device.clone(), 0));
        }
        for (n, b) in p.buses.iter().enumerate().filter(|(_, b)| !b.device.is_empty()) {
            routes.push((b.device.clone(), n as u8 + 1));
        }
        let phones = if p.headphones_device.is_empty() {
            DEFAULT_DEVICE.to_string()
        } else {
            p.headphones_device.clone()
        };
        routes.push((phones, 8));
        Mix {
            strips,
            master,
            buses,
            headphones: p.headphones_level * p.headphones_level,
            routes,
            any_solo: !s.solo.is_empty(),
        }
    }

    pub(crate) fn audio_loop(self: Arc<Self>) {
        // Asking the system about devices can be slow, so it happens beside
        // the sound, which must not miss a block.
        self.list_audio_devices();
        let default_name = Arc::new(Mutex::new(default_output_name()));
        let (engine, shared) = (self.clone(), default_name.clone());
        thread::spawn(move || {
            while !engine.quit.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_secs(3));
                engine.list_audio_devices();
                *shared.lock().unwrap() = default_output_name();
            }
        });
        let mut devices: HashMap<String, Slot> = HashMap::new();
        let mut chains: HashMap<u64, Chain> = HashMap::new();
        let mut gains: HashMap<u64, f32> = HashMap::new();
        let mut tmp = vec![0f32; BLOCK];
        let mut buses = vec![vec![0f32; BLOCK]; 8];
        let mut solo = vec![0f32; BLOCK];
        let mut next = Instant::now();
        while !self.quit.load(Ordering::Relaxed) {
            let mix = self.read_mix();
            buses.iter_mut().for_each(|b| b.fill(0.0));
            solo.fill(0.0);
            chains.retain(|id, _| mix.strips.iter().any(|s| s.id == *id));
            gains.retain(|id, _| mix.strips.iter().any(|s| s.id == *id));
            for strip in &mix.strips {
                let Some(rt) = self.rt(strip.id) else { continue };
                let Some(ring) = &rt.audio else { continue };
                if !rt.is_playing() && !strip.always_on {
                    rt.peak.store(0, Ordering::Relaxed);
                    continue;
                }
                ring.pull(&mut tmp);
                let chain = chains.entry(strip.id).or_default();
                chain.process(&mut tmp, &strip.cfg);
                let fader = strip.cfg.volume * strip.cfg.volume;
                let target = if strip.audible { fader } else { 0.0 };
                let from = *gains.get(&strip.id).unwrap_or(&target);
                gains.insert(strip.id, target);
                let step = (target - from) / FRAMES as f32;
                let (pan_l, pan_r) = balance(strip.cfg.balance);
                let mut peak = 0f32;
                for (n, pair) in tmp.as_chunks::<2>().0.iter().enumerate() {
                    let g = from + step * n as f32;
                    let (l, r) = (pair[0] * g * pan_l, pair[1] * g * pan_r);
                    // The meter reads the fader even when muted: seeing a live
                    // source is how one knows it is worth unmuting.
                    peak = peak.max(pair[0].abs() * fader * pan_l).max(pair[1].abs() * fader * pan_r);
                    for (b, bus) in buses.iter_mut().enumerate() {
                        if strip.cfg.buses & (1 << b) != 0 {
                            let send = strip.cfg.bus_levels[b];
                            bus[n * 2] += l * send;
                            bus[n * 2 + 1] += r * send;
                        }
                    }
                    if strip.solo {
                        // Pre-fader hears the source itself; otherwise what
                        // the fader leaves, audible even when muted.
                        let g = if strip.cfg.pfl { 1.0 } else { fader };
                        let phones = g * strip.cfg.headphones;
                        solo[n * 2] += pair[0] * phones;
                        solo[n * 2 + 1] += pair[1] * phones;
                    }
                }
                rt.peak.store(peak.to_bits(), Ordering::Relaxed);
            }
            // Buses first, because some of them feed Master.
            for b in 1..8 {
                let (level, to_master) = mix.buses[b - 1];
                let (head, tail) = buses.split_at_mut(b);
                tail[0].iter_mut().for_each(|v| *v = (*v * level).clamp(-1.0, 1.0));
                if to_master {
                    for (m, v) in head[0].iter_mut().zip(tail[0].iter()) {
                        *m += *v;
                    }
                }
                let peak = tail[0].iter().fold(0f32, |m, v| m.max(v.abs()));
                self.bus_peaks[b].store(peak.to_bits(), Ordering::Relaxed);
            }
            buses[0].iter_mut().for_each(|v| *v = (*v * mix.master).clamp(-1.0, 1.0));
            let peak = buses[0].iter().fold(0f32, |m, v| m.max(v.abs()));
            self.bus_peaks[0].store(peak.to_bits(), Ordering::Relaxed);

            let default = default_name.lock().unwrap().clone();
            play_routes(&mix, &buses, &solo, &mut devices, &default);
            self.feed_encoders(&buses);

            next += Duration::from_millis(20);
            let now = Instant::now();
            if next > now {
                thread::sleep(next - now);
            } else if now - next > Duration::from_millis(500) {
                next = now;
            }
            // Slightly late runs the next block at once: recordings need
            // every block to stay in step with the picture.
        }
    }

    fn list_audio_devices(&self) {
        let mut names = vec![DEFAULT_DEVICE.to_string()];
        if let Ok(list) = cpal::default_host().output_devices() {
            names.extend(list.filter_map(|d| device_name(&d)));
        }
        *self.audio_devices.lock().unwrap() = names;
    }

    fn feed_encoders(&self, buses: &[Vec<f32>]) {
        let pcm = |pick: AudioPick| interleave(buses, pick);
        for enc in self.recorders.lock().unwrap().iter() {
            enc.push_audio(pcm(enc.pick));
        }
        for enc in self.streamers.lock().unwrap().iter() {
            enc.push_audio(pcm(enc.pick));
        }
        for (_, enc) in self.multi.lock().unwrap().iter() {
            enc.push_audio(pcm(enc.pick));
        }
        for (_, enc) in self.srt.lock().unwrap().iter() {
            enc.push_audio(pcm(enc.pick));
        }
        for cam in self.replay.lock().unwrap().cams.iter() {
            cam.encoder.push_audio(pcm(cam.encoder.pick));
        }
    }
}

fn default_output_name() -> String {
    cpal::default_host().default_output_device().as_ref().and_then(device_name).unwrap_or_default()
}

/// One block per device per turn: two pushes would play at double speed.
fn play_routes(
    mix: &Mix,
    buses: &[Vec<f32>],
    solo: &[f32],
    devices: &mut HashMap<String, Slot>,
    default_name: &str,
) {
    let routes = effective_routes(&mix.routes, default_name);
    let wanted: HashSet<&String> = routes.iter().map(|(d, _)| d).collect();
    devices.retain(|name, _| wanted.contains(name));
    let mut blocks: HashMap<&String, Vec<f32>> = HashMap::new();
    for (name, what) in &routes {
        let block = blocks.entry(name).or_insert_with(|| vec![0.0; BLOCK]);
        if *what == 8 {
            let src = if mix.any_solo { solo } else { &buses[0][..] };
            block.iter_mut().zip(src).for_each(|(o, v)| *o += *v * mix.headphones);
        } else {
            block.iter_mut().zip(&buses[*what as usize]).for_each(|(o, v)| *o += *v);
        }
    }
    let now = Instant::now();
    for (name, mut block) in blocks {
        let slot = devices.entry(name.clone()).or_insert(Slot { out: None, retry_at: now });
        if slot.out.as_ref().is_some_and(|d| d.broken.load(Ordering::Relaxed)) {
            slot.out = None;
            slot.retry_at = now + RETRY;
        }
        if slot.out.is_none() && now >= slot.retry_at {
            slot.out = open_device(name);
            slot.retry_at = now + RETRY;
        }
        if let Some(dev) = &slot.out {
            block.iter_mut().for_each(|v| *v = v.clamp(-1.0, 1.0));
            dev.ring.push(&block);
        }
    }
}

/// Files and lists wait for Play, but a live device always runs.
fn is_always_on(kind: &Kind) -> bool {
    matches!(kind, Kind::Microphone { .. } | Kind::SystemAudio | Kind::Tone | Kind::Network { .. })
}

/// Balance from -1 (left) to 1 (right): the centre leaves both sides at full
/// level, and moving away only turns the other side down.
fn balance(b: f32) -> (f32, f32) {
    let b = b.clamp(-1.0, 1.0);
    ((1.0 - b).min(1.0), (1.0 + b).min(1.0))
}

/// Puts the picked buses side by side as a multi-track block of 16-bit sound.
pub fn interleave(buses: &[Vec<f32>], pick: AudioPick) -> Vec<i16> {
    let count = pick.count.max(1) as usize;
    let mut out = Vec::with_capacity(BLOCK * count);
    for n in 0..buses[0].len() / 2 {
        for b in &pick.buses[..count] {
            let bus = &buses[*b as usize];
            out.push((bus[n * 2] * 32767.0) as i16);
            out.push((bus[n * 2 + 1] * 32767.0) as i16);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_named_twice_counts_once() {
        let r = |d: &str, w| (d.to_string(), w);
        let routes = [r("Casque", 0), r("Enceintes", 1), r("Enceintes", 2), r(DEFAULT_DEVICE, 8)];
        let got = effective_routes(&routes, "Casque");
        assert_eq!(got, [r("Enceintes", 1), r("Enceintes", 2), r("Casque", 8)]);
        let got = effective_routes(&[r(DEFAULT_DEVICE, 8)], "");
        assert_eq!(got, [r(DEFAULT_DEVICE, 8)]);
    }

    #[test]
    fn balance_leaves_centre_alone() {
        assert_eq!(balance(0.0), (1.0, 1.0));
        assert_eq!(balance(1.0), (0.0, 1.0));
    }

    #[test]
    fn paired_buses_come_out_as_four_channels() {
        let mut buses = vec![vec![0f32; 4]; 8];
        buses[1] = vec![0.5; 4];
        buses[2] = vec![-0.5; 4];
        let pick = AudioPick::all().into_iter().find(|(n, _)| n == "AB").unwrap().1;
        let pcm = interleave(&buses, pick);
        assert_eq!(pcm.len(), 8);
        assert!(pcm[0] > 0 && pcm[2] < 0);
    }

    #[test]
    fn every_new_bus_pair_exists() {
        let names: Vec<String> = AudioPick::all().into_iter().map(|(n, _)| n).collect();
        for want in ["AB", "CD", "DE", "ABCD", "DEFG"] {
            assert!(names.iter().any(|n| n == want));
        }
    }
}
