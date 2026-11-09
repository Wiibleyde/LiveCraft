//! Interface tests: the real window and engine, driven by real clicks.

use super::*;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use std::thread::sleep;

fn saved(project: Project) -> Saved {
    Saved::fresh(project)
}

fn desk(project: Project) -> Harness<'static, App> {
    // Steps short enough for two clicks to count as a double click.
    let builder =
        Harness::builder().with_size(vec2(2200.0, 1100.0)).with_step_dt(0.05).with_accessibility_check(false);
    let mut h =
        builder.build_eframe(move |cc| App::assemble(cc, saved(project), None, "", None, None, false));
    h.run_steps(4);
    h
}

fn small_project() -> Project {
    let mut p = Project::sample();
    p.out_w = 320;
    p.out_h = 180;
    p.inputs.iter_mut().for_each(|i| i.compact = false);
    for t in &mut p.trans {
        t.ms = 60;
    }
    p.quick_play.ms = 60;
    p
}

fn click(h: &mut Harness<'static, App>, label: &str) {
    h.get_by_label(label).click();
    h.run_steps(3);
}

fn wait(h: &mut Harness<'static, App>, ms: u64) {
    sleep(Duration::from_millis(ms));
    h.run_steps(3);
}

/// Live threads take their time, more so with many tests running at once.
fn wait_for_air(h: &mut Harness<'static, App>, target: Option<u64>, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.state().with(|s| s.project.program != target || s.trans.is_some()) {
        assert!(Instant::now() < deadline, "{what} n'est pas arrivé au direct");
        wait(h, 20);
    }
}

fn project(h: &Harness<'static, App>) -> Project {
    h.state().with(|s| s.project.clone())
}

fn end(h: Harness<'static, App>) {
    h.state().engine.shutdown();
}

/// The reported bug: hide a channel, reshow it, the same input comes back.
#[test]
fn a_channel_brings_back_what_it_showed_not_the_preview() {
    let mut h = desk(small_project());
    click(&mut h, "Incrustation 1 Bandeau titre");
    assert_eq!(project(&h).overlays[0], Some(11));
    click(&mut h, "Entrée CAM 2");
    assert_eq!(project(&h).preview, Some(2));

    click(&mut h, "Canal 1");
    assert_eq!(project(&h).overlays[0], None);
    click(&mut h, "Canal 1");
    assert_eq!(project(&h).overlays[0], Some(11), "must not take the preview");
    click(&mut h, "Canal 1");
    click(&mut h, "Canal 1");
    assert_eq!(project(&h).overlays[0], Some(11), "still the same on a second round");
    end(h);
}

#[test]
fn an_unused_channel_takes_the_preview() {
    let mut h = desk(small_project());
    click(&mut h, "Entrée Horloge");
    click(&mut h, "Canal 5");
    assert_eq!(project(&h).overlays[4], Some(14));
    end(h);
}

#[test]
fn each_channel_keeps_its_own_memory() {
    let mut h = desk(small_project());
    click(&mut h, "Incrustation 2 Bandeau titre");
    click(&mut h, "Incrustation 3 Horloge");
    click(&mut h, "Canal 2");
    click(&mut h, "Canal 3");
    click(&mut h, "Canal 3");
    click(&mut h, "Canal 2");
    let p = project(&h);
    assert_eq!(p.overlays[1], Some(11));
    assert_eq!(p.overlays[2], Some(14));
    end(h);
}

#[test]
fn card_overlay_buttons_toggle_and_replace() {
    let mut h = desk(small_project());
    click(&mut h, "Incrustation 8 Bandeau titre");
    assert_eq!(project(&h).overlays[7], Some(11));
    click(&mut h, "Incrustation 8 Horloge");
    assert_eq!(project(&h).overlays[7], Some(14));
    click(&mut h, "Incrustation 8 Horloge");
    assert_eq!(project(&h).overlays[7], None);
    click(&mut h, "Canal 8");
    assert_eq!(project(&h).overlays[7], Some(14), "the one shown last");
    end(h);
}

#[test]
fn a_closed_input_does_not_come_back_on_a_channel() {
    let mut h = desk(small_project());
    click(&mut h, "Incrustation 1 Horloge");
    click(&mut h, "Canal 1");
    click(&mut h, "Fermer Horloge");
    click(&mut h, "Entrée CAM 2");
    click(&mut h, "Canal 1");
    assert_eq!(project(&h).overlays[0], Some(2));
    end(h);
}

