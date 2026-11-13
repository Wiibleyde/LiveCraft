//! Starting and stopping recordings, streams and the other outputs. Starting
//! something already running does nothing; a start that fails part way closes
//! what it had opened; a stop never makes the window wait.

use crate::engine::Engine;
use crate::model::*;
use crate::netvideo::{Net, NetOut};
use crate::output::{Encoder, Spec, Target};
use std::sync::Mutex;
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

/// How long a stopped recording waits for the picture under way and its
/// sound. Longer means a loop is stuck, and its end is padded.
const END_WAIT: Duration = Duration::from_secs(1);

/// A recording stopped, which the loops keep feeding until its picture and
/// its sound reach where it ends.
pub(crate) struct Ending {
    pub(crate) encoder: Encoder,
    /// The input a multi-recording records, `None` for the program.
    pub(crate) input: Option<u64>,
    patience: Duration,
    since: Instant,
}

/// Recordings being ended, and those being finished. Streams are not ended
/// this way: their last pictures do not matter, and a publisher kept a moment
/// longer would make a quick restart to the same key fail.
///
/// Locked before the lists recordings are taken from, so that the loops,
/// which visit it after them, find each recording in one or the other.
#[derive(Default)]
pub(crate) struct Endings {
    pub(crate) list: Vec<Ending>,
    /// Closing the program waits for these: killed with it, ffmpeg would
    /// leave a file without its end.
    finishing: Vec<thread::JoinHandle<bool>>,
    /// Closing has begun: it finishes the recordings left here itself.
    closing: bool,
}

impl Endings {
    fn end(&mut self, list: impl IntoIterator<Item = (Encoder, Option<u64>)>, patience: Duration) {
        let since = Instant::now();
        for (encoder, input) in list {
            encoder.stop();
            self.list.push(Ending { encoder, input, patience, since });
        }
    }

    fn finish(&mut self, list: Vec<Encoder>, patience: Duration) {
        self.finishing.retain(|job| !job.is_finished());
        self.finishing.extend(finish_all(list, patience));
    }
}

/// Stamp to the millisecond: stopping and starting within a second must not
/// write over the file still being closed.
fn stamp() -> String {
    chrono::Local::now().format("livecraft-%Y%m%d-%H%M%S-%3f").to_string()
}

/// Network senders being destroyed in the background. Starting new ones waits
/// for these, as a name stays taken until its sender is gone.
pub(crate) struct Teardowns {
    count: Mutex<usize>,
    idle: std::sync::Condvar,
}

impl Teardowns {
    fn begin(&self) {
        *self.count.lock().unwrap() += 1;
    }

    fn end(&self) {
        *self.count.lock().unwrap() -= 1;
        self.idle.notify_all();
    }

    /// Returns when none is left, or after `limit`.
    pub(crate) fn wait_idle(&self, limit: Duration) {
        let guard = self.count.lock().unwrap();
        drop(self.idle.wait_timeout_while(guard, limit, |n| *n > 0).unwrap());
    }
}

