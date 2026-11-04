//! Instant replay: each chosen input records to its own growing file while the
//! show runs; marked moments play back from those files at any speed, on one
//! camera or four at a time.

use crate::engine::Engine;
use crate::model::{AudioPick, Input, Kind};
use crate::output::{Encoder, Spec, Target};
use crate::sources::ReplayPlay;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

pub const MAX_CAMERAS: usize = 8;

pub struct Camera {
    pub input: u64,
    pub name: String,
    pub path: String,
    pub encoder: Encoder,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub struct Event {
    pub start_s: f64,
    pub end_s: f64,
    pub camera: usize,
    pub text: String,
    pub tags: Vec<String>,
    pub speed: f32,
    /// The recordings it was marked on: a later session records to new files,
    /// and this moment must keep pointing at the old ones.
    #[serde(default)]
    pub files: Vec<String>,
    /// The tags as typed, so a comma can be typed before the next tag.
    #[serde(skip)]
    pub tag_text: Option<String>,
}

#[derive(Default)]
pub struct ReplayState {
    pub cams: Vec<Camera>,
    pub started: Option<Instant>,
    pub events: Vec<Event>,
    pub mark_in: Option<f64>,
    pub selected: Option<usize>,
    pub speed: f32,
    pub quad: bool,
    pub input: Option<u64>,
    pub folder: Option<PathBuf>,
}

impl ReplayState {
    /// Marks are positions in the files, which begin once the encoder is up,
    /// a moment after the button was pressed.
    fn now_s(&self) -> f64 {
        let began = self.cams.first().and_then(|c| c.encoder.began());
        began.map_or(0.0, |t| t.elapsed().as_secs_f64())
    }

    pub fn recorded_s(&self) -> f64 {
        self.now_s()
    }

    fn mark(&mut self, start_s: f64, end_s: f64) {
        let files = self.cams.iter().map(|c| c.path.clone()).collect();
        let speed = self.speed;
        self.events.push(Event { start_s, end_s, speed, files, ..Event::default() });
        self.selected = Some(self.events.len() - 1);
    }
}

/// Text search over an event's note and tags.
pub fn matches(e: &Event, query: &str) -> bool {
    let q = query.to_lowercase();
    q.is_empty() || e.text.to_lowercase().contains(&q) || e.tags.iter().any(|t| t.to_lowercase().contains(&q))
}

fn finish_cameras(cams: Vec<Camera>) {
    for c in cams {
        thread::spawn(move || c.encoder.finish(Duration::from_secs(5)));
    }
}

impl Engine {
    /// Starts recording the chosen inputs, adding the Replay input if needed.
    pub fn replay_start(&self, ids: &[u64], folder: PathBuf) -> Result<usize, String> {
        if self.is_replay() {
            return Ok(0);
        }
        std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
        let (inputs, w, h, fps) = {
            let s = self.state.lock().unwrap();
            let p = &s.project;
            let picked: Vec<Input> =
                p.inputs.iter().filter(|i| ids.contains(&i.id)).take(MAX_CAMERAS).cloned().collect();
            (picked, p.out_w, p.out_h, p.fps)
        };
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S-%3f").to_string();
        let mut cams: Vec<Camera> = Vec::new();
        for (n, input) in inputs.into_iter().enumerate() {
            let path = folder.join(format!("ralenti-{stamp}-cam{}.ts", n + 1)).to_string_lossy().into_owned();
            let opened = Encoder::start(Spec {
                label: format!("Ralenti : {}", input.name),
                target: Target::Growing(path.clone()),
                codec: "libx264".into(),
                w,
                h,
                fps,
                kbps: 20_000,
                audio_kbps: 128,
                audio: AudioPick::MASTER,
            });
            let encoder = match opened {
                Ok(e) => e,
                Err(e) => {
                    finish_cameras(cams);
                    return Err(e);
                }
            };
            cams.push(Camera { input: input.id, name: input.name, path, encoder });
        }
        let count = cams.len();
        if count == 0 {
            return Err("Choisissez au moins une caméra".into());
        }
        let existing = {
            let s = self.state.lock().unwrap();
            s.project.inputs.iter().find(|i| matches!(i.kind, Kind::Replay)).map(|i| i.id)
        };
        let id = existing.unwrap_or_else(|| {
            let id = self.new_id();
            self.add_input(Input::new(id, "Ralenti", Kind::Replay));
            id
        });
        let mut r = self.replay.lock().unwrap();
        r.cams = cams;
        r.started = Some(Instant::now());
        r.input = Some(id);
        r.folder = Some(folder);
        // A mark left open belongs to the session before: its time means
        // nothing against this one's clock.
        r.mark_in = None;
        if r.speed <= 0.0 {
            r.speed = 0.5;
        }
        Ok(count)
    }

    pub fn replay_stop(&self) {
        let cams = {
            let mut r = self.replay.lock().unwrap();
            r.started = None;
            std::mem::take(&mut r.cams)
        };
        finish_cameras(cams);
    }

    pub fn replay_mark_in(&self) {
        let mut r = self.replay.lock().unwrap();
        if r.started.is_some() {
            r.mark_in = Some(r.now_s());
        }
    }

    pub fn replay_mark_out(&self) {
        let mut r = self.replay.lock().unwrap();
        if let Some(start) = r.mark_in.take() {
            let end = r.now_s();
            r.mark(start, end);
        }
    }

    /// Marks the last few seconds in one press.
    pub fn replay_mark_last(&self, seconds: f64) {
        let mut r = self.replay.lock().unwrap();
        if r.started.is_none() {
            return;
        }
        let end = r.now_s();
        r.mark((end - seconds).max(0.0), end);
    }

    /// Plays an event on the Replay input, on its camera or four at once.
    pub fn replay_play(&self, index: usize) -> Option<u64> {
        let (play, input) = {
            let r = self.replay.lock().unwrap();
            let e = r.events.get(index)?;
            let all: Vec<String> = if e.files.is_empty() {
                r.cams.iter().map(|c| c.path.clone()).collect()
            } else {
                e.files.clone()
            };
            let files: Vec<String> = if r.quad {
                all.iter().take(4).cloned().collect()
            } else {
                vec![all.get(e.camera).or(all.first())?.clone()]
            };
            let speed = if e.speed > 0.0 { e.speed } else { r.speed.max(0.05) };
            (ReplayPlay { files, start_s: e.start_s, end_s: e.end_s, speed }, r.input?)
        };
        let rt = self.rt(input)?;
        rt.playing.store(true, Ordering::Relaxed);
        *rt.replay.lock().unwrap() = Some(play);
        Some(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_found_by_note_or_tag() {
        let e = Event { text: "But".into(), tags: vec!["Équipe A".into()], ..Event::default() };
        assert!(matches(&e, "but"));
        assert!(matches(&e, "équipe"));
        assert!(!matches(&e, "corner"));
        assert!(matches(&e, ""));
    }
}