#[test]
fn clicking_a_card_previews_and_cut_swaps() {
    let mut h = desk(small_project());
    click(&mut h, "Entrée CAM 3");
    assert_eq!(project(&h).preview, Some(3));
    let before = project(&h).program;
    click(&mut h, "Coupe");
    let p = project(&h);
    assert_eq!(p.program, Some(3));
    assert_eq!(p.preview, before);
    end(h);
}

#[test]
fn quick_play_and_every_transition_button_reach_air() {
    let mut h = desk(small_project());
    for button in ["Passer au direct", "Fondu", "Fusion", "Rideau", "Zoom"] {
        let target = project(&h).preview;
        click(&mut h, button);
        wait_for_air(&mut h, target, button);
    }
    end(h);
}

#[test]
fn card_go_and_card_cut_work_on_their_own_input() {
    let mut h = desk(small_project());
    click(&mut h, "GO CAM 3");
    wait_for_air(&mut h, Some(3), "GO");
    click(&mut h, "Coupe CAM 2");
    assert_eq!(project(&h).program, Some(2));
    end(h);
}

#[test]
fn fade_to_black_turns_on_and_off() {
    let mut h = desk(small_project());
    click(&mut h, "Fondu au noir");
    assert!(h.state().with(|s| s.ftb));
    click(&mut h, "Fondu au noir");
    assert!(!h.state().with(|s| s.ftb));
    end(h);
}

#[test]
fn tabs_filter_and_search_narrow_the_cards() {
    let mut h = desk(small_project());
    click(&mut h, "TEXTES");
    assert!(h.query_by_label("Entrée CAM 1").is_none());
    assert!(h.query_by_label("Entrée Bandeau titre").is_some());
    click(&mut h, "Tout");
    assert!(h.query_by_label("Entrée CAM 1").is_some());
    h.state_mut().search = "horl".into();
    h.run_steps(3);
    assert!(h.query_by_label("Entrée CAM 1").is_none());
    assert!(h.query_by_label("Entrée Horloge").is_some());
    end(h);
}

#[test]
fn closing_then_undo_restores_the_input_and_its_place_in_scenes() {
    let mut h = desk(small_project());
    let count = project(&h).inputs.len();
    click(&mut h, "Fermer CAM 1");
    assert_eq!(project(&h).inputs.len(), count - 1);
    click(&mut h, "Annuler");
    let p = project(&h);
    assert_eq!(p.inputs.len(), count);
    assert_eq!(p.inputs[0].name, "CAM 1");
    let scene = p.inputs.iter().find(|i| i.name == "2 CAM").unwrap();
    assert!(scene.layers.iter().any(|l| l.input == 1), "the layer comes back too");
    end(h);
}

#[test]
fn the_padlock_protects_inputs() {
    let mut h = desk(small_project());
    click(&mut h, "Cadenas");
    let count = project(&h).inputs.len();
    click(&mut h, "Fermer CAM 1");
    assert_eq!(project(&h).inputs.len(), count);
    click(&mut h, "Cadenas");
    click(&mut h, "Fermer CAM 1");
    assert_eq!(project(&h).inputs.len(), count - 1);
    end(h);
}

#[test]
fn simple_mode_hides_card_buttons_and_returns() {
    let mut h = desk(small_project());
    click(&mut h, "Simple");
    assert!(h.query_by_label("GO CAM 1").is_none());
    click(&mut h, "Avancé");
    assert!(h.query_by_label("GO CAM 1").is_some());
    end(h);
}

#[test]
fn audio_strip_buttons_change_only_sound() {
    let mut h = desk(small_project());
    let version = h.state().with(|s| s.version);
    click(&mut h, "Muet Son de test");
    let tone = |p: &Project| p.inputs.iter().find(|i| i.name == "Son de test").unwrap().audio.clone();
    assert!(!tone(&project(&h)).muted);
    click(&mut h, "Bus A Son de test");
    assert_eq!(tone(&project(&h)).buses & 2, 2);
    click(&mut h, "Suivi Son de test");
    assert!(tone(&project(&h)).follow);
    click(&mut h, "Solo Son de test");
    assert!(h.state().with(|s| !s.solo.is_empty()));
    assert_eq!(h.state().with(|s| s.version), version, "the picture has nothing to redo");
    end(h);
}

