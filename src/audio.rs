//! The sound thread: treats each input, sums the Master and A to G buses,
//! feeds headphones and output devices, and hands sound to recordings.

use crate::clap;
use crate::clock::{self, Drift, Pace, Take};
use crate::dsp::Chain;
use crate::engine::Engine;
use crate::model::{AudioCfg, AudioPick, Kind, ReturnSound};
use crate::plugins::{Owner, Rack};
use crate::sources::{AUDIO_RATE, Ring};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) const FRAMES: usize = AUDIO_RATE / 50; // 20 ms per block
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

/// Up to this far behind, blocks run back to back so that none is missing;
/// further, the time is given up as silence in recordings.
const CATCH_UP: u64 = clock::NS / 2;

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
    /// Its sound comes at a device's or a sender's pace, not ours.
    live: bool,
    /// A stream fetched as fast as its sound is taken, which can run ahead
    /// and wait for room.
    may_wait: bool,
    /// What a video call guest hears back.
    call: Option<ReturnSound>,
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
                cfg: i.audio.without_plugins(),
                audible: !i.audio.muted && (!i.audio.follow || air.contains(&i.id)),
                solo: s.solo.contains(&i.id),
                always_on: is_always_on(&i.kind),
                live: is_live(&i.kind),
                may_wait: matches!(&i.kind, Kind::Network { url, .. } if may_run_ahead(url)),
                call: match i.kind {
                    Kind::VideoCall { return_sound, .. } => Some(return_sound),
                    _ => None,
                },
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
        clap::this_thread_is_audio();
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
        let mut drifts: HashMap<u64, Drift> = HashMap::new();
        let mut raw = vec![0f32; BLOCK + 2];
        let mut tmp = vec![0f32; BLOCK];
        let mut buses = vec![vec![0f32; BLOCK]; 8];
        let mut solo = vec![0f32; BLOCK];
        // What each video call guest adds to Master, to take back out of
        // what they hear. It is taken out of `dry`, Master as the inputs
        // send it, before the plug-ins of buses and Master: those change the
        // guest's voice in the mix, which would then no longer cancel.
        let mut own: HashMap<u64, Vec<f32>> = HashMap::new();
        let mut dry = vec![0f32; BLOCK];
        let mut back = vec![0f32; BLOCK];
        let mut rack = Rack::new(&self.plugins);
        let mut pace = Pace::new(clock::now(), (AUDIO_RATE / FRAMES) as u32);
        let mut skipped = 0;
        while !self.quit.load(Ordering::Relaxed) {
            let mix = self.read_mix();
            rack.refresh();
            buses.iter_mut().for_each(|b| b.fill(0.0));
            solo.fill(0.0);
            chains.retain(|id, _| mix.strips.iter().any(|s| s.id == *id));
            gains.retain(|id, _| mix.strips.iter().any(|s| s.id == *id));
            drifts.retain(|id, _| mix.strips.iter().any(|s| s.id == *id && s.live));
            own.retain(|id, _| mix.strips.iter().any(|s| s.id == *id));
            own.values_mut().for_each(|v| v.fill(0.0));
            dry.fill(0.0);
            let minus = mix.strips.iter().any(|s| s.call == Some(ReturnSound::MasterMinusGuest));
            for strip in &mix.strips {
                let Some(rt) = self.rt(strip.id) else { continue };
                let Some(ring) = &rt.audio else { continue };
                if !rt.is_playing() && !strip.always_on {
                    rt.peak.store(0, Ordering::Relaxed);
                    continue;
                }
                // Time the loop gave up went by for every input too: a file's
                // sound stays with its picture, a live input drops what aged.
                let take = match strip.live.then(|| drifts.entry(strip.id).or_default()) {
                    Some(drift) => {
                        if skipped > 0 {
                            drift.resync();
                        }
                        drift.take(ring.queued(), FRAMES, held(ring, strip.may_wait))
                    }
                    None => {
                        ring.discard(skipped as usize * FRAMES);
                        Take { discard: 0, frames: FRAMES, silence: 0 }
                    }
                };
                fill(ring, &take, &mut raw, &mut tmp);
                let chain = chains.entry(strip.id).or_default();
                chain.process(&mut tmp, &strip.cfg);
                rack.run(Owner::Input(strip.id), &mut tmp);
                let fader = strip.cfg.volume * strip.cfg.volume;
                let target = if strip.audible { fader } else { 0.0 };
                let from = *gains.get(&strip.id).unwrap_or(&target);
                gains.insert(strip.id, target);
                let step = (target - from) / FRAMES as f32;
                let (pan_l, pan_r) = balance(strip.cfg.balance);
                let mut own_block = (strip.call == Some(ReturnSound::MasterMinusGuest))
                    .then(|| own.entry(strip.id).or_insert_with(|| vec![0f32; BLOCK]));
                let to_master = if minus { reach_of_master(&strip.cfg, &mix.buses) } else { 0.0 };
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
                    if minus {
                        dry[n * 2] += l * to_master;
                        dry[n * 2 + 1] += r * to_master;
                    }
                    if let Some(block) = own_block.as_mut() {
                        block[n * 2] = l * to_master;
                        block[n * 2 + 1] = r * to_master;
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
                rack.run(Owner::Bus(b as u8), &mut tail[0]);
                tail[0].iter_mut().for_each(|v| *v = (*v * level).clamp(-1.0, 1.0));
                if to_master {
                    for (m, v) in head[0].iter_mut().zip(tail[0].iter()) {
                        *m += *v;
                    }
                }
                let peak = tail[0].iter().fold(0f32, |m, v| m.max(v.abs()));
                self.bus_peaks[b].store(peak.to_bits(), Ordering::Relaxed);
            }
            rack.run(Owner::Bus(0), &mut buses[0]);
            buses[0].iter_mut().for_each(|v| *v = (*v * mix.master).clamp(-1.0, 1.0));
            let peak = buses[0].iter().fold(0f32, |m, v| m.max(v.abs()));
            self.bus_peaks[0].store(peak.to_bits(), Ordering::Relaxed);

            let default = default_name.lock().unwrap().clone();
            play_routes(&mix, &buses, &solo, &mut devices, &default);
            self.feed_encoders(&buses, pace.stamp());
            self.feed_calls(&mix, &buses, (&dry, &own), &mut back);
            rack.idle();

            let step = pace.advance(clock::now(), CATCH_UP);
            skipped = step.skipped;
            thread::sleep(step.wait);
        }
        rack.stop();
    }

    fn list_audio_devices(&self) {
        let mut names = vec![DEFAULT_DEVICE.to_string()];
        if let Ok(list) = cpal::default_host().output_devices() {
            names.extend(list.filter_map(|d| device_name(&d)));
        }
        *self.audio_devices.lock().unwrap() = names;
    }

    /// `at` is the time on the shared clock of the block's first frame.
    fn feed_encoders(&self, buses: &[Vec<f32>], at: u64) {
        let pcm = |pick: AudioPick| interleave(buses, pick);
        for enc in self.recorders.lock().unwrap().iter() {
            enc.push_audio(pcm(enc.pick), at);
        }
        for enc in self.streamers.lock().unwrap().iter() {
            enc.push_audio(pcm(enc.pick), at);
        }
        for e in self.ending.lock().unwrap().iter() {
            e.encoder.push_audio(pcm(e.encoder.pick), at);
        }
        for (_, enc) in self.multi.lock().unwrap().iter() {
            enc.push_audio(pcm(enc.pick), at);
        }
        for (_, enc) in self.srt.lock().unwrap().iter() {
            enc.push_audio(pcm(enc.pick), at);
        }
        for out in self.net.lock().unwrap().iter() {
            out.push_audio(&buses[0]);
        }
        for cam in self.replay.lock().unwrap().cams.iter() {
            cam.encoder.push_audio(pcm(cam.encoder.pick), at);
        }
    }

    /// Sends each connected guest what they asked to hear.
    /// `dry` is Master as the inputs send it, `own` what each guest sends it.
    fn feed_calls(
        &self,
        mix: &Mix,
        buses: &[Vec<f32>],
        (dry, own): (&[f32], &HashMap<u64, Vec<f32>>),
        back: &mut [f32],
    ) {
        for strip in &mix.strips {
            let Some(sound) = strip.call else { continue };
            let Some(rt) = self.rt(strip.id) else { continue };
            let Some(link) = rt.call.as_ref().filter(|l| l.is_connected()) else { continue };
            match sound {
                ReturnSound::MasterMinusGuest => {
                    let mine = own.get(&strip.id);
                    for (n, v) in back.iter_mut().enumerate() {
                        let guest = mine.map_or(0.0, |m| m[n]);
                        *v = ((dry[n] - guest) * mix.master).clamp(-1.0, 1.0);
                    }
                    link.sound.push(back);
                }
                ReturnSound::Bus(b) => {
                    if let Some(bus) = buses.get(b as usize) {
                        link.sound.push(bus);
                    }
                }
                ReturnSound::Off => {}
            }
        }
    }
}

