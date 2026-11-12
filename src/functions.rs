//! One list of named functions behind shortcuts, triggers, GO buttons,
//! scripts and remote control. The names follow the published shortcut
//! reference, so existing control surfaces and scripts carry over.

use crate::engine::{Engine, State};
use crate::model::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

#[derive(Default, Clone, Debug)]
pub struct Args {
    pub input: Option<String>,
    pub value: Option<String>,
    pub duration: Option<u32>,
    pub selected_name: Option<String>,
}

impl Args {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn input(mut self, v: &str) -> Self {
        if !v.is_empty() {
            self.input = Some(v.to_string());
        }
        self
    }

    pub fn value(mut self, v: &str) -> Self {
        if !v.is_empty() {
            self.value = Some(v.to_string());
        }
        self
    }

    pub fn from_query(q: &HashMap<String, String>) -> Self {
        let get = |k: &str| q.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone());
        Self {
            input: get("Input"),
            value: get("Value"),
            duration: get("Duration").and_then(|v| v.parse().ok()),
            selected_name: get("SelectedName"),
        }
    }

    fn number(&self) -> Result<f32, String> {
        let v = self.value.as_deref().ok_or(t!("functions.missing_value"))?;
        v.trim().parse().map_err(|_| t!("functions.unreadable_value", value = v))
    }
}

/// Input by number (1 and up; 0 Preview, -1 Output), name or key.
pub fn resolve(s: &State, spec: Option<&str>) -> Option<u64> {
    let spec = spec?.trim();
    if let Ok(n) = spec.parse::<i64>() {
        return match n {
            0 => s.project.preview,
            -1 => s.project.program,
            n if n > 0 => s.project.inputs.get(n as usize - 1).map(|i| i.id),
            _ => None,
        };
    }
    s.project.inputs.iter().find(|i| i.name == spec || i.key_id().eq_ignore_ascii_case(spec)).map(|i| i.id)
}

/// Every function reached by its exact name below.
pub const NAMES: &[&str] = &[
    "ActiveInput",
    "Audio",
    "AudioAuto",
    "AudioAutoOff",
    "AudioAutoOn",
    "AudioBus",
    "AudioBusOff",
    "AudioBusOn",
    "AudioOff",
    "AudioOn",
    "CutDirect",
    "FadeToBlack",
    "LayerOff",
    "LayerOn",
    "LayerOnOff",
    "Loop",
    "LoopOff",
    "LoopOn",
    "MasterAudio",
    "MasterAudioOff",
    "MasterAudioOn",
    "MirrorOff",
    "MirrorOn",
    "NextItem",
    "NextPlayListEntry",
    "Pause",
    "PauseCountdown",
    "Play",
    "PlayPause",
    "PreviewInput",
    "PreviewInputNext",
    "PreviewInputPrevious",
    "PreviousItem",
    "QuickPlay",
    "RemoveInput",
    "Restart",
    "ScriptStart",
    "ScriptStopAll",
    "SelectIndex",
    "SetAlpha",
    "SetBalance",
    "SetCountdown",
    "SetFader",
    "SetGain",
    "SetInputName",
    "SetMasterVolume",
    "SetPanX",
    "SetPanY",
    "SetPosition",
    "SetText",
    "SetVolume",
    "SetZoom",
    "Solo",
    "SoloAllOff",
    "SoloOff",
    "SoloOn",
    "StartCountdown",
    "StartMultiCorder",
    "StartPlayList",
    "StartRecording",
    "StartSRTOutput",
    "StartStopMultiCorder",
    "StartStopRecording",
    "StartStopStreaming",
    "StartStreaming",
    "StopCountdown",
    "StopMultiCorder",
    "StopPlayList",
    "StopRecording",
    "StopSRTOutput",
    "StopStreaming",
    "Undo",
];

fn strip_prefix_any_case<'a>(name: &'a str, prefix: &str) -> Option<&'a str> {
    let head = name.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &name[prefix.len()..])
}

fn volume_from_value(v: f32) -> f32 {
    (v / 100.0).clamp(0.0, 1.2)
}