#[test]
fn new_project_asks_first_and_can_be_cancelled() {
    let mut h = desk(small_project());
    let count = project(&h).inputs.len();
    click(&mut h, "Nouveau");
    click(&mut h, "Annuler");
    assert_eq!(project(&h).inputs.len(), count);
    click(&mut h, "Nouveau");
    click(&mut h, "Tout vider");
    assert!(project(&h).inputs.is_empty());
    end(h);
}

#[test]
fn every_tab_of_input_settings_opens_for_every_kind_of_input() {
    let mut p = small_project();
    let extra = [
        Kind::Image { path: "x.png".into() },
        Kind::Video { path: "x.mp4".into(), looped: true, mark_in: 0, mark_out: 0, rate: 1.0 },
        Kind::AudioFile { path: "x.mp3".into(), looped: false },
        Kind::List { items: Vec::new(), auto_next: true, auto_first: false, looped: false, shuffle: false },
        Kind::Photos { folder: "x".into(), seconds: 0.0 },
        Kind::ImageSequence { first: "x0001.png".into(), fps: 30.0, looped: true },
        Kind::PowerPoint { path: "x.pptx".into() },
        Kind::Camera { device: "x".into(), ptz_host: String::new() },
        Kind::Network { url: "srt://127.0.0.1:1".into(), buffer_ms: 0 },
        Kind::Browser { url: "about:blank".into(), alpha: false },
        Kind::Microphone { device: "x".into() },
        Kind::Designed { objects: Vec::new(), anim_ms: 500 },
        Kind::Mix { preview: Some(1), output: Some(2), ms: 500 },
        Kind::Telestrator { background: None },
        Kind::Replay,
    ];
    for (n, kind) in extra.into_iter().enumerate() {
        p.inputs.push(Input::new(100 + n as u64, format!("Essai {n}"), kind));
    }
    let ids: Vec<u64> = p.inputs.iter().map(|i| i.id).collect();
    let mut h = desk(p);
    for id in ids {
        h.state_mut().editing = Some(id);
        for tab in 0..10 {
            h.state_mut().edit_tab = tab;
            h.run_steps(2);
        }
    }
    end(h);
}

#[test]
fn every_settings_tab_and_tool_window_opens() {
    let mut h = desk(small_project());
    h.state_mut().settings_open = true;
    for tab in 0..11 {
        h.state_mut().settings_tab = tab;
        h.run_steps(2);
    }
    h.state_mut().settings_open = false;
    let s = h.state_mut();
    s.playlist_open = true;
    s.overlays_open = true;
    s.categories_open = true;
    s.clocks_open = true;
    s.stats_open = true;
    s.help_open = true;
    s.multi_open = true;
    s.replay_open = true;
    s.scope_open = true;
    h.run_steps(4);
    for pick in 0..16 {
        h.state_mut().overlay_pick = pick;
        h.run_steps(2);
    }
    end(h);
}

#[test]
fn a_settings_change_does_not_undo_what_the_desk_just_did() {
    let mut h = desk(small_project());
    click(&mut h, "Entrée CAM 3");
    h.state_mut().settings_open = true;
    h.state_mut().settings_tab = 1;
    h.run_steps(3);
    // A cut landing in the same frame as an edit must survive it.
    let program = project(&h).program;
    click(&mut h, "Coupe");
    assert_ne!(project(&h).program, program);
    let air = project(&h).program;
    h.run_steps(3);
    assert_eq!(project(&h).program, air);
    end(h);
}

#[test]
fn an_empty_desk_shows_its_hint_and_buttons_are_safe() {
    let mut h = desk(Project::blank());
    for button in ["Passer au direct", "Coupe", "Fondu", "Canal 1", "Fondu au noir"] {
        click(&mut h, button);
    }
    wait(&mut h, 200);
    click(&mut h, "Fondu au noir");
    assert!(project(&h).inputs.is_empty());
    end(h);
}