/// How much of an input reaches Master: straight, and through the buses
/// that feed it.
fn reach_of_master(cfg: &AudioCfg, buses: &[(f32, bool)]) -> f32 {
    let mut total = if cfg.buses & 1 != 0 { cfg.bus_levels[0] } else { 0.0 };
    for (b, (level, to_master)) in buses.iter().enumerate() {
        if *to_master && cfg.buses & (1 << (b + 1)) != 0 {
            total += cfg.bus_levels[b + 1] * level;
        }
    }
    total
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
    matches!(
        kind,
        Kind::Microphone { .. }
            | Kind::SystemAudio
            | Kind::Tone
            | Kind::Network { .. }
            | Kind::Ndi { .. }
            | Kind::Omt { .. }
            | Kind::VideoCall { .. }
    )
}

/// Sound timed by a device or a sender rather than by our clock, which a
/// file or the test tone is: it can run fast or slow and must be kept in step.
fn is_live(kind: &Kind) -> bool {
    matches!(kind, Kind::Microphone { .. } | Kind::SystemAudio | Kind::Network { .. })
}

/// Whether a stream is fetched as fast as its sound is taken, as a playlist
/// (HLS, DASH) or a media file is: it can run ahead and wait for room in its
/// ring, and what waits there is still to come. Anything else delivers at its
/// sender's pace, so sound waiting in its ring is late, however full the ring
/// is: SRT, UDP, RTMP, RTSP, and over HTTP an MPEG-TS feed, a radio or a
/// camera. Watching the ring cannot tell them apart: a live stream backed up
/// behind a full ring waits for room just as a playlist does.
fn may_run_ahead(url: &str) -> bool {
    let (scheme, rest) = url.split_once("://").unwrap_or(("file", url));
    let name = rest.split(['?', '#']).next().unwrap_or("").rsplit('/').next().unwrap_or("");
    let kind = name.rsplit_once('.').map_or("", |(_, e)| e).to_ascii_lowercase();
    let fetched = ["m3u8", "mpd", "mp4", "m4v", "m4a", "mov", "mkv", "webm", "avi", "wmv"];
    match scheme.to_ascii_lowercase().as_str() {
        "http" | "https" => fetched.contains(&kind.as_str()),
        "file" => true,
        _ => false,
    }
}

