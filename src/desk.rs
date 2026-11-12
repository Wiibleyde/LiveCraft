//! The desk's rules: what preview, program, the T-bar and the overlay channels
//! do in response to each action, including actions that land mid-transition.
//! Plain state with no threads, so every rule is testable on its own.

use crate::model::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const OVERLAYS: usize = 8;
const FTB_SECS: f32 = 1.0;

#[derive(Clone, Copy)]
pub struct Trans {
    pub from: Option<u64>,
    pub to: Option<u64>,
    pub kind: TransKind,
    pub started: Instant,
    pub ms: u32,
    /// Copied at the start, so editing it cannot move a running cut point.
    pub stinger: Option<StingerCfg>,
}

/// Things the state decided that need the running inputs to carry out.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Note {
    Trigger(u64, TriggerEvent),
    Play(u64),
    Pause(u64),
    Restart(u64),
    /// Back to the first item of a list.
    FirstItem(u64),
}

#[derive(Clone, Copy)]
pub struct MixFade {
    pub from: Option<u64>,
    pub started: Instant,
    pub ms: u32,
}

/// A virtual set's camera on its way to its chosen preset.
#[derive(Clone, Copy)]
pub struct SetMove {
    pub from: SetView,
    pub started: Instant,
    pub ms: u32,
}

impl SetMove {
    fn progress(&self, now: Instant) -> f32 {
        (now.saturating_duration_since(self.started).as_secs_f32() * 1000.0 / self.ms.max(1) as f32).min(1.0)
    }
}

/// The parts of the project the mixer reads, rebuilt only on change.
pub struct Snapshot {
    pub version: u64,
    pub inputs: HashMap<u64, Input>,
    pub order: Vec<u64>,
    pub w: usize,
    pub h: usize,
    pub fps: u32,
}

/// What closing an input removed, so Undo can put it all back.
pub struct Closed {
    pub at: usize,
    pub input: Input,
    pub layers: Vec<(u64, usize, Layer)>,
    pub overlays: Vec<usize>,
    pub playlist: Vec<(usize, PlaylistItem)>,
    pub was_preview: bool,
    pub was_program: bool,
}

/// The show clock: time since the show started, which the operator can pause.
/// Plain data with the time passed in, so it can be tested without waiting.
/// It lives for the session only: a clock saved with a project would show a
/// stale time when the project is opened for the next show.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ShowClock {
    /// What had passed before the last start.
    before: Duration,
    /// When it last started, while it runs.
    since: Option<Instant>,
}

impl ShowClock {
    pub fn running(&self) -> bool {
        self.since.is_some()
    }

    pub fn elapsed(&self, now: Instant) -> Duration {
        self.before + self.since.map_or(Duration::ZERO, |t| now.saturating_duration_since(t))
    }

    /// Starts, or resumes after a pause. Does nothing when it already runs.
    pub fn start(&mut self, now: Instant) {
        self.since.get_or_insert(now);
    }

    pub fn pause(&mut self, now: Instant) {
        self.before = self.elapsed(now);
        self.since = None;
    }

    pub fn toggle(&mut self, now: Instant) {
        if self.running() {
            self.pause(now);
        } else {
            self.start(now);
        }
    }

    /// Back to 00:00:00 and stopped, ready for the next show.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

pub struct State {
    pub project: Project,
    pub clock: ShowClock,
    pub trans: Option<Trans>,
    /// Hand-held transition position, 0..1, while the T-bar is off its rest.
    pub tbar: f32,
    pub overlay_shown: [Option<u64>; OVERLAYS],
    pub overlay_t: [f32; OVERLAYS],
    pub overlay_since: [Option<Instant>; OVERLAYS],
    pub solo: HashSet<u64>,
    pub ftb: bool,
    pub ftb_level: f32,
    pub inputs_paused: bool,
    pub playlist_on: bool,
    pub playlist_index: usize,
    pub playlist_next: Option<Instant>,
    pub mix_fades: HashMap<u64, MixFade>,
    pub set_moves: HashMap<u64, SetMove>,
    pub undo: Option<Closed>,
    /// Inputs that Pause Inputs stopped, the ones to start again.
    pub paused_by_desk: Vec<u64>,
    pub padlock: bool,
    pub notes: Vec<Note>,
    /// Bumped by every change that can alter the picture.
    pub version: u64,
    snapshot: Option<Arc<Snapshot>>,
}

impl State {
    pub fn new(project: Project) -> Self {
        let shown = project.overlays;
        let t = shown.map(|o| if o.is_some() { 1.0 } else { 0.0 });
        Self {
            project,
            clock: ShowClock::default(),
            trans: None,
            tbar: 0.0,
            overlay_shown: shown,
            overlay_t: t,
            overlay_since: shown.map(|o| o.map(|_| Instant::now())),
            solo: HashSet::new(),
            ftb: false,
            ftb_level: 0.0,
            inputs_paused: false,
            playlist_on: false,
            playlist_index: 0,
            playlist_next: None,
            mix_fades: HashMap::new(),
            set_moves: HashMap::new(),
            undo: None,
            paused_by_desk: Vec::new(),
            padlock: false,
            notes: Vec::new(),
            version: 1,
            snapshot: None,
        }
    }

