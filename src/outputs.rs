//! Starting and stopping recordings, streams and the other outputs. Starting
//! something already running does nothing; a start that fails part way closes
//! what it had opened; a stop never makes the window wait.

use crate::engine::Engine;
use crate::model::*;
use crate::output::{Encoder, Spec, Target};
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

/// Finishes encoders side by side, so several cost one wait, not the sum.
fn finish_all(list: Vec<Encoder>, patience: Duration) -> Vec<thread::JoinHandle<bool>> {
    list.into_iter().map(|e| thread::spawn(move || e.finish(patience))).collect()
}

fn finish_later(list: Vec<Encoder>, patience: Duration) {
    drop(finish_all(list, patience));
}

/// Stamp to the millisecond: stopping and starting within a second must not
/// write over the file still being closed.
fn stamp() -> String {
    chrono::Local::now().format("livecraft-%Y%m%d-%H%M%S-%3f").to_string()
}

impl Engine {
    fn format(&self) -> (usize, usize, u32) {
        let s = self.state.lock().unwrap();
        (s.project.out_w, s.project.out_h, s.project.fps)
    }

    pub fn is_recording(&self) -> bool {
        !self.recorders.lock().unwrap().is_empty()
    }

    pub fn is_streaming(&self) -> bool {
        !self.streamers.lock().unwrap().is_empty()
    }

    pub fn is_multi(&self) -> bool {
        !self.multi.lock().unwrap().is_empty()
    }

    pub fn is_srt(&self) -> bool {
        !self.srt.lock().unwrap().is_empty()
    }

    pub fn is_replay(&self) -> bool {
        self.replay.lock().unwrap().started.is_some()
    }

    /// Starts every enabled recorder. Returns where the first one writes.
    pub fn start_recording(&self, fallback_dir: &str) -> Result<String, String> {
        if self.is_recording() {
            return Ok(String::new());
        }
        let (w, h, fps) = self.format();
        let cfgs = self.state.lock().unwrap().project.records.clone();
        let stamp = stamp();
        let mut started = Vec::new();
        let mut first = String::new();
        for (n, cfg) in cfgs.iter().enumerate().filter(|(_, c)| c.enabled) {
            let dir = if cfg.folder.is_empty() { fallback_dir } else { &cfg.folder };
            let path = std::path::Path::new(dir)
                .join(format!("{stamp}-{}.{}", n + 1, cfg.container))
                .to_string_lossy()
                .into_owned();
            let opened = std::fs::create_dir_all(dir).map_err(|e| e.to_string()).and_then(|()| {
                Encoder::start(Spec {
                    label: format!("Enregistrement {}", n + 1),
                    target: Target::File { path: path.clone(), split_minutes: cfg.split_minutes },
                    codec: cfg.codec.clone(),
                    w,
                    h,
                    fps,
                    kbps: cfg.video_kbps,
                    audio_kbps: cfg.audio_kbps,
                    audio: cfg.audio,
                })
            });
            match opened {
                Ok(e) => started.push(e),
                Err(e) => {
                    finish_later(started, Duration::from_secs(5));
                    return Err(e);
                }
            }
            if first.is_empty() {
                first = path;
            }
        }
        if started.is_empty() {
            return Err("Aucun enregistreur activé".into());
        }
        *self.recorders.lock().unwrap() = started;
        *self.record_started.lock().unwrap() = Some(Instant::now());
        Ok(first)
    }

    pub fn stop_recording(&self) {
        let list = std::mem::take(&mut *self.recorders.lock().unwrap());
        *self.record_started.lock().unwrap() = None;
        finish_later(list, Duration::from_secs(15));
    }