fn parse_time(v: &str) -> Option<u64> {
    let mut total = 0f64;
    for part in v.split(':') {
        total = total * 60.0 + part.trim().parse::<f64>().ok()?;
    }
    Some((total * 1000.0) as u64)
}

impl Engine {
    fn target(&self, a: &Args) -> Result<u64, String> {
        let s = self.state.lock().unwrap();
        // A function with no input means the one in Preview, as on the desk.
        resolve(&s, a.input.as_deref().or(Some("0")))
            .ok_or_else(|| t!("functions.input_not_found").to_string())
    }

    fn with_input(&self, a: &Args, f: impl FnOnce(&mut Input)) -> Result<(), String> {
        let id = self.target(a)?;
        let mut s = self.state.lock().unwrap();
        s.input_mut(id).map(f).ok_or_else(|| t!("functions.input_not_found").to_string())
    }

    /// Runs a function by name, written in any case, as control surfaces and
    /// hand-written scripts vary. Unknown names are an error, not silence.
    pub fn call(self: &Arc<Self>, name: &str, a: &Args) -> Result<(), String> {
        let name = NAMES.iter().find(|n| n.eq_ignore_ascii_case(name)).copied().unwrap_or(name);
        if let Some(kind) = TransKind::from_api(name) {
            return self.transition(kind, a);
        }
        if let Some(rest) = strip_prefix_any_case(name, "OverlayInput") {
            return self.overlay_function(rest, a);
        }
        if let Some(n) = strip_prefix_any_case(name, "Transition").and_then(|v| v.parse::<usize>().ok())
            && (1..=4).contains(&n)
        {
            self.state.lock().unwrap().auto(n - 1);
            return Ok(());
        }
        match name {
            "CutDirect" => {
                let id = self.target(a)?;
                self.state.lock().unwrap().cut_to(id);
            }
            "QuickPlay" => {
                let id = self.target(a)?;
                self.state.lock().unwrap().quick_play(id);
            }
            "FadeToBlack" => {
                let mut s = self.state.lock().unwrap();
                s.ftb = !s.ftb;
            }
            "SetFader" => {
                let v = a.number()? / 255.0;
                self.state.lock().unwrap().set_tbar(v);
            }
            "PreviewInput" => {
                let id = self.target(a)?;
                self.state.lock().unwrap().set_preview(id);
            }
            "PreviewInputNext" | "PreviewInputPrevious" => {
                let step = if name.ends_with("Next") { 1 } else { -1 };
                let mut s = self.state.lock().unwrap();
                let list: Vec<u64> = s.project.inputs.iter().map(|i| i.id).collect();
                if !list.is_empty() {
                    let at = s.project.preview.and_then(|p| list.iter().position(|i| *i == p));
                    let n = list.len() as i64;
                    let i = (at.map_or(0, |v| v as i64 + step) + n) % n;
                    s.set_preview(list[i as usize]);
                }
            }
            "ActiveInput" => {
                let id = self.target(a)?;
                self.state.lock().unwrap().cut_to(id);
            }
            "Play" | "Pause" | "PlayPause" | "Restart" | "Loop" | "LoopOn" | "LoopOff" | "SetPosition"
            | "NextItem" | "PreviousItem" | "SelectIndex" => {
                return self.playback_function(name, a);
            }
            "StartCountdown" | "StopCountdown" | "PauseCountdown" | "SetCountdown" => {
                return self.countdown_function(name, a);
            }
            "SetText" => return self.set_text(a),
            "SetInputName" => {
                let v = a.value.clone().unwrap_or_default();
                self.with_input(a, |i| i.name = v)?;
            }
            "SetZoom" => {
                let v = a.number()?;
                self.with_input(a, |i| i.position.zoom = v)?;
            }
            "SetPanX" => {
                let v = a.number()?;
                self.with_input(a, |i| i.position.pan_x = v)?;
            }
            "SetPanY" => {
                let v = a.number()?;
                self.with_input(a, |i| i.position.pan_y = v)?;
            }
            "SetAlpha" => {
                let v = a.number()? / 255.0;
                self.with_input(a, |i| i.colour.alpha = v)?;
            }
            "MirrorOn" => self.with_input(a, |i| i.mirror = true)?,
            "MirrorOff" => self.with_input(a, |i| i.mirror = false)?,
            "RemoveInput" => {
                let id = self.target(a)?;
                self.close_input(id);
            }
            "LayerOn" | "LayerOff" | "LayerOnOff" => return self.layer_function(name, a),
            "Audio" | "AudioOn" | "AudioOff" => {
                self.with_input(a, |i| {
                    i.audio.muted = match name {
                        "AudioOn" => false,
                        "AudioOff" => true,
                        _ => !i.audio.muted,
                    }
                })?;
            }
            "AudioAuto" | "AudioAutoOn" | "AudioAutoOff" => {
                self.with_input(a, |i| {
                    i.audio.follow = match name {
                        "AudioAutoOn" => true,
                        "AudioAutoOff" => false,
                        _ => !i.audio.follow,
                    }
                })?;
            }
            "SetVolume" => {
                let v = volume_from_value(a.number()?);
                self.with_input(a, |i| i.audio.volume = v)?;
            }
            "SetBalance" => {
                let v = a.number()?;
                self.with_input(a, |i| i.audio.balance = v)?;
            }
            "SetGain" => {
                let v = a.number()?;
                self.with_input(a, |i| i.audio.gain_db = v)?;
            }
            "SetMasterVolume" => {
                let v = volume_from_value(a.number()?);
                self.state.lock().unwrap().project.master_level = v;
            }
            "MasterAudio" | "MasterAudioOn" | "MasterAudioOff" => {
                let mut s = self.state.lock().unwrap();
                s.project.master_muted = match name {
                    "MasterAudioOn" => false,
                    "MasterAudioOff" => true,
                    _ => !s.project.master_muted,
                };
            }
            "AudioBus" | "AudioBusOn" | "AudioBusOff" => {
                let letter = a.value.as_deref().unwrap_or("M").to_uppercase();
                let bit = "MABCDEFG".find(&letter).ok_or(t!("functions.unknown_bus"))?;
                self.with_input(a, |i| {
                    let mask = 1u8 << bit;
                    i.audio.buses = match name {
                        "AudioBusOn" => i.audio.buses | mask,
                        "AudioBusOff" => i.audio.buses & !mask,
                        _ => i.audio.buses ^ mask,
                    };
                })?;
            }
            "Solo" | "SoloOn" | "SoloOff" | "SoloAllOff" => {
                if name == "SoloAllOff" {
                    self.state.lock().unwrap().solo.clear();
                } else {
                    let id = self.target(a)?;
                    let mut s = self.state.lock().unwrap();
                    let on = match name {
                        "SoloOn" => true,
                        "SoloOff" => false,
                        _ => !s.solo.contains(&id),
                    };
                    if on {
                        s.solo.insert(id);
                    } else {
                        s.solo.remove(&id);
                    }
                }
            }
            "StartRecording" => {
                self.start_recording(&default_record_dir())?;
            }
            "StopRecording" => self.stop_recording(),
            "StartStopRecording" => {
                if self.is_recording() {
                    self.stop_recording();
                } else {
                    self.start_recording(&default_record_dir())?;
                }
            }
            "StartStreaming" => {
                self.start_streaming()?;
            }
            "StopStreaming" => self.stop_streaming(),
            "StartStopStreaming" => {
                if self.is_streaming() {
                    self.stop_streaming();
                } else {
                    self.start_streaming()?;
                }
            }
            "StartMultiCorder" => self.start_multi_live()?,
            "StopMultiCorder" => self.stop_multi(),
            "StartStopMultiCorder" => {
                if self.is_multi() {
                    self.stop_multi();
                } else {
                    self.start_multi_live()?;
                }
            }
            "StartSRTOutput" => {
                self.start_srt()?;
            }
            "StopSRTOutput" => self.stop_srt(),
            "StartPlayList" => {
                let mut s = self.state.lock().unwrap();
                s.playlist_on = true;
                s.playlist_index = 0;
                s.playlist_next = None;
            }
            "StopPlayList" => self.state.lock().unwrap().playlist_on = false,
            "NextPlayListEntry" => self.playlist_step(),
            "Undo" => self.undo(),
            "ScriptStart" => {
                let wanted = a.value.clone().unwrap_or_default();
                let code = {
                    let s = self.state.lock().unwrap();
                    s.project.scripts.iter().find(|x| x.name == wanted).map(|x| x.code.clone())
                };
                self.run_script(code.ok_or(t!("functions.script_not_found"))?);
            }
            "ScriptStopAll" => {
                self.script_stop.fetch_add(1, Ordering::Relaxed);
            }
            _ => return Err(t!("functions.unknown_function", name = name)),
        }
        Ok(())
    }