    pub fn input(&self, id: u64) -> Option<&Input> {
        self.project.inputs.iter().find(|i| i.id == id)
    }

    pub fn input_mut(&mut self, id: u64) -> Option<&mut Input> {
        self.version += 1;
        self.project.inputs.iter_mut().find(|i| i.id == id)
    }

    /// For sound-only changes: the picture has nothing to redo.
    pub fn audio_mut(&mut self, id: u64) -> Option<&mut AudioCfg> {
        self.project.inputs.iter_mut().find(|i| i.id == id).map(|i| &mut i.audio)
    }

    pub fn snapshot(&mut self) -> Arc<Snapshot> {
        if let Some(s) = &self.snapshot
            && s.version == self.version
        {
            return s.clone();
        }
        let p = &self.project;
        let snap = Arc::new(Snapshot {
            version: self.version,
            inputs: p.inputs.iter().map(|i| (i.id, i.clone())).collect(),
            order: p.inputs.iter().map(|i| i.id).collect(),
            w: p.out_w,
            h: p.out_h,
            fps: p.fps,
        });
        self.snapshot = Some(snap.clone());
        snap
    }

    /// What appearing on air asks of an input and everything shown inside it.
    /// Only files have a timeline to start; a countdown waits for its own
    /// Start. `before` is what was already on air, which must not jump back.
    fn note_arriving(&mut self, id: u64, before: &HashSet<u64>) {
        for member in self.with_layers(HashSet::from([id])) {
            let Some(i) = self.input(member).filter(|i| i.kind.has_timeline()) else { continue };
            let first = matches!(i.kind, Kind::List { auto_first: true, .. });
            let (restart, play) = (i.auto_restart, i.auto_play);
            if !before.contains(&member) {
                if first {
                    self.notes.push(Note::FirstItem(member));
                } else if restart {
                    self.notes.push(Note::Restart(member));
                }
            }
            if play {
                self.notes.push(Note::Play(member));
            }
        }
    }

    /// What leaving the air asks, unless it is still shown somewhere else.
    fn note_leaving(&mut self, id: u64) {
        let on_air = self.on_air();
        for member in self.with_layers(HashSet::from([id])) {
            if on_air.contains(&member) {
                continue;
            }
            let pause = self.input(member).is_some_and(|i| i.auto_pause && i.kind.has_timeline());
            if pause {
                self.notes.push(Note::Pause(member));
            }
        }
    }

    fn note_going_on(&mut self, id: Option<u64>, before: &HashSet<u64>) {
        let Some(id) = id else { return };
        self.notes.push(Note::Trigger(id, TriggerEvent::OnTransitionIn));
        self.note_arriving(id, before);
    }

    fn note_going_off(&mut self, id: Option<u64>) {
        let Some(id) = id else { return };
        self.notes.push(Note::Trigger(id, TriggerEvent::OnTransitionOut));
        self.note_leaving(id);
    }

    /// Everything on air after a project opens starts as it would by hand.
    pub fn note_project_on_air(&mut self) {
        let before = HashSet::new();
        let mut ids: Vec<u64> = self.project.overlays.iter().flatten().copied().collect();
        ids.extend(self.project.program);
        for id in ids {
            self.note_arriving(id, &before);
        }
    }

    /// Program takes `to`. Preview takes the old program, unless the operator
    /// has already chosen something else there meanwhile.
    fn land(&mut self, from: Option<u64>, to: Option<u64>) {
        self.project.program = to;
        if self.project.preview == to {
            self.project.preview = from;
        }
        self.note_going_off(from);
        self.version += 1;
    }

    fn finish_transition(&mut self, tr: Trans) {
        self.trans = None;
        self.land(tr.from, tr.to);
        if let Some(id) = tr.stinger.and_then(|s| s.input) {
            self.note_leaving(id);
        }
    }

    /// Swaps preview and program, as a switcher does. During a transition it
    /// finishes that transition at once.
    pub fn cut(&mut self) {
        if let Some(tr) = self.trans {
            self.finish_transition(tr);
            return;
        }
        let (from, to) = (self.project.program, self.project.preview);
        if self.tbar > 0.0 {
            self.tbar = 0.0;
            self.land(from, to);
            return;
        }
        if to.is_none() {
            return;
        }
        let before = self.on_air();
        self.project.program = to;
        self.project.preview = from;
        self.note_going_on(to, &before);
        self.note_going_off(from);
        self.version += 1;
    }

    /// Straight to air, leaving the preview where it is.
    pub fn cut_to(&mut self, id: u64) {
        if let Some(tr) = self.trans {
            self.finish_transition(tr);
        }
        self.drop_tbar();
        let from = self.project.program;
        if from == Some(id) {
            return;
        }
        let before = self.on_air();
        self.project.program = Some(id);
        self.note_going_on(Some(id), &before);
        self.note_going_off(from);
        self.version += 1;
    }