/// The source is waiting for room in its ring, with sound still to come. Only
/// a fetched stream can be: a live one found waiting was held up, as when the
/// sound loop or the receiving ffmpeg stalls for a second on a busy machine.
/// Taken as held, its ring would stay full from then on, the sender putting
/// back only what each block takes, and its sound a second behind its picture
/// for the rest of the show.
fn held(ring: &Ring, may_wait: bool) -> bool {
    may_wait && ring.room() < 2 * BLOCK
}

/// Balance from -1 (left) to 1 (right): the centre leaves both sides at full
/// level, and moving away only turns the other side down.
fn balance(b: f32) -> (f32, f32) {
    let b = b.clamp(-1.0, 1.0);
    ((1.0 - b).min(1.0), (1.0 + b).min(1.0))
}

/// Makes one block of an input's sound in `out` as `take` says: old sound
/// thrown away, silence first if any, then the frames taken spread over the rest.
fn fill(ring: &Ring, take: &Take, raw: &mut [f32], out: &mut [f32]) {
    ring.discard(take.discard);
    let (quiet, rest) = out.split_at_mut(take.silence * 2);
    quiet.fill(0.0);
    if take.frames * 2 == rest.len() {
        ring.pull(rest);
    } else {
        let raw = &mut raw[..take.frames * 2];
        ring.pull(raw);
        fit(raw, rest);
    }
}