    /// Destinations sharing destination 1's quality go through one encoder.
    pub fn start_streaming(&self) -> Result<usize, String> {
        if self.is_streaming() {
            return Ok(0);
        }
        let (w, h, fps) = self.format();
        let cfgs = self.state.lock().unwrap().project.streams.clone();
        let address = |c: &StreamCfg| {
            let base = c.url.trim().trim_end_matches('/');
            let key = c.key.trim();
            if key.is_empty() { base.to_string() } else { format!("{base}/{key}") }
        };
        let ready: Vec<&StreamCfg> =
            cfgs.iter().filter(|c| c.enabled && c.url.trim().len() > "rtmp://".len()).collect();
        let Some(lead) = ready.first() else {
            return Err("Indiquez d'abord une adresse de diffusion dans les réglages".into());
        };
        let shared: Vec<String> =
            ready.iter().enumerate().filter(|(n, c)| *n == 0 || c.shared).map(|(_, c)| address(c)).collect();
        let count = ready.len();
        let spec = |label: String, urls: Vec<String>, c: &StreamCfg| Spec {
            label,
            target: Target::Rtmp(urls),
            codec: c.codec.clone(),
            w,
            h,
            fps,
            kbps: c.kbps,
            audio_kbps: c.audio_kbps,
            audio: AudioPick::MASTER,
        };
        let mut started =
            vec![Encoder::start(spec(format!("Diffusion ({} destination(s))", shared.len()), shared, lead))?];
        for (n, c) in ready.iter().enumerate().filter(|(n, c)| *n > 0 && !c.shared) {
            match Encoder::start(spec(format!("Diffusion {}", n + 1), vec![address(c)], c)) {
                Ok(e) => started.push(e),
                Err(e) => {
                    finish_later(started, Duration::from_secs(3));
                    return Err(e);
                }
            }
        }
        *self.streamers.lock().unwrap() = started;
        *self.stream_started.lock().unwrap() = Some(Instant::now());
        Ok(count)
    }

    pub fn stop_streaming(&self) {
        let list = std::mem::take(&mut *self.streamers.lock().unwrap());
        *self.stream_started.lock().unwrap() = None;
        finish_later(list, Duration::from_secs(5));
    }

    /// One file per live input, with the chosen sound. Returns how many.
    pub fn start_multi(&self, dir: &str, ids: &[u64], audio: AudioPick) -> Result<usize, String> {
        if self.is_multi() {
            return Ok(0);
        }
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let (w, h, fps) = self.format();
        let (inputs, cfg) = {
            let s = self.state.lock().unwrap();
            let p = &s.project;
            let picked: Vec<Input> = p.inputs.iter().filter(|i| ids.contains(&i.id)).cloned().collect();
            (picked, p.records[0].clone())
        };
        let stamp = stamp();
        let mut started: Vec<(u64, Encoder)> = Vec::new();
        for input in inputs {
            let safe: String =
                input.name.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
            let path = std::path::Path::new(dir)
                .join(format!("{stamp}-{}-{safe}.mp4", input.id))
                .to_string_lossy()
                .into_owned();
            let opened = Encoder::start(Spec {
                label: format!("Multi : {}", input.name),
                target: Target::File { path, split_minutes: 0 },
                codec: cfg.codec.clone(),
                w,
                h,
                fps,
                kbps: cfg.video_kbps.min(20000),
                audio_kbps: 160,
                audio,
            });
            match opened {
                Ok(e) => started.push((input.id, e)),
                Err(e) => {
                    finish_later(started.into_iter().map(|(_, e)| e).collect(), Duration::from_secs(5));
                    return Err(e);
                }
            }
        }
        let n = started.len();
        *self.multi.lock().unwrap() = started;
        Ok(n)
    }

    pub fn stop_multi(&self) {
        let list = std::mem::take(&mut *self.multi.lock().unwrap());
        finish_later(list.into_iter().map(|(_, e)| e).collect(), Duration::from_secs(15));
    }