    /// Lets go of a half-made T-bar move: its target leaves the air again.
    fn drop_tbar(&mut self) {
        if self.tbar > 0.0 {
            self.tbar = 0.0;
            let target = self.project.preview;
            self.note_going_off(target);
            self.version += 1;
        }
    }

    pub fn run_transition(&mut self, kind: TransKind, ms: u32) {
        if kind == TransKind::Cut {
            self.cut();
            return;
        }
        if self.trans.is_some() || self.project.preview.is_none() {
            return;
        }
        let stinger = match kind {
            TransKind::Stinger(n) => Some(self.project.stingers[n]),
            _ => None,
        };
        let ms = stinger.map_or(ms, |s| s.duration_ms).max(1);
        let (from, to) = (self.project.program, self.project.preview);
        // A T-bar part way along hands over from where it stands.
        let already = Duration::from_secs_f32(self.tbar * ms as f32 / 1000.0);
        let held = self.tbar > 0.0;
        self.tbar = 0.0;
        let before = self.on_air();
        let started = Instant::now().checked_sub(already).unwrap_or_else(Instant::now);
        self.trans = Some(Trans { from, to, kind, started, ms, stinger });
        if !held {
            self.note_going_on(to, &before);
        }
        if let Some(id) = stinger.and_then(|s| s.input) {
            self.notes.push(Note::Restart(id));
            self.notes.push(Note::Play(id));
        }
        self.version += 1;
    }

    /// Runs one of the four transition buttons.
    pub fn auto(&mut self, slot: usize) {
        let t = self.project.trans[slot];
        self.run_transition(t.kind, t.ms);
    }

    pub fn quick_play_preview(&mut self) {
        let t = self.project.quick_play;
        self.run_transition(t.kind, t.ms);
    }

    /// Sends one given input to air. If another transition is running it is
    /// finished first, so this request is not lost.
    pub fn take_to_air(&mut self, id: u64, kind: TransKind, ms: u32) {
        if let Some(tr) = self.trans {
            if tr.to == Some(id) {
                return;
            }
            self.finish_transition(tr);
        }
        if self.project.program == Some(id) {
            return;
        }
        self.drop_tbar();
        self.project.preview = Some(id);
        self.version += 1;
        self.run_transition(kind, ms);
    }

    pub fn quick_play(&mut self, id: u64) {
        let t = self.project.quick_play;
        self.take_to_air(id, t.kind, t.ms);
    }

    pub fn set_preview(&mut self, id: u64) {
        // A T-bar in mid-travel is showing the preview on air.
        if self.tbar > 0.0 {
            return;
        }
        self.project.preview = Some(id);
        self.version += 1;
    }

    /// The T-bar drives the first transition button.
    pub fn set_tbar(&mut self, t: f32) {
        if self.trans.is_some() || self.project.preview.is_none() {
            return;
        }
        let was = self.tbar;
        let target = self.project.preview;
        if was == 0.0 && t > 0.0 {
            let before = self.on_air();
            self.note_going_on(target, &before);
        }
        if t >= 0.995 {
            self.tbar = 0.0;
            self.land(self.project.program, target);
            return;
        }
        self.tbar = t.max(0.0);
        if was > 0.0 && self.tbar == 0.0 {
            self.note_going_off(target);
        }
        self.version += 1;
    }

    pub fn toggle_overlay(&mut self, slot: usize, id: u64) {
        let on = self.project.overlays[slot] == Some(id);
        self.set_overlay(slot, if on { None } else { Some(id) });
    }

    pub fn set_overlay(&mut self, slot: usize, id: Option<u64>) {
        let old = self.project.overlays[slot];
        if old == id {
            return;
        }
        let before = self.on_air();
        self.project.overlays[slot] = id;
        self.overlay_since[slot] = id.map(|_| Instant::now());
        if let Some(old) = old {
            self.project.overlay_last[slot] = Some(old);
            self.notes.push(Note::Trigger(old, TriggerEvent::OnOverlayOut));
        }
        if let Some(new) = id {
            self.project.overlay_last[slot] = Some(new);
            self.notes.push(Note::Trigger(new, TriggerEvent::OnOverlayIn));
            self.note_arriving(new, &before);
        }
        self.version += 1;
    }

    /// The channel button: off if on, else back with what it last showed.
    /// A channel that never held anything takes the input in Preview.
    pub fn overlay_button(&mut self, slot: usize) {
        if self.project.overlays[slot].is_some() {
            self.set_overlay(slot, None);
        } else {
            let last = self.project.overlay_last[slot].filter(|id| self.input(*id).is_some());
            self.set_overlay(slot, last.or(self.project.preview));
        }
    }