    fn start_multi_live(&self) -> Result<(), String> {
        let ids: Vec<u64> = {
            let s = self.state.lock().unwrap();
            let live = s.project.inputs.iter().filter(|i| i.kind.is_recordable());
            live.map(|i| i.id).collect()
        };
        self.start_multi(&default_record_dir(), &ids, AudioPick::MASTER).map(|_| ())
    }

    /// `Fade` and friends: with an input, that input goes through Preview.
    fn transition(&self, kind: TransKind, a: &Args) -> Result<(), String> {
        let mut s = self.state.lock().unwrap();
        if a.input.is_some() {
            let id = resolve(&s, a.input.as_deref()).ok_or(t!("functions.input_not_found"))?;
            s.set_preview(id);
        }
        s.run_transition(kind, a.duration.unwrap_or(1000));
        Ok(())
    }

    /// OverlayInput1, OverlayInput1In, OverlayInput1Out, OverlayInput1Off.
    fn overlay_function(&self, rest: &str, a: &Args) -> Result<(), String> {
        if rest.eq_ignore_ascii_case("AllOff") {
            self.state.lock().unwrap().all_overlays_off();
            return Ok(());
        }
        let digit =
            rest.chars().next().and_then(|c| c.to_digit(10)).ok_or(t!("functions.unknown_channel"))?;
        if !(1..=8).contains(&digit) {
            return Err(t!("functions.unknown_channel").into());
        }
        let slot = digit as usize - 1;
        let action = rest[1..].to_ascii_lowercase();
        let id = self.target(a);
        let mut s = self.state.lock().unwrap();
        match action.as_str() {
            "" => s.toggle_overlay(slot, id?),
            "in" => s.set_overlay(slot, Some(id?)),
            "out" | "off" => s.set_overlay(slot, None),
            "last" => s.overlay_button(slot),
            _ => return Err(t!("functions.unknown_function", name = format!("OverlayInput{rest}"))),
        }
        Ok(())
    }