/// Spreads stereo `src` over `out` by linear interpolation. Taking one frame
/// more or less than a block then plays a thousandth faster or slower for
/// 20 ms instead of jumping, and the block's first and last frames stay put
/// so blocks still join without a step.
fn fit(src: &[f32], out: &mut [f32]) {
    let (n, m) = (src.len() / 2, out.len() / 2);
    if n == 0 {
        out.fill(0.0);
        return;
    }
    let scale = (n - 1) as f32 / (m - 1).max(1) as f32;
    for (j, pair) in out.as_chunks_mut::<2>().0.iter_mut().enumerate() {
        let x = j as f32 * scale;
        let i = (x as usize).min(n - 1);
        let k = (i + 1).min(n - 1);
        let t = x - i as f32;
        for c in 0..2 {
            pair[c] = src[i * 2 + c] + (src[k * 2 + c] - src[i * 2 + c]) * t;
        }
    }
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

    /// Keeping a live input in step must not click: a block made from one
    /// frame more or one less is as smooth as the wave, into the next block too.
    #[test]
    fn a_block_one_frame_long_or_short_stays_smooth() {
        let w = 1000.0 * std::f32::consts::TAU / AUDIO_RATE as f32;
        let wave: Vec<f32> = (0..FRAMES + 2).flat_map(|n| [(n as f32 * w).sin(); 2]).collect();
        for frames in [FRAMES - 1, FRAMES + 1] {
            let mut out = vec![0f32; BLOCK];
            fit(&wave[..frames * 2], &mut out);
            assert_eq!((out[0], out[BLOCK - 1]), (wave[0], wave[frames * 2 - 1]));
            out.extend_from_slice(&wave[frames * 2..frames * 2 + 2]);
            let left: Vec<f32> = out.iter().step_by(2).copied().collect();
            let steepest = left.windows(2).map(|p| (p[1] - p[0]).abs()).fold(0f32, f32::max);
            assert!(steepest <= w * 1.01, "{frames} frames: a step of {steepest}");
        }
    }

    #[test]
    fn only_fetched_streams_can_run_ahead() {
        for live in [
            "srt://h:9000?mode=caller",
            "udp://127.0.0.1:5000",
            "RTMP://h/live/k",
            "rtsp://cam/1",
            "rtp://h:5004",
            "http://h:8080/live.ts",
            "http://radio.example:8000/stream.mp3",
            "http://192.168.1.20/video.mjpg",
            "https://h/feed",
        ] {
            assert!(!may_run_ahead(live), "{live}");
        }
        for fetched in [
            "https://h/live/index.m3u8?token=a.b",
            "https://h/manifest.mpd",
            "http://h.example/vod/show.MP4",
            "C:/clips/show.ts",
        ] {
            assert!(may_run_ahead(fetched), "{fetched}");
        }
    }

    /// Sound left waiting in a ring a minute after the sound loop stalled for
    /// 1.5 s, from a sender running 0.05 % fast that waits for room as the
    /// pump does, through the loop's own `held` and `Drift`.
    fn waiting_after_a_stall(url: &str) -> usize {
        let ring = Ring::new(1.0);
        let mut drift = Drift::default();
        let chunk = [0f32; 960];
        let (mut raw, mut out) = ([0f32; BLOCK + 2], [0f32; BLOCK]);
        let (mut sent, mut pushed) = (0f64, 0usize);
        for n in 0..3500 {
            sent += FRAMES as f64 * 1.0005;
            while sent as usize >= pushed + chunk.len() / 2 && ring.room() >= chunk.len() {
                ring.push(&chunk);
                pushed += chunk.len() / 2;
            }
            if (500..575).contains(&n) {
                continue;
            }
            if n == 575 {
                drift.resync();
            }
            let take = drift.take(ring.queued(), FRAMES, held(&ring, may_run_ahead(url)));
            fill(&ring, &take, &mut raw, &mut out);
        }
        ring.queued()
    }

    /// What a fetched stream holds is still to come and stays; a live
    /// sender's is late, and must come back to the margin and stay there.
    #[test]
    fn a_live_sender_whose_ring_filled_comes_back_in_step() {
        let ms = |frames: usize| frames * 1000 / AUDIO_RATE;
        assert!(ms(waiting_after_a_stall("udp://127.0.0.1:5000")) < 30);
        assert!(ms(waiting_after_a_stall("https://h/show.m3u8")) > 900);
    }

    /// A 1 kHz tone from a device 100 ppm fast that delivers 40 ms at a time,
    /// up to 10 ms late and now and then 45 ms late, as a virtual cable on a
    /// busy machine does, through the loop's own path. Read back as the live
    /// check reads its streams, it must still be one steady tone: correcting
    /// a frame per block for a whole second would leave it a third of the sound.
    #[test]
    fn a_tone_from_an_uneven_device_keeps_its_pitch() {
        let ring = Ring::new(1.0);
        let mut drift = Drift::default();
        let (mut raw, mut out) = ([0f32; BLOCK + 2], [0f32; BLOCK]);
        let mut dice = 7u64;
        let mut roll = |n: u64| {
            dice = dice.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (dice >> 33) % n
        };
        let w = 1000.0 * std::f64::consts::TAU / AUDIO_RATE as f64;
        let device_rate = AUDIO_RATE as f64 * 1.0001;
        let (mut chunks, mut due) = (0u64, 0f64);
        let mut heard = Vec::new();
        for n in 0..50 * 10 {
            let now = n as f64 * 0.02;
            while due <= now {
                let first = chunks * 1920;
                let chunk: Vec<f32> =
                    (first..first + 1920).flat_map(|i| [((i as f64 * w).sin() * 0.25) as f32; 2]).collect();
                ring.push(&chunk);
                chunks += 1;
                let late = if chunks % 37 == 0 { 0.045 } else { roll(10_000) as f64 / 1e6 };
                due = due.max((chunks + 1) as f64 * 1920.0 / device_rate + late);
            }
            let take = drift.take(ring.queued(), FRAMES, held(&ring, false));
            fill(&ring, &take, &mut raw, &mut out);
            heard.extend(out.iter().step_by(2));
        }
        let read = &heard[5 * AUDIO_RATE..9 * AUDIO_RATE];
        if let Err(e) = crate::clock::judge_tone(read, AUDIO_RATE) {
            panic!("{e}");
        }
    }

    /// The trap of any return feed: the guest hears their own voice come
    /// back a moment late. What they hear is Master without them.
    #[test]
    fn a_guest_hears_the_show_but_not_themselves() {
        use crate::model::{Input, Project};
        let mut p = Project::blank();
        p.inputs.push(Input::new(1, "Son", Kind::Tone));
        let mut guest = Input::new(2, "Invité", crate::call::new_call());
        guest.audio.follow = false;
        p.inputs.push(guest);
        let engine = Engine::start(p);
        let rt = engine.rt(2).unwrap();
        let link = rt.call.clone().unwrap();
        // The guest's voice: a steady level, easy to find in what comes back,
        // kept a little ahead so a late wake-up does not make a gap in it.
        // Connected again each time, as the page opening meanwhile resets it.
        let voice = rt.audio.clone().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (talking, connection) = (stop.clone(), link.clone());
        let talker = thread::spawn(move || {
            while !talking.load(Ordering::Relaxed) {
                connection.set_connected(true);
                while voice.len() < 9600 {
                    voice.push(&[0.3f32; 960]);
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
        // The sound thread first lists the devices, and on a busy machine a
        // source can start late: wait until the show's tone and the guest
        // are both on their meters, then take a fresh stretch.
        let tone = engine.rt(1).unwrap();
        let (mut tone_seen, mut guest_seen) = (false, false);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(tone_seen && guest_seen) && Instant::now() < deadline {
            tone_seen |= tone.peak() > 0.05;
            guest_seen |= rt.peak() > 0.2;
            thread::sleep(Duration::from_millis(5));
        }
        // Master is read during that stretch, while the guest talks: read
        // after they stop, it only holds the rest of the show.
        let until = |enough: usize| {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut master = 0f32;
            loop {
                master = master.max(f32::from_bits(engine.bus_peaks[0].load(Ordering::Relaxed)));
                if link.sound.len() >= enough || Instant::now() > deadline {
                    return master;
                }
                thread::sleep(Duration::from_millis(5));
            }
        };
        until(1);
        link.sound.clear();
        let master_peak = until(9600);
        let mut back = vec![0f32; link.sound.len()];
        link.sound.pull(&mut back);
        stop.store(true, Ordering::Relaxed);
        let _ = talker.join();
        engine.shutdown();
        assert!(back.len() > 4800, "sound goes back: {}", back.len());
        let mean = back.iter().sum::<f32>() / back.len() as f32;
        let loudest = back.iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!(master_peak > 0.3, "the guest is on Master: {master_peak}");
        assert!(mean.abs() < 0.02, "the guest's own voice came back: {mean}");
        assert!(loudest > 0.05, "the rest of the show is heard: {loudest}");
    }
}