    pub fn all_overlays_off(&mut self) {
        for slot in 0..OVERLAYS {
            if self.project.overlays[slot].is_some() {
                self.set_overlay(slot, None);
            }
        }
    }

    pub fn advance(&mut self, now: Instant, dt: f32) {
        if let Some(tr) = self.trans
            && now.saturating_duration_since(tr.started).as_millis() as u32 >= tr.ms
        {
            self.finish_transition(tr);
        }
        for i in 0..OVERLAYS {
            let target = self.project.overlays[i];
            let cfg = self.project.overlay_cfg[i];
            let step =
                if cfg.effect == TransKind::Cut { 1.0 } else { dt * 1000.0 / cfg.effect_ms.max(1) as f32 };
            if self.overlay_shown[i] == target {
                if target.is_some() && self.overlay_t[i] < 1.0 {
                    self.overlay_t[i] = (self.overlay_t[i] + step).min(1.0);
                }
            } else if self.overlay_t[i] > 0.0 {
                self.overlay_t[i] = (self.overlay_t[i] - step).max(0.0);
            } else {
                // Only now is the old input really off the screen.
                let old = std::mem::replace(&mut self.overlay_shown[i], target);
                if let Some(old) = old {
                    self.note_leaving(old);
                }
            }
            if let (Some(since), true) = (self.overlay_since[i], cfg.duration_ms > 0)
                && now.saturating_duration_since(since).as_millis() as u32 >= cfg.duration_ms
            {
                self.set_overlay(i, None);
            }
        }
        let dark = if self.ftb { 1.0 } else { 0.0 };
        let step = dt / FTB_SECS;
        self.ftb_level = if self.ftb_level < dark {
            (self.ftb_level + step).min(dark)
        } else {
            (self.ftb_level - step).max(dark)
        };
        self.finish_mix_fades(now);
        // The resting view is the chosen preset itself, so nothing else
        // needs to change when a move ends.
        self.set_moves.retain(|_, m| m.progress(now) < 1.0);
    }

    fn finish_mix_fades(&mut self, now: Instant) {
        let done: Vec<u64> = self
            .mix_fades
            .iter()
            .filter(|(_, f)| now.saturating_duration_since(f.started).as_millis() as u32 >= f.ms)
            .map(|(id, _)| *id)
            .collect();
        for id in done {
            self.mix_fades.remove(&id);
            self.version += 1;
        }
    }

    /// Swaps a Mix input's two pictures, instantly or fading.
    pub fn mix_switch(&mut self, id: u64, fade: bool) {
        let Some(input) = self.input_mut(id) else { return };
        let Kind::Mix { preview, output, ms } = &mut input.kind else { return };
        let from = *output;
        std::mem::swap(preview, output);
        let ms = *ms;
        if fade {
            self.mix_fades.insert(id, MixFade { from, started: Instant::now(), ms: ms.max(1) });
        } else {
            self.mix_fades.remove(&id);
        }
    }

    /// Sends a virtual set's camera to one of its presets, from wherever it
    /// stands now, even part way through another move. `ms` replaces the
    /// set's own move time.
    pub fn set_camera(&mut self, id: u64, n: usize, ms: Option<u32>, now: Instant) -> bool {
        let from = self.set_view(id, now);
        let Some(input) = self.input_mut(id) else { return false };
        let Kind::VirtualSet { cameras, camera, move_ms, .. } = &mut input.kind else { return false };
        let Some(to) = cameras.get(n).map(|c| c.view.clamped()) else { return false };
        *camera = n;
        let ms = ms.unwrap_or(*move_ms);
        match from {
            Some(from) if ms > 0 && from != to => {
                self.set_moves.insert(id, SetMove { from, started: now, ms });
            }
            _ => {
                self.set_moves.remove(&id);
            }
        }
        true
    }

    /// Where a virtual set's camera looks at `now`.
    pub fn set_view(&self, id: u64, now: Instant) -> Option<SetView> {
        let target = crate::sets::resting_view(&self.input(id)?.kind)?;
        Some(match self.set_moves.get(&id) {
            Some(m) => m.from.toward(target, m.progress(now)),
            None => target,
        })
    }

    pub fn on_air(&self) -> HashSet<u64> {
        let mut set: HashSet<u64> = self.overlay_shown.iter().flatten().copied().collect();
        set.extend(self.project.overlays.iter().flatten());
        set.extend(self.project.program);
        if let Some(tr) = self.trans {
            set.extend(tr.from);
            set.extend(tr.to);
            set.extend(tr.stinger.and_then(|s| s.input));
        }
        if self.tbar > 0.0 {
            set.extend(self.project.preview);
        }
        self.with_layers(set)
    }