    fn playback_function(&self, name: &str, a: &Args) -> Result<(), String> {
        let id = self.target(a)?;
        let rt = self.rt(id).ok_or(t!("functions.input_not_found"))?;
        match name {
            "Play" => rt.playing.store(true, Ordering::Relaxed),
            "Pause" => rt.playing.store(false, Ordering::Relaxed),
            "PlayPause" => rt.playing.store(!rt.is_playing(), Ordering::Relaxed),
            "Restart" => rt.seek_ms.store(0, Ordering::Relaxed),
            "SetPosition" => rt.seek_ms.store(a.number()? as i64, Ordering::Relaxed),
            "SelectIndex" => rt.select.store(a.number()? as i64 - 1, Ordering::Relaxed),
            "NextItem" | "PreviousItem" => {
                let step = if name == "NextItem" { 1 } else { -1 };
                let count = rt.items.load(Ordering::Relaxed).max(1) as i64;
                let at = rt.item.load(Ordering::Relaxed) as i64;
                rt.select.store((at + step + count) % count, Ordering::Relaxed);
            }
            _ => {
                {
                    let mut s = self.state.lock().unwrap();
                    if let Some(Kind::Video { looped, .. }) = s.input_mut(id).map(|i| &mut i.kind) {
                        *looped = match name {
                            "LoopOn" => true,
                            "LoopOff" => false,
                            _ => !*looped,
                        };
                    }
                }
                self.restart_input(id, true);
            }
        }
        Ok(())
    }