/// Files from when the program was called Regie keep working.
#[test]
fn the_old_name_still_opens_old_files() {
    let dir = std::env::temp_dir().join(format!("livecraft-session-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (current, older) = (dir.join("nouvelle.json"), dir.join("ancienne.json"));
    assert_eq!(session_to_read(current.clone(), older.clone()), current, "nothing saved yet");
    std::fs::write(&older, "{}").unwrap();
    assert_eq!(session_to_read(current.clone(), older.clone()), older, "first start after the rename");
    std::fs::write(&current, "{}").unwrap();
    assert_eq!(session_to_read(current.clone(), older), current, "the new one wins from then on");
    let _ = std::fs::remove_dir_all(&dir);

    let mut h = desk(small_project());
    h.state_mut().add_file(std::path::Path::new("C:/ailleurs/ancienne.regie"));
    assert!(h.state().confirm_open.is_some(), "still recognised as a project");
    end(h);
}

#[test]
fn a_dragged_card_lands_just_before_its_target() {
    let mut h = desk(small_project());
    let order = |h: &Harness<'static, App>| -> Vec<u64> { project(h).inputs.iter().map(|i| i.id).collect() };
    let start = order(&h);
    let (a, c) = (start[0], start[2]);
    h.state_mut().move_input(a, c);
    let moved = order(&h);
    assert_eq!(moved[..2], [start[1], a], "forwards");
    h.state_mut().move_input(c, moved[0]);
    assert_eq!(order(&h)[0], c, "backwards");
    end(h);
}

#[test]
fn a_double_click_opens_settings_only_when_the_click_is_harmless() {
    let mut p = small_project();
    p.inputs.iter_mut().find(|i| i.id == 2).unwrap().click = ClickAction::Overlay(0);
    let mut h = desk(p);
    for _ in 0..2 {
        h.get_by_label("Entrée CAM 2").click();
        h.step();
    }
    assert_eq!(h.state().editing, None, "an action on air stays an action");
    assert_eq!(project(&h).overlays[0], None, "on, then off again");
    h.run_steps(12);
    for _ in 0..2 {
        h.get_by_label("Entrée CAM 1").click();
        h.step();
    }
    assert_eq!(h.state().editing, Some(1));
    end(h);
}

#[test]
fn a_dropped_project_asks_first_and_respects_the_padlock() {
    let mut h = desk(small_project());
    h.state_mut().add_file(std::path::Path::new("C:/ailleurs/emission.livecraft"));
    assert!(h.state().confirm_open.is_some());
    h.run_steps(2);
    click(&mut h, "Annuler");
    assert!(h.state().confirm_open.is_none());
    assert!(!project(&h).inputs.is_empty(), "the desk is still there");
    h.state().with(|s| s.padlock = true);
    h.state_mut().add_file(std::path::Path::new("C:/ailleurs/emission.vmix"));
    assert!(h.state().confirm_open.is_none());
    h.run_steps(2);
    h.get_by_label("Nouveau").click();
    h.run_steps(2);
    assert!(!h.state().confirm_new, "locked");
    end(h);
}

#[test]
fn simple_mode_shows_inputs_whatever_tab_was_chosen() {
    let mut h = desk(small_project());
    h.state_mut().tab = Some(21);
    h.run_steps(3);
    assert!(h.query_by_label("Entrée CAM 1").is_none());
    click(&mut h, "Simple");
    assert!(h.query_by_label("Entrée CAM 1").is_some());
    end(h);
}

#[test]
fn a_moving_setting_restarts_its_source_once_it_settles() {
    let mut h = desk(small_project());
    let first = h.state().engine.rt(1).unwrap();
    for _ in 0..5 {
        h.state_mut().queue_restart(1, true);
        h.run_steps(1);
    }
    assert!(Arc::ptr_eq(&first, &h.state().engine.rt(1).unwrap()), "still moving");
    wait(&mut h, 300);
    assert!(!Arc::ptr_eq(&first, &h.state().engine.rt(1).unwrap()), "settled");
    assert!(h.state().pending_restart.is_none());
    end(h);
}

#[test]
fn loading_a_project_leaves_nothing_of_the_last_one() {
    let mut h = desk(small_project());
    {
        let app = h.state_mut();
        app.editing = Some(2);
        app.tab = Some(1);
        app.multi_ids.push(2);
        app.external_route = Route::Input(2);
        app.engine.load_project(Project::blank());
        app.after_load(None, None);
    }
    h.run_steps(3);
    let app = h.state();
    assert_eq!((app.editing, app.tab), (None, None));
    assert!(app.multi_ids.is_empty());
    assert_eq!(app.external_route, Route::Output);
    end(h);
}

/// Every function by name, on the real engine. What needs an output or writes
/// files is left to the end-to-end run.
#[test]
fn every_named_function_is_known_and_leaves_the_desk_working() {
    use crate::functions::NAMES;
    let h = desk(small_project());
    let engine = h.state().engine.clone();
    let writes = [
        "StartRecording",
        "StartStopRecording",
        "StartMultiCorder",
        "StartStopMultiCorder",
        "StartSRTOutput",
    ];
    for name in NAMES.iter().filter(|n| !writes.contains(n) && **n != "RemoveInput") {
        for value in ["1", "50", "A", ""] {
            let a = Args::new().input("2").value(value);
            if let Err(e) = engine.call(name, &a) {
                assert!(!e.contains("Fonction inconnue"), "{name} : {e}");
            }
        }
    }
    for name in
        ["CUT", "fade", "overlayinput1last", "OVERLAYINPUTALLOFF", "transition1", "stinger1", "quickplay"]
    {
        let a = Args::new().input("2");
        if let Err(e) = engine.call(name, &a) {
            assert!(!e.contains("Fonction inconnue"), "{name} : {e}");
        }
    }
    assert!(engine.call("PasUneFonction", &Args::new()).is_err());
    assert!(engine.call("OverlayInput9", &Args::new().input("2")).is_err());
    engine.call("RemoveInput", &Args::new().input("2")).unwrap();
    assert!(engine.state.lock().unwrap().input(2).is_none());
    end(h);
}

/// A presentation dropped on the desk becomes a PowerPoint input, and the
/// card's buttons walk through its slides without going past either end.
/// Needs PowerPoint or LibreOffice; skipped where neither is installed.
#[test]
fn a_presentation_is_added_and_its_card_buttons_turn_the_slides() {
    if crate::slides::Converter::find().is_none() {
        eprintln!("ni PowerPoint ni LibreOffice : essai sauté");
        return;
    }
    const COLOURS: [[u8; 3]; 3] = [[230, 20, 20], [20, 200, 40], [30, 40, 220]];
    let dir = std::env::temp_dir().join(format!("livecraft-ui-deck-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let deck = dir.join("Conférence.pptx");
    crate::slides::write_test_deck(&deck, &COLOURS);
    let mut p = Project::blank();
    p.out_w = 320;
    p.out_h = 180;
    let mut h = desk(p);
    h.state_mut().add_file(&deck);
    h.run_steps(2);
    let input = project(&h).inputs.last().cloned().unwrap();
    assert!(matches!(&input.kind, Kind::PowerPoint { path } if path.ends_with("Conférence.pptx")));
    let rt = h.state().engine.rt(input.id).unwrap();
    let on_screen = |want: usize| {
        let f = rt.latest()?;
        let at = (f.h / 2 * f.w + f.w / 2) * 4;
        let near = f.data[at..at + 3].iter().zip(COLOURS[want]).all(|(a, b)| a.abs_diff(b) < 12);
        Some(near && rt.item.load(Ordering::Relaxed) as usize == want)
    };
    let until = |h: &mut Harness<'static, App>, what: &str, secs: u64, want: usize| {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while on_screen(want) != Some(true) || rt.items.load(Ordering::Relaxed) != 3 {
            let failed = rt.status().filter(|s| s.contains("illisible") || s.contains("arrêté"));
            assert!(failed.is_none(), "{what} : {failed:?}");
            assert!(Instant::now() < deadline, "{what} : {:?}", rt.status());
            wait(h, 50);
        }
    };
    // A first start of the converter can take a while.
    until(&mut h, "première diapositive", 180, 0);
    assert!(h.query_by_label("1/3").is_some(), "the card counts the slides");
    click(&mut h, "Diapositive suivante Conférence");
    until(&mut h, "deuxième diapositive", 5, 1);
    click(&mut h, "Diapositive suivante Conférence");
    until(&mut h, "troisième diapositive", 5, 2);
    click(&mut h, "Diapositive suivante Conférence");
    wait(&mut h, 200);
    until(&mut h, "toujours la dernière", 1, 2);
    click(&mut h, "Diapositive précédente Conférence");
    until(&mut h, "retour à la deuxième", 5, 1);
    assert!(h.query_by_label("2/3").is_some());
    end(h);
    let _ = std::fs::remove_dir_all(dir);
}