    /// Adds whatever the inputs in `set` show, however deep.
    pub fn with_layers(&self, mut set: HashSet<u64>) -> HashSet<u64> {
        let mut todo: Vec<u64> = set.iter().copied().collect();
        while let Some(id) = todo.pop() {
            let Some(input) = self.input(id) else { continue };
            let mut inner: Vec<u64> = input.layers.iter().filter(|l| l.visible).map(|l| l.input).collect();
            match input.kind {
                Kind::Mix { preview, output, .. } => {
                    inner.extend(preview);
                    inner.extend(output);
                }
                Kind::VirtualSet { background, foreground, .. } => {
                    inner.extend(background);
                    inner.extend(foreground);
                }
                _ => {}
            }
            for i in inner {
                if set.insert(i) {
                    todo.push(i);
                }
            }
        }
        set
    }

    pub fn close_input(&mut self, id: u64) {
        let Some(at) = self.project.inputs.iter().position(|i| i.id == id) else { return };
        let input = self.project.inputs.remove(at);
        let p = &mut self.project;
        let mut closed = Closed {
            at,
            input,
            layers: Vec::new(),
            overlays: Vec::new(),
            playlist: Vec::new(),
            was_preview: p.preview == Some(id),
            was_program: p.program == Some(id),
        };
        if closed.was_preview {
            p.preview = None;
        }
        if closed.was_program {
            p.program = None;
        }
        for (slot, o) in p.overlays.iter_mut().enumerate() {
            if *o == Some(id) {
                *o = None;
                closed.overlays.push(slot);
            }
        }
        // Nothing may keep pointing at an input that no longer exists.
        let forget = |v: &mut Option<u64>| {
            if *v == Some(id) {
                *v = None;
            }
        };
        for i in &mut p.inputs {
            for (n, l) in i.layers.iter().enumerate().filter(|(_, l)| l.input == id) {
                closed.layers.push((i.id, n, *l));
            }
            i.layers.retain(|l| l.input != id);
            forget(&mut i.key.fill_input);
            match &mut i.kind {
                Kind::Mix { preview, output, .. } => {
                    forget(preview);
                    forget(output);
                }
                Kind::Telestrator { background } => forget(background),
                Kind::VirtualSet { background, foreground, .. } => {
                    forget(background);
                    forget(foreground);
                }
                _ => {}
            }
        }
        p.stingers.iter_mut().for_each(|s| forget(&mut s.input));
        for route in &mut p.fullscreen_route {
            if *route == Route::Input(id) {
                *route = Route::Output;
            }
        }
        for s in &mut p.srt {
            if s.route == Route::Input(id) {
                s.route = Route::Output;
            }
        }
        let mut index = 0;
        let mut removed_before = 0;
        p.playlist.items.retain(|item| {
            let keep = item.input != id;
            if !keep {
                closed.playlist.push((index, item.clone()));
                if index < self.playlist_index {
                    removed_before += 1;
                }
            }
            index += 1;
            keep
        });
        self.playlist_index -= removed_before;
        if let Some(tr) = &mut self.trans {
            forget(&mut tr.from);
            forget(&mut tr.to);
            if let Some(s) = &mut tr.stinger {
                forget(&mut s.input);
            }
        }
        self.mix_fades.remove(&id);
        self.set_moves.remove(&id);
        self.undo = Some(closed);
        self.solo.remove(&id);
        self.version += 1;
    }

    /// Puts a closed input back in every place it was.
    pub fn restore(&mut self, closed: Closed) {
        let input = closed.input;
        let id = input.id;
        let at = closed.at.min(self.project.inputs.len());
        self.project.inputs.insert(at, input);
        for (owner, n, layer) in closed.layers {
            if let Some(i) = self.input_mut(owner) {
                let n = n.min(i.layers.len());
                i.layers.insert(n, layer);
            }
        }
        for (n, item) in closed.playlist {
            let n = n.min(self.project.playlist.items.len());
            self.project.playlist.items.insert(n, item);
        }
        for slot in closed.overlays {
            if self.project.overlays[slot].is_none() {
                self.set_overlay(slot, Some(id));
            }
        }
        if closed.was_preview && self.project.preview.is_none() {
            self.project.preview = Some(id);
        }
        if closed.was_program && self.project.program.is_none() {
            let before = self.on_air();
            self.project.program = Some(id);
            self.note_arriving(id, &before);
        }
        self.version += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        State::new(Project::sample())
    }

    #[test]
    fn the_show_clock_counts_pauses_resumes_and_resets_exactly() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut c = ShowClock::default();
        assert_eq!(c.elapsed(at(50)), Duration::ZERO, "a clock never started stays at zero");
        c.start(at(10));
        assert!(c.running());
        assert_eq!(c.elapsed(at(70)), Duration::from_secs(60));
        c.start(at(40));
        assert_eq!(c.elapsed(at(70)), Duration::from_secs(60), "starting a running clock changes nothing");
        c.pause(at(70));
        assert!(!c.running());
        assert_eq!(c.elapsed(at(500)), Duration::from_secs(60), "paused time does not count");
        c.start(at(600));
        assert_eq!(c.elapsed(at(660)), Duration::from_secs(120));
        c.toggle(at(660));
        assert!(!c.running());
        c.toggle(at(700));
        assert_eq!(c.elapsed(at(700) + Duration::from_secs(62)), Duration::from_secs(182));
        c.reset();
        assert!(!c.running());
        assert_eq!(c.elapsed(at(9000)), Duration::ZERO);
    }