pub(crate) fn net_teardowns() -> &'static Teardowns {
    static T: Teardowns = Teardowns { count: Mutex::new(0), idle: std::sync::Condvar::new() };
    &T
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
        // Before anything starts, so that no recorder leaves a file behind for
        // another that cannot. Asked without waiting: not known yet is let
        // through, and ffmpeg then says what is wrong itself.
        for (n, cfg) in cfgs.iter().enumerate().filter(|(_, c)| c.enabled) {
            if let Some(p) = cfg.editing()
                && crate::ffmpeg::capabilities_if_known().is_some_and(|c| c.lacks_editing(p.encoder))
            {
                return Err(t!("outputs.profile_missing", n = n + 1, encoder = p.encoder));
            }
        }
        let mut started = Vec::new();
        let mut first = String::new();
        for (n, cfg) in cfgs.iter().enumerate().filter(|(_, c)| c.enabled) {
            let dir = if cfg.folder.is_empty() { fallback_dir } else { &cfg.folder };
            let path = std::path::Path::new(dir)
                .join(format!("{stamp}-{}.{}", n + 1, cfg.extension()))
                .to_string_lossy()
                .into_owned();
            let opened = std::fs::create_dir_all(dir).map_err(|e| e.to_string()).and_then(|()| {
                Encoder::start(Spec {
                    label: t!("outputs.recording_n", n = n + 1),
                    target: Target::File { path: path.clone(), split_minutes: cfg.split_minutes },
                    codec: cfg.encoder().to_string(),
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
            return Err(t!("outputs.no_recorder").into());
        }
        *self.recorders.lock().unwrap() = started;
        *self.record_started.lock().unwrap() = Some(Instant::now());
        Ok(first)
    }

    /// Each file ends where it was stopped, with the picture under way and
    /// the sound already made: the loops feed it until then, out of sight.
    pub fn stop_recording(&self) {
        let mut ending = self.ending.lock().unwrap();
        let list = std::mem::take(&mut *self.recorders.lock().unwrap());
        ending.end(list.into_iter().map(|e| (e, None)), Duration::from_secs(15));
        drop(ending);
        *self.record_started.lock().unwrap() = None;
    }

    /// Finishes the stopped recordings that reached their end, or waited too
    /// long for it. Both loops call it, so that one stuck cannot hold a file
    /// open.
    pub(crate) fn finish_ended(&self) {
        let mut ending = self.ending.lock().unwrap();
        if ending.closing || ending.list.is_empty() {
            return;
        }
        let done: Vec<Ending> =
            ending.list.extract_if(.., |e| e.encoder.ended() || e.since.elapsed() > END_WAIT).collect();
        for e in done {
            ending.finish(vec![e.encoder], e.patience);
        }
    }

    /// Every encoder that closing the program lets reach its end.
    fn each_ending_encoder(&self, mut visit: impl FnMut(&Encoder)) {
        self.recorders.lock().unwrap().iter().for_each(&mut visit);
        self.multi.lock().unwrap().iter().for_each(|(_, e)| visit(e));
        self.replay.lock().unwrap().cams.iter().for_each(|c| visit(&c.encoder));
        self.ending.lock().unwrap().list.iter().for_each(|e| visit(&e.encoder));
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
            return Err(t!("outputs.no_stream_address").into());
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
        let label = t!("outputs.streaming_shared", count = shared.len());
        let mut started = vec![Encoder::start(spec(label, shared, lead))?];
        for (n, c) in ready.iter().enumerate().filter(|(n, c)| *n > 0 && !c.shared) {
            match Encoder::start(spec(t!("outputs.streaming_n", n = n + 1), vec![address(c)], c)) {
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
        if let Some(p) = cfg.editing()
            && crate::ffmpeg::capabilities_if_known().is_some_and(|c| c.lacks_editing(p.encoder))
        {
            return Err(t!("outputs.profile_missing", n = 1, encoder = p.encoder));
        }
        let stamp = stamp();
        let mut started: Vec<(u64, Encoder)> = Vec::new();
        for input in inputs {
            let safe: String =
                input.name.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
            let ext = if cfg.editing().is_some() { cfg.extension() } else { "mp4" };
            let path = std::path::Path::new(dir)
                .join(format!("{stamp}-{}-{safe}.{ext}", input.id))
                .to_string_lossy()
                .into_owned();
            let opened = Encoder::start(Spec {
                label: t!("outputs.multi_label", name = input.name),
                target: Target::File { path, split_minutes: 0 },
                codec: cfg.encoder().to_string(),
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

    /// Each file ends where it was stopped, as a recording does.
    pub fn stop_multi(&self) {
        let mut ending = self.ending.lock().unwrap();
        let list = std::mem::take(&mut *self.multi.lock().unwrap());
        ending.end(list.into_iter().map(|(id, e)| (e, Some(id))), Duration::from_secs(15));
    }

    /// Ends at once what records the inputs `which` picks, those already
    /// ending included: such an input is closing, or about to be another one
    /// carrying the same number in a new project.
    pub(crate) fn stop_multi_at_once(&self, which: impl Fn(u64) -> bool) {
        let mut ending = self.ending.lock().unwrap();
        let mut multi = self.multi.lock().unwrap();
        let (gone, kept): (Vec<_>, Vec<_>) =
            std::mem::take(&mut *multi).into_iter().partition(|(i, _)| which(*i));
        *multi = kept;
        drop(multi);
        let mut list: Vec<Encoder> = gone.into_iter().map(|(_, e)| e).collect();
        list.extend(ending.list.extract_if(.., |e| e.input.is_some_and(&which)).map(|e| e.encoder));
        ending.finish(list, Duration::from_secs(15));
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

    pub fn is_net(&self) -> bool {
        !self.net.lock().unwrap().is_empty()
    }

    /// Offers the program on the network as each enabled NDI or OMT source.
    pub fn start_net(&self) -> Result<usize, String> {
        if self.is_net() {
            return Ok(0);
        }
        let (_, _, fps) = self.format();
        let wanted: Vec<(Net, NetOutCfg)> = {
            let p = &self.state.lock().unwrap().project;
            [(Net::Ndi, p.ndi_out.clone()), (Net::Omt, p.omt_out.clone())]
                .into_iter()
                .filter(|(_, c)| c.enabled)
                .collect()
        };
        if wanted.is_empty() {
            return Err(t!("outputs.net_none_ticked").into());
        }
        // A sender just stopped may still hold its name; wait for it to let go.
        net_teardowns().wait_idle(Duration::from_secs(5));
        let mut started = Vec::new();
        for (net, cfg) in wanted {
            let name = cfg.name.trim();
            let name = if name.is_empty() { "LiveCraft" } else { name };
            // On failure, those already started stop as `started` is dropped.
            started.push(NetOut::start(net, name, fps)?);
        }
        let n = started.len();
        *self.net.lock().unwrap() = started;
        Ok(n)
    }

    pub fn stop_net(&self) {
        let list = std::mem::take(&mut *self.net.lock().unwrap());
        // Each waits for its last picture to leave; the window does not.
        if list.is_empty() {
            return;
        }
        let pending = net_teardowns();
        pending.begin();
        thread::spawn(move || {
            drop(list);
            pending.end();
        });
    }

    /// An input closing ends whatever was recording it on its own.
    pub(crate) fn stop_outputs_of(&self, id: u64) {
        self.stop_multi_at_once(|i| i == id);
        for (route, _) in self.srt.lock().unwrap().iter_mut() {
            if *route == Route::Input(id) {
                *route = Route::Output;
            }
        }
    }

    /// For closing the program: waits for files to be complete.
    pub fn shutdown(&self) {
        // Streams and network senders stop at once, as when stopped by hand.
        let mut streams = std::mem::take(&mut *self.streamers.lock().unwrap());
        streams.extend(std::mem::take(&mut *self.srt.lock().unwrap()).into_iter().map(|(_, e)| e));
        let mut jobs = finish_all(streams, Duration::from_secs(15));
        drop(std::mem::take(&mut *self.net.lock().unwrap()));
        // Every file ends where it was stopped, while the loops still run to
        // bring it the picture under way. Those already ending stay to be fed
        // too, but the loops no longer hand them on: they are finished here.
        self.ending.lock().unwrap().closing = true;
        self.each_ending_encoder(Encoder::stop);
        let asked = Instant::now();
        while asked.elapsed() < END_WAIT {
            let mut all = true;
            self.each_ending_encoder(|e| all &= e.ended());
            if all {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        self.quit.store(true, Ordering::Relaxed);
        self.replay_stop();
        let ended = std::mem::take(&mut *self.ending.lock().unwrap());
        let mut list = std::mem::take(&mut *self.recorders.lock().unwrap());
        list.extend(ended.list.into_iter().map(|e| e.encoder));
        list.extend(std::mem::take(&mut *self.multi.lock().unwrap()).into_iter().map(|(_, e)| e));
        jobs.extend(finish_all(list, Duration::from_secs(15)));
        // Those stopped earlier and still being finished.
        jobs.extend(ended.finishing);
        for job in jobs {
            let _ = job.join();
        }
        for rt in self.rts.lock().unwrap().values() {
            rt.shutdown();
        }
    }

    pub(crate) fn check_outputs(&self) {
        let mut problem = None;
        for (slot, recording) in [(&self.recorders, true), (&self.streamers, false)] {
            let mut list = slot.lock().unwrap();
            for enc in list.iter() {
                if let Some(warning) = enc.take_warning() {
                    problem = Some(if recording {
                        t!("outputs.recording_warning", warning = warning)
                    } else {
                        t!("outputs.streaming_warning", warning = warning)
                    });
                }
            }
            if let Some(i) = list.iter().position(|e| e.failure().is_some()) {
                let enc = list.remove(i);
                let reason = enc.failure().unwrap_or_default();
                problem = Some(if recording {
                    t!("outputs.recording_stopped", reason = reason)
                } else {
                    t!("outputs.streaming_stopped", reason = reason)
                });
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
            problem = Some(t!("outputs.multi_failed", reason = enc.failure().unwrap_or_default()));
            finish_later(vec![enc], Duration::from_secs(2));
        }
        drop(multi);
        let mut srt = self.srt.lock().unwrap();
        if let Some(i) = srt.iter().position(|(_, e)| e.failure().is_some()) {
            let (_, enc) = srt.remove(i);
            problem = Some(t!("outputs.srt_stopped", reason = enc.failure().unwrap_or_default()));
            finish_later(vec![enc], Duration::from_secs(2));
        }
        drop(srt);
        if problem.is_some() {
            *self.output_error.lock().unwrap() = problem;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stopped recording whose picture or sound never comes, as when a loop
    /// is stuck, is left to reach its end for a second, then finished padded
    /// rather than left open; closing the program waits for its file.
    #[test]
    fn a_recording_that_never_reaches_its_end_is_finished_after_a_second() {
        let engine = Engine::start(Project::blank());
        // Its process never ends by itself: finishing it waits this long.
        let patience = Duration::from_secs(2);
        let stopped = Instant::now();
        engine.ending.lock().unwrap().end([(crate::output::never_ending(), None)], patience);
        engine.finish_ended();
        assert_eq!(engine.ending.lock().unwrap().list.len(), 1, "finished before its end");
        // Both loops hand it on once the second is up.
        while !engine.ending.lock().unwrap().list.is_empty() {
            assert!(stopped.elapsed() < END_WAIT * 2, "still open after {:?}", stopped.elapsed());
            thread::sleep(Duration::from_millis(10));
        }
        assert!(stopped.elapsed() >= END_WAIT);
        assert_eq!(engine.ending.lock().unwrap().finishing.len(), 1);
        let handed = Instant::now();
        engine.shutdown();
        let waited = handed.elapsed();
        assert!(waited >= patience - Duration::from_millis(300), "closing waited only {waited:?} for it");
    }
}
