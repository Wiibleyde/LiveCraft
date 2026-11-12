//! What the saved data must keep doing as the program changes.

use crate::model::*;

/// A file saved by an earlier version has fewer fields, and must still open.
#[test]
fn older_files_keep_opening() {
    let mut v = serde_json::to_value(Project::sample()).unwrap();
    let p = v.as_object_mut().unwrap();
    p.remove("overlay_last");
    p.remove("layer_templates");
    p.remove("hardware_decoding");
    for input in p["inputs"].as_array_mut().unwrap() {
        let i = input.as_object_mut().unwrap();
        i.remove("auto_pause");
        i["audio"].as_object_mut().unwrap().remove("delay_ms");
        if let Some(title) = i["kind"].get_mut("Title").and_then(|t| t.as_object_mut()) {
            title.remove("font");
            title.remove("size");
        }
    }
    let back: Project = serde_json::from_value(v).unwrap();
    assert_eq!(back.inputs.len(), Project::sample().inputs.len());
    assert!(back.hardware_decoding, "the card helps unless someone turned it off");
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
}

/// Later versions will add to sets too: what is missing takes its default.
#[test]
fn a_virtual_set_with_missing_fields_still_opens() {
    let kind: Kind = serde_json::from_str(r#"{"VirtualSet":{"cameras":[{"name":"Large"}]}}"#).unwrap();
    let Kind::VirtualSet { look, background, cameras, camera, .. } = kind else { panic!("{kind:?}") };
    assert_eq!((look, background, camera), (SetLook::Studio, None, 0));
    assert_eq!(cameras[0].view, SetView::default());
}