    fn clip(id: u64) -> Input {
        let kind = Kind::Video { path: "x.mp4".into(), looped: false, mark_in: 0, mark_out: 0, rate: 1.0 };
        Input::new(id, "Clip", kind)
    }

    fn later(ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(ms)
    }

    #[test]
    fn blank_project_is_empty_and_unnamed() {
        let p = Project::blank();
        assert!(p.inputs.is_empty());
        assert_eq!(p.categories.len(), 6);
        assert!(p.categories.iter().all(|c| c.name.is_empty()));
        assert_eq!(p.quick_play.ms, 500);
    }

    #[test]
    fn cut_swaps_preview_and_program() {
        let mut s = state();
        s.cut();
        assert_eq!(s.project.program, Some(8));
        assert_eq!(s.project.preview, Some(7));
    }

    #[test]
    fn nothing_in_preview_means_nothing_goes_to_air() {
        let mut s = state();
        s.project.preview = None;
        s.cut();
        s.auto(0);
        s.set_tbar(1.0);
        assert_eq!(s.project.program, Some(7));
    }

    #[test]
    fn cut_to_leaves_the_preview() {
        let mut s = state();
        s.cut_to(3);
        assert_eq!(s.project.program, Some(3));
        assert_eq!(s.project.preview, Some(8));
    }

    #[test]
    fn layers_count_as_on_air() {
        let s = state();
        let air = s.on_air();
        assert!(air.contains(&5) && air.contains(&6));
        assert!(!air.contains(&2));
    }

    #[test]
    fn fade_to_black_reaches_black_and_returns() {
        let mut s = state();
        s.ftb = true;
        s.advance(Instant::now(), 2.0);
        assert_eq!(s.ftb_level, 1.0);
        s.ftb = false;
        s.advance(Instant::now(), 2.0);
        assert_eq!(s.ftb_level, 0.0);
    }

    #[test]
    fn transition_finishes_as_a_swap() {
        let mut s = state();
        s.project.trans[0].ms = 10;
        s.auto(0);
        assert!(s.trans.is_some());
        s.advance(later(50), 0.05);
        assert!(s.trans.is_none());
        assert_eq!(s.project.program, Some(8));
        assert_eq!(s.project.preview, Some(7));
    }

    #[test]
    fn a_preview_chosen_during_a_transition_stays() {
        let mut s = state();
        s.project.trans[0].ms = 10;
        s.auto(0);
        s.set_preview(3);
        s.advance(later(50), 0.05);
        assert_eq!(s.project.program, Some(8));
        assert_eq!(s.project.preview, Some(3));
    }

    #[test]
    fn a_cut_finishes_a_running_transition_without_doubling_triggers() {
        let mut s = state();
        s.project.trans[0].ms = 100_000;
        s.notes.clear();
        s.auto(0);
        s.cut();
        assert!(s.trans.is_none());
        assert_eq!(s.project.program, Some(8));
        let ins = s.notes.iter().filter(|n| **n == Note::Trigger(8, TriggerEvent::OnTransitionIn));
        assert_eq!(ins.count(), 1);
    }

    #[test]
    fn go_on_another_input_during_a_transition_is_not_lost() {
        let mut s = state();
        s.project.trans[0].ms = 100_000;
        s.project.quick_play.ms = 10;
        s.auto(0);
        s.quick_play(3);
        s.advance(later(50), 0.05);
        assert_eq!(s.project.program, Some(3));
    }

    #[test]
    fn pressing_twice_does_not_bounce_back() {
        let mut s = state();
        s.project.quick_play.ms = 100_000;
        s.quick_play(3);
        s.quick_play(3);
        assert_eq!(s.trans.unwrap().to, Some(3));
    }

    #[test]
    fn tbar_at_the_end_commits_and_can_go_back() {
        let mut s = state();
        s.set_tbar(0.5);
        assert_eq!(s.project.program, Some(7));
        s.set_tbar(1.0);
        assert_eq!(s.project.program, Some(8));
        assert_eq!(s.project.preview, Some(7));
        assert_eq!(s.tbar, 0.0);
    }

    #[test]
    fn tbar_is_ignored_during_a_transition() {
        let mut s = state();
        s.project.trans[0].ms = 100_000;
        s.auto(0);
        s.set_tbar(0.5);
        assert!(s.trans.is_some());
    }

    #[test]
    fn a_button_takes_over_from_the_tbar_without_jumping() {
        let mut s = state();
        s.project.trans[0].ms = 1000;
        s.set_tbar(0.5);
        s.auto(0);
        let tr = s.trans.unwrap();
        let t = Instant::now().duration_since(tr.started).as_secs_f32();
        assert!(t >= 0.45, "starts halfway, not from zero");
    }

