//! What the saved data must keep doing as the program changes.

use crate::model::*;

/// A file saved by an earlier version has fewer fields, and must still open.
#[test]
fn older_files_keep_opening() {
    let mut v = serde_json::to_value(Project::sample()).unwrap();
    let p = v.as_object_mut().unwrap();
    p.remove("overlay_last");
    p.remove("layer_templates");
    p.remove("master_plugins");
    for bus in p["buses"].as_array_mut().unwrap() {
        bus.as_object_mut().unwrap().remove("plugins");
    }
    p.remove("ndi_out");
    p.remove("omt_out");
    for input in p["inputs"].as_array_mut().unwrap() {
        let i = input.as_object_mut().unwrap();
        i.remove("auto_pause");
        let audio = i["audio"].as_object_mut().unwrap();
        audio.remove("delay_ms");
        audio.remove("plugins");
        if let Some(title) = i["kind"].get_mut("Title").and_then(|t| t.as_object_mut()) {
            title.remove("font");
            title.remove("size");
        }
    }
    let back: Project = serde_json::from_value(v).unwrap();
    assert_eq!(back.inputs.len(), Project::sample().inputs.len());
    assert_eq!(back.ndi_out, NetOutCfg::default(), "les sorties réseau restent éteintes");
    assert!(!back.omt_out.enabled);
}

#[test]
fn an_editor_only_writes_what_it_changed() {
    let before = Input::new(1, "A", Kind::Bars);
    let mut after = before.clone();
    after.name = "B".into();
    let mut live = before.clone();
    live.audio.muted = true;
    live.merge_changes(&before, after);
    assert_eq!(live.name, "B");
    assert!(live.audio.muted, "changed elsewhere meanwhile, must stay");
}

#[test]
fn a_project_file_leaves_its_secrets_behind() {
    let mut p = Project::blank();
    p.streams[0].key = "cle".into();
    p.srt[0].url = "srt://h:9000?mode=caller&passphrase=abc&latency=200".into();
    let url = "srt://h:1?passphrase=abc".to_string();
    p.inputs.push(Input::new(1, "N", Kind::Network { url, buffer_ms: 0 }));
    let p = p.without_secrets();
    assert_eq!(p.streams[0].key, "");
    assert_eq!(p.srt[0].url, "srt://h:9000?mode=caller&latency=200");
    assert_eq!(p.inputs[0].kind, Kind::Network { url: "srt://h:1".into(), buffer_ms: 0 });
}

#[test]
fn a_new_setting_is_not_a_new_source() {
    let video =
        |path: &str, rate| Kind::Video { path: path.into(), looped: false, mark_in: 0, mark_out: 0, rate };
    assert!(video("a", 1.0).same_source(&video("a", 2.0)));
    assert!(!video("a", 1.0).same_source(&video("b", 1.0)));
    assert!(!Kind::Bars.same_source(&Kind::Clock));
    // Another presentation starts at its first slide, not where the last one was.
    let deck = |path: &str| Kind::PowerPoint { path: path.into() };
    assert!(!deck("a.pptx").same_source(&deck("b.pptx")));
}

fn call(token: &str, turn_password: &str) -> Kind {
    Kind::VideoCall {
        token: token.into(),
        address: "studio.example".into(),
        stun: "stun:s:3478".into(),
        turn: "turn:t:3478".into(),
        turn_user: "u".into(),
        turn_password: turn_password.into(),
        return_sound: ReturnSound::Bus(2),
    }
}

#[test]
fn a_project_file_leaves_the_guest_link_behind() {
    let mut p = Project::blank();
    p.inputs.push(Input::new(1, "Invité", call("0123456789abcdef", "mdp")));
    let p = p.without_secrets();
    assert_eq!(p.inputs[0].kind, call("", ""), "the link and the TURN password stay on this computer");
}

#[test]
fn a_call_saved_with_fewer_settings_still_opens() {
    let json = r#"{"inputs":[{"id":1,"name":"Invité","kind":{"VideoCall":{"token":"ab"}}}]}"#;
    let p: Project = serde_json::from_str(json).unwrap();
    let Kind::VideoCall { token, return_sound, stun, .. } = &p.inputs[0].kind else { panic!() };
    assert_eq!((token.as_str(), *return_sound, stun.as_str()), ("ab", ReturnSound::MasterMinusGuest, ""));
}

#[test]
fn changing_what_a_guest_hears_keeps_them_connected() {
    let before = call("a", "p");
    let mut after = before.clone();
    if let Kind::VideoCall { return_sound, address, .. } = &mut after {
        *return_sound = ReturnSound::Off;
        address.clear();
    }
    assert!(!after.restarts_source(&before));
    assert!(call("b", "p").restarts_source(&before), "a new link drops the old guest");
    assert!(call("a", "q").restarts_source(&before), "new TURN settings need a new call");
    assert!(Kind::Clock.restarts_source(&Kind::Bars));
}

/// Later versions will add to sets too: what is missing takes its default.
#[test]
fn a_virtual_set_with_missing_fields_still_opens() {
    let kind: Kind = serde_json::from_str(r#"{"VirtualSet":{"cameras":[{"name":"Large"}]}}"#).unwrap();
    let Kind::VirtualSet { look, background, cameras, camera, .. } = kind else { panic!("{kind:?}") };
    assert_eq!((look, background, camera), (SetLook::Studio, None, 0));
    assert_eq!(cameras[0].view, SetView::default());
}