    pub fn start_srt(&self) -> Result<usize, String> {
        if self.is_srt() {
            return Ok(0);
        }
        let (w, h, fps) = self.format();
        let cfgs = self.state.lock().unwrap().project.srt.clone();
        let mut started: Vec<(Route, Encoder)> = Vec::new();
        for (n, c) in cfgs.iter().enumerate().filter(|(_, c)| c.enabled) {
            let opened = Encoder::start(Spec {
                label: format!("SRT {}", n + 1),
                target: Target::Srt(c.url.clone()),
                codec: "libx264".into(),
                w,
                h,
                fps,
                kbps: c.kbps,
                audio_kbps: 160,
                audio: c.audio,
            });
            match opened {
                Ok(e) => started.push((c.route, e)),
                Err(e) => {
                    finish_later(started.into_iter().map(|(_, e)| e).collect(), Duration::from_secs(3));
                    return Err(e);
                }
            }
        }
        let n = started.len();
        *self.srt.lock().unwrap() = started;
        Ok(n)
    }

    pub fn stop_srt(&self) {
        let list = std::mem::take(&mut *self.srt.lock().unwrap());
        finish_later(list.into_iter().map(|(_, e)| e).collect(), Duration::from_secs(3));
    }

    /// An input closing ends whatever was recording it on its own.
    pub(crate) fn stop_outputs_of(&self, id: u64) {
        let mut multi = self.multi.lock().unwrap();
        let (gone, kept): (Vec<_>, Vec<_>) =
            std::mem::take(&mut *multi).into_iter().partition(|(i, _)| *i == id);
        *multi = kept;
        drop(multi);
        finish_later(gone.into_iter().map(|(_, e)| e).collect(), Duration::from_secs(15));
        for (route, _) in self.srt.lock().unwrap().iter_mut() {
            if *route == Route::Input(id) {
                *route = Route::Output;
            }
        }
    }

    /// For closing the program: waits for files to be complete.
    pub fn shutdown(&self) {
        self.quit.store(true, Ordering::Relaxed);
        self.replay_stop();
        let mut list = std::mem::take(&mut *self.recorders.lock().unwrap());
        list.extend(std::mem::take(&mut *self.streamers.lock().unwrap()));
        list.extend(std::mem::take(&mut *self.multi.lock().unwrap()).into_iter().map(|(_, e)| e));
        list.extend(std::mem::take(&mut *self.srt.lock().unwrap()).into_iter().map(|(_, e)| e));
        for job in finish_all(list, Duration::from_secs(15)) {
            let _ = job.join();
        }
        for rt in self.rts.lock().unwrap().values() {
            rt.shutdown();
        }
    }

    pub(crate) fn check_outputs(&self) {
        let mut problem = None;
        for (slot, what) in [(&self.recorders, "Enregistrement"), (&self.streamers, "Diffusion")] {
            let mut list = slot.lock().unwrap();
            for enc in list.iter() {
                if let Some(warning) = enc.take_warning() {
                    problem = Some(format!("{what} : {warning}"));
                }
            }
            if let Some(i) = list.iter().position(|e| e.failure().is_some()) {
                let enc = list.remove(i);
                problem = Some(format!("{what} arrêté : {}", enc.failure().unwrap_or_default()));
                finish_later(vec![enc], Duration::from_secs(2));
            }
        }
        // A clock must not keep running once what it times has stopped.
        if !self.is_recording() {
            *self.record_started.lock().unwrap() = None;
        }
        if !self.is_streaming() {
            *self.stream_started.lock().unwrap() = None;
        }
        let mut multi = self.multi.lock().unwrap();
        if let Some(i) = multi.iter().position(|(_, e)| e.failure().is_some()) {
            let (_, enc) = multi.remove(i);
            problem = Some(format!("Multi-enregistrement : {}", enc.failure().unwrap_or_default()));
            finish_later(vec![enc], Duration::from_secs(2));
        }
        drop(multi);
        let mut srt = self.srt.lock().unwrap();
        if let Some(i) = srt.iter().position(|(_, e)| e.failure().is_some()) {
            let (_, enc) = srt.remove(i);
            problem = Some(format!("SRT arrêté : {}", enc.failure().unwrap_or_default()));
            finish_later(vec![enc], Duration::from_secs(2));
        }
        drop(srt);
        if problem.is_some() {
            *self.output_error.lock().unwrap() = problem;
        }
    }
}