    #[test]
    fn tbar_back_to_rest_releases_its_target() {
        let mut s = state();
        s.notes.clear();
        s.set_tbar(0.4);
        s.set_tbar(0.0);
        assert!(s.notes.contains(&Note::Trigger(8, TriggerEvent::OnTransitionOut)));
    }

    #[test]
    fn all_eight_overlays_work_and_fade_out() {
        let mut s = state();
        s.toggle_overlay(7, 11);
        s.advance(Instant::now(), 0.01);
        s.advance(Instant::now(), 1.0);
        assert_eq!(s.overlay_t[7], 1.0);
        s.toggle_overlay(7, 11);
        assert_eq!(s.project.overlays[7], None);
        s.advance(Instant::now(), 1.0);
        assert_eq!(s.overlay_t[7], 0.0);
        s.advance(Instant::now(), 0.01);
        assert_eq!(s.overlay_shown[7], None);
    }

    #[test]
    fn overlay_closes_itself_after_its_duration() {
        let mut s = state();
        s.project.overlay_cfg[0].duration_ms = 100;
        s.set_overlay(0, Some(11));
        s.advance(later(200), 0.2);
        assert_eq!(s.project.overlays[0], None);
    }

    #[test]
    fn a_saved_overlay_still_closes_itself() {
        let mut p = Project::sample();
        p.overlays[0] = Some(11);
        p.overlay_cfg[0].duration_ms = 100;
        let mut s = State::new(p);
        s.advance(later(200), 0.2);
        assert_eq!(s.project.overlays[0], None);
    }

    #[test]
    fn overlay_button_returns_to_its_last_input() {
        let mut s = state();
        s.set_overlay(0, Some(11));
        s.set_overlay(0, None);
        s.set_preview(2);
        s.overlay_button(0);
        assert_eq!(s.project.overlays[0], Some(11));
        s.overlay_button(0);
        s.close_input(11);
        s.overlay_button(0);
        assert_eq!(s.project.overlays[0], Some(2), "a closed input cannot return");
    }

    #[test]
    fn a_hidden_overlay_video_pauses_once_it_has_faded_out() {
        let mut s = state();
        s.project.inputs.push(clip(50));
        s.set_overlay(0, Some(50));
        s.advance(Instant::now(), 0.01);
        s.advance(Instant::now(), 1.0);
        s.notes.clear();
        s.set_overlay(0, None);
        assert!(!s.notes.contains(&Note::Pause(50)), "still fading out");
        s.advance(Instant::now(), 1.0);
        s.advance(Instant::now(), 0.01);
        assert!(s.notes.contains(&Note::Pause(50)));
    }

    #[test]
    fn going_on_air_asks_videos_to_play_and_fires_triggers() {
        let mut s = state();
        s.project.inputs.push(clip(50));
        s.input_mut(8).unwrap().layers.push(Layer::new(50, Placement::FULL));
        s.notes.clear();
        s.cut();
        assert!(s.notes.contains(&Note::Trigger(8, TriggerEvent::OnTransitionIn)));
        assert!(s.notes.contains(&Note::Play(50)), "a video inside the scene plays");
        assert!(!s.notes.contains(&Note::Play(1)), "nothing to play in a test card");
        assert!(s.notes.contains(&Note::Trigger(7, TriggerEvent::OnTransitionOut)));
    }

    #[test]
    fn going_on_air_leaves_a_countdown_alone() {
        let mut s = state();
        s.notes.clear();
        s.set_preview(13);
        s.cut();
        assert!(!s.notes.contains(&Note::Play(13)));
        s.cut();
        assert!(!s.notes.contains(&Note::Pause(13)));
    }

    #[test]
    fn a_clip_already_on_air_does_not_jump_back_to_its_start() {
        let mut s = state();
        let mut c = clip(50);
        c.auto_restart = true;
        s.project.inputs.push(c);
        s.cut_to(50);
        s.notes.clear();
        s.set_overlay(0, Some(50));
        assert!(!s.notes.contains(&Note::Restart(50)));
    }

    #[test]
    fn stinger_uses_its_own_duration_and_ignores_later_edits() {
        let mut s = state();
        s.project.stingers[2].duration_ms = 1234;
        s.project.stingers[2].input = Some(2);
        s.run_transition(TransKind::Stinger(2), 10);
        assert_eq!(s.trans.unwrap().ms, 1234);
        s.project.stingers[2].cut_ms = 1;
        assert_eq!(s.trans.unwrap().stinger.unwrap().cut_ms, 1000);
    }