    fn countdown_function(&self, name: &str, a: &Args) -> Result<(), String> {
        let id = self.target(a)?;
        let rt = self.rt(id).ok_or(t!("functions.input_not_found"))?;
        match name {
            "StartCountdown" => rt.playing.store(true, Ordering::Relaxed),
            "PauseCountdown" => rt.playing.store(false, Ordering::Relaxed),
            "StopCountdown" => {
                rt.playing.store(false, Ordering::Relaxed);
                rt.seek_ms.store(0, Ordering::Relaxed);
            }
            _ => {
                let v = a.value.as_deref().unwrap_or("");
                let ms = parse_time(v).ok_or_else(|| t!("functions.unreadable_duration", value = v))?;
                rt.dur_ms.store(ms, Ordering::Relaxed);
                rt.seek_ms.store(0, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    fn set_text(&self, a: &Args) -> Result<(), String> {
        let id = self.target(a)?;
        let value = a.value.clone().unwrap_or_default();
        let second =
            a.selected_name.as_deref().is_some_and(|n| n.to_lowercase().contains("sub") || n.ends_with('1'));
        {
            let mut s = self.state.lock().unwrap();
            let kind = s.input_mut(id).map(|i| &mut i.kind).ok_or(t!("functions.input_not_found"))?;
            let Kind::Title { text, sub, .. } = kind else {
                return Err(t!("functions.not_a_title").into());
            };
            *(if second { sub } else { text }) = value;
        }
        self.restart_input(id, true);
        Ok(())
    }

    fn layer_function(&self, name: &str, a: &Args) -> Result<(), String> {
        let n = a.number()? as usize;
        self.with_input(a, |i| {
            if let Some(l) = n.checked_sub(1).and_then(|n| i.layers.get_mut(n)) {
                l.visible = match name {
                    "LayerOn" => true,
                    "LayerOff" => false,
                    _ => !l.visible,
                };
            }
        })
    }
}

fn default_record_dir() -> String {
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
    std::path::Path::new(&home).join("Videos").join("LiveCraft").to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_resolve_by_number_name_and_key() {
        let s = State::new(Project::sample());
        assert_eq!(resolve(&s, Some("1")), Some(1));
        assert_eq!(resolve(&s, Some("0")), Some(8));
        assert_eq!(resolve(&s, Some("-1")), Some(7));
        assert_eq!(resolve(&s, Some("CAM 2")), Some(2));
        let key = s.input(3).unwrap().key_id();
        assert_eq!(resolve(&s, Some(&key)), Some(3));
        assert_eq!(resolve(&s, Some("nobody")), None);
    }

    #[test]
    fn countdown_times_read_as_clock_values() {
        assert_eq!(parse_time("00:01:30"), Some(90_000));
        assert_eq!(parse_time("45"), Some(45_000));
        assert_eq!(parse_time("x"), None);
    }

    /// A name missing from the list would work only with its exact capitals.
    #[test]
    fn every_function_below_is_in_the_list_of_names() {
        let source = include_str!("functions.rs");
        let start = source.find("        match name {").unwrap();
        let end = source.find("fn default_record_dir").unwrap();
        for (n, text) in source[start..end].split('"').enumerate() {
            let is_name = n % 2 == 1
                && !["Next", "AllOff", "MABCDEFG"].contains(&text)
                && text.len() > 2
                && text.starts_with(|c: char| c.is_ascii_uppercase())
                && text.chars().all(|c| c.is_ascii_alphanumeric());
            if is_name {
                assert!(NAMES.contains(&text), "{text} n'est pas dans NAMES");
            }
        }
    }

    #[test]
    fn query_names_ignore_case() {
        let mut q = HashMap::new();
        q.insert("input".to_string(), "3".to_string());
        q.insert("DURATION".to_string(), "500".to_string());
        let a = Args::from_query(&q);
        assert_eq!(a.input.as_deref(), Some("3"));
        assert_eq!(a.duration, Some(500));
    }
}