    #[test]
    fn closing_an_input_clears_every_reference_and_undo_restores_them() {
        let mut s = state();
        s.project.stingers[0].input = Some(1);
        s.project.fullscreen_route[0] = Route::Input(1);
        s.project.playlist.items.push(PlaylistItem {
            input: 1,
            start_ms: None,
            duration_ms: 1000,
            trans: TransKind::Fade,
            trans_ms: 100,
            overlay: false,
        });
        s.set_overlay(2, Some(1));
        s.close_input(1);
        assert!(s.input(8).unwrap().layers.is_empty());
        assert_eq!(s.project.stingers[0].input, None);
        assert_eq!(s.project.fullscreen_route[0], Route::Output);
        assert!(s.project.playlist.items.is_empty());
        let closed = s.undo.take().unwrap();
        s.restore(closed);
        assert_eq!(s.input(8).unwrap().layers.len(), 1);
        assert_eq!(s.project.playlist.items.len(), 1);
        assert_eq!(s.project.overlays[2], Some(1));
    }

    #[test]
    fn closing_what_is_on_air_leaves_a_clean_desk() {
        let mut s = state();
        s.project.trans[0].ms = 10;
        s.auto(0);
        s.close_input(8);
        s.advance(later(50), 0.05);
        assert_eq!(s.project.program, None);
    }

    #[test]
    fn the_playlist_keeps_its_place_when_an_earlier_item_closes() {
        let mut s = state();
        for id in [1, 2, 3] {
            s.project.playlist.items.push(PlaylistItem {
                input: id,
                start_ms: None,
                duration_ms: 1000,
                trans: TransKind::Fade,
                trans_ms: 100,
                overlay: false,
            });
        }
        s.playlist_index = 1;
        s.close_input(1);
        assert_eq!(s.playlist_index, 0);
        assert_eq!(s.project.playlist.items[0].input, 2, "next is still the second one");
    }

    #[test]
    fn mix_input_swaps_its_two_pictures_and_a_cut_cancels_a_fade() {
        let mut s = state();
        let id = 99;
        let mix = Kind::Mix { preview: Some(1), output: Some(2), ms: 500 };
        s.project.inputs.push(Input::new(id, "Mix", mix));
        s.mix_switch(id, true);
        assert!(matches!(s.input(id).unwrap().kind, Kind::Mix { preview: Some(2), output: Some(1), .. }));
        assert!(s.mix_fades.contains_key(&id));
        s.mix_switch(id, false);
        assert!(!s.mix_fades.contains_key(&id));
    }

    fn with_set() -> (State, u64) {
        let mut s = state();
        let id = 98;
        s.project.inputs.push(Input::new(id, "Plateau", crate::sets::new_set(SetLook::Studio)));
        (s, id)
    }

    /// Asked for another preset mid-move, the camera turns from where it
    /// stands instead of jumping back to where it started.
    #[test]
    fn a_set_camera_changes_course_without_a_jump() {
        let (mut s, id) = with_set();
        let t0 = Instant::now();
        let wide = s.set_view(id, t0).unwrap();
        assert!(s.set_camera(id, 2, Some(1000), t0));
        assert_eq!(s.set_view(id, t0), Some(wide), "a move starts where the camera was");
        let half = t0 + Duration::from_millis(500);
        let there = s.set_view(id, half).unwrap();
        assert!(there.zoom > wide.zoom && there.zoom < 2.4);
        assert!(s.set_camera(id, 3, Some(1000), half));
        assert_eq!(s.set_view(id, half), Some(there));
        let end = half + Duration::from_millis(1000);
        s.advance(end, 0.0);
        assert!(s.set_moves.is_empty(), "the move is over");
        let screen = s.set_view(id, end).unwrap();
        assert!((screen.zoom - 2.0).abs() < 1e-5 && (screen.x - 0.73).abs() < 1e-5);
    }

    #[test]
    fn a_set_camera_with_no_time_cuts_and_bad_presets_are_refused() {
        let (mut s, id) = with_set();
        let now = Instant::now();
        assert!(s.set_camera(id, 1, Some(0), now));
        assert!(s.set_moves.is_empty());
        assert!((s.set_view(id, now).unwrap().zoom - 1.5).abs() < 1e-5);
        assert!(!s.set_camera(id, 9, None, now), "no such preset");
        assert!(!s.set_camera(1, 0, None, now), "not a set");
        assert!(matches!(s.input(id).unwrap().kind, Kind::VirtualSet { camera: 1, .. }));
    }

    /// A video behind the presenters must play when the set goes on air.
    #[test]
    fn a_set_shows_its_own_pictures_and_forgets_closed_ones() {
        let (mut s, id) = with_set();
        if let Some(Kind::VirtualSet { background, foreground, .. }) = s.input_mut(id).map(|i| &mut i.kind) {
            *background = Some(5);
            *foreground = Some(6);
        }
        s.input_mut(id).unwrap().layers.push(Layer::new(1, crate::sets::presenter_spot(0)));
        let shown = s.with_layers(HashSet::from([id]));
        assert!([1, 5, 6].iter().all(|i| shown.contains(i)), "{shown:?}");
        s.close_input(5);
        assert!(matches!(
            s.input(id).unwrap().kind,
            Kind::VirtualSet { background: None, foreground: Some(6), .. }
        ));
    }
}
