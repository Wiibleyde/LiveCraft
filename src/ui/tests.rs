//! Interface tests: the real window and engine, driven by real clicks.

use super::*;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use std::thread::sleep;

/// Tests find widgets by their French text, whatever language this computer speaks.
fn saved(project: Project) -> Saved {
    Saved { language: "fr".into(), ..Saved::fresh(project) }
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

fn wait_ms(ms: u64) {
    sleep(Duration::from_millis(ms));
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
fn a_production_clock_can_show_the_show_and_its_buttons_start_pause_and_reset_it() {
    let mut h = desk(small_project());
    h.state_mut().with(|s| s.project.clocks[0].precision = 0);
    h.state_mut().clocks_open = true;
    h.run_steps(3);
    h.get_all_by_label("Émission").next().unwrap().click();
    h.run_steps(3);
    assert_eq!(project(&h).clocks[0].kind, ClockType::Show);
    assert_eq!(project(&h).clocks[0].precision, 1, "minutes would look frozen, so it moves to seconds");
    assert!(!h.state().with(|s| s.clock.running()));
    click(&mut h, "Émission Démarrer");
    assert!(h.state().with(|s| s.clock.running()), "start runs it");
    wait(&mut h, 1100);
    let seen = h.state().with(|s| s.clock.elapsed(Instant::now()));
    assert!(seen >= Duration::from_secs(1), "it counts: {seen:?}");
    click(&mut h, "Émission Pause");
    assert!(!h.state().with(|s| s.clock.running()), "pause stops it");
    click(&mut h, "Émission Démarrer");
    assert!(h.state().with(|s| s.clock.running()), "and it resumes");
    click(&mut h, "Émission Remettre à zéro");
    let (running, spent) = h.state().with(|s| (s.clock.running(), s.clock.elapsed(Instant::now())));
    assert!(!running && spent == Duration::ZERO, "reset stops it at zero");
    end(h);
}

#[test]
fn loading_a_project_does_not_reset_a_running_show_clock() {
    let h = desk(small_project());
    let engine = h.state().engine.clone();
    engine.call("StartShowClock", &Args::new()).unwrap();
    wait_ms(1100);
    let before = engine.state.lock().unwrap().clock.elapsed(Instant::now());
    engine.load_project(Project::blank());
    let s = engine.state.lock().unwrap();
    assert!(s.clock.running(), "still running");
    assert!(s.clock.elapsed(Instant::now()) >= before, "and not back at zero");
    drop(s);
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
        Kind::Dvd { path: "C:/introuvable/disque.iso".into(), title: 1 },
        Kind::PowerPoint { path: "x.pptx".into() },
        Kind::Camera { device: "x".into(), ptz_host: String::new() },
        Kind::Network { url: "srt://127.0.0.1:1".into(), buffer_ms: 0 },
        Kind::Ndi { source: String::new() },
        Kind::Omt { source: "omt://127.0.0.1:1".into() },
        Kind::Browser { url: "about:blank".into(), alpha: false },
        Kind::Microphone { device: "x".into() },
        Kind::Designed { objects: Vec::new(), anim_ms: 500 },
        Kind::Mix { preview: Some(1), output: Some(2), ms: 500 },
        Kind::Telestrator { background: None },
        crate::sets::new_set(SetLook::Evening),
        Kind::Replay,
        crate::call::new_call(),
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

/// An input added from the menu used to have its settings closed again at
/// once, as the window read the project from before the click.
#[test]
fn a_web_page_added_from_the_menu_opens_its_settings() {
    let mut h = desk(small_project());
    click(&mut h, "Ajouter une entrée");
    click(&mut h, "Page web");
    let added = project(&h).inputs.iter().find(|i| matches!(i.kind, Kind::Browser { .. })).map(|i| i.id);
    assert!(added.is_some());
    assert_eq!(h.state().editing, added);
    assert!(h.query_by_label("Adresse de la page").is_some(), "its address can be typed at once");
    end(h);
}

fn calls(h: &Harness<'static, App>) -> Vec<(u64, String)> {
    let p = project(h);
    p.inputs
        .iter()
        .filter_map(|i| match &i.kind {
            Kind::VideoCall { token, .. } => Some((i.id, token.clone())),
            _ => None,
        })
        .collect()
}

/// The link shown is the one the desk answers, a new one shuts the old one
/// out, and two guests never share a link.
#[test]
fn a_video_call_shows_the_link_that_lets_its_guest_in() {
    let mut h = desk(small_project());
    click(&mut h, "Ajouter une entrée");
    click(&mut h, "Appel vidéo");
    let found = calls(&h);
    assert_eq!(found.len(), 1);
    let (id, first) = found[0].clone();
    assert_eq!(h.state().editing, Some(id), "its settings open at once, link in view");
    let link = |token: &str| format!("https://127.0.0.1:{}/call/{token}", crate::api::HTTP_PORT);
    assert!(h.query_by_label(&link(&first)).is_some(), "shown: {:?}", link(&first));
    assert!(h.state().engine.guest_page(&first).is_some(), "the link shown opens the guest's page");

    click(&mut h, "Nouveau lien");
    let renewed = calls(&h)[0].1.clone();
    assert_ne!(renewed, first);
    assert!(h.query_by_label(&link(&first)).is_none(), "the old link is no longer shown");
    assert!(h.query_by_label(&link(&renewed)).is_some());
    assert!(h.state().engine.guest_page(&first).is_none(), "the old link no longer opens anything");
    assert!(h.state().engine.guest_page(&renewed).is_some());

    click(&mut h, "Ajouter une entrée");
    click(&mut h, "Appel vidéo");
    let both = calls(&h);
    assert_eq!(both.len(), 2);
    assert_ne!(both[0].1, both[1].1, "each guest has a link of their own");
    end(h);
}

/// A copy of a call is a call of its own: one link for both would let the
/// copy's holder into the original's call.
#[test]
fn a_duplicated_call_gets_a_link_of_its_own() {
    let mut h = desk(small_project());
    click(&mut h, "Ajouter une entrée");
    click(&mut h, "Appel vidéo");
    // What the card's "Dupliquer" does; the card itself may be scrolled away.
    let call = project(&h).inputs.iter().find(|i| matches!(i.kind, Kind::VideoCall { .. })).cloned().unwrap();
    h.state_mut().duplicate_input(&call);
    h.run_steps(3);
    let found = calls(&h);
    assert_eq!(found.len(), 2, "{found:?}");
    assert_ne!(found[0].1, found[1].1, "the copy has a link of its own");
    for (_, token) in &found {
        assert!(h.state().engine.guest_page(token).is_some(), "each link opens its own call");
    }
    end(h);
}

/// In English, a video call's settings say nothing in French, and the way to
/// the network setting is named as that window names it.
#[test]
fn a_video_call_is_set_up_in_english_too() {
    use crate::i18n::{Lang, set_language};
    // Before the desk, so the demo inputs are named in English; after it, as
    // the window starts in the language saved with it.
    set_language(Lang::En);
    let mut h = desk(small_project());
    set_language(Lang::En);
    h.run_steps(2);
    click(&mut h, "Add input");
    click(&mut h, "Video call");
    for shown in
        ["Link to send the guest", "Copy link", "New link", "Waiting for the guest", "Address in the link"]
    {
        assert!(h.query_by_label(shown).is_some(), "{shown}");
    }
    let way = "Only this computer can open this link. For a guest elsewhere, tick “Reachable from the local network” \
               in Settings, Web control, then restart.";
    assert!(h.query_by_label(way).is_some(), "{way}");
    assert_eq!(french_left(&h), Vec::<String>::new(), "French left on screen");
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

/// The settings say what composes the picture, and unticking the card really
/// moves the work to the processor without the output stopping.
#[test]
fn the_settings_show_and_switch_what_composes_the_picture() {
    let mut h = desk(small_project());
    h.state_mut().settings_open = true;
    h.state_mut().settings_tab = 0;
    let engine = h.state().engine.clone();
    let deadline = Instant::now() + Duration::from_secs(10);
    while *engine.compositor.lock().unwrap() == Compositor::Starting {
        assert!(Instant::now() < deadline, "the mixer never chose");
        wait(&mut h, 20);
    }
    h.run_steps(2);
    assert!(
        h.query_by_label_contains("Sur la carte graphique").is_some()
            || h.query_by_label_contains("Sur le processeur").is_some()
    );

    click(&mut h, "Composer sur la carte graphique");
    assert!(!engine.use_gpu.load(Ordering::Relaxed));
    let deadline = Instant::now() + Duration::from_secs(5);
    while *engine.compositor.lock().unwrap() != Compositor::Cpu("choisi dans les réglages".into()) {
        assert!(Instant::now() < deadline, "still {:?}", engine.compositor.lock().unwrap());
        wait(&mut h, 20);
    }
    h.run_steps(2);
    assert!(h.query_by_label_contains("Sur le processeur : choisi dans les réglages").is_some());
    let seq = engine.program_seq.load(Ordering::Acquire);
    click(&mut h, "Coupe");
    let deadline = Instant::now() + Duration::from_secs(5);
    while engine.program_seq.load(Ordering::Acquire) == seq {
        assert!(Instant::now() < deadline, "no output after the switch");
        wait(&mut h, 20);
    }

    click(&mut h, "Composer sur la carte graphique");
    let deadline = Instant::now() + Duration::from_secs(5);
    while *engine.compositor.lock().unwrap() == Compositor::Cpu("choisi dans les réglages".into()) {
        assert!(Instant::now() < deadline, "ticking it again changes nothing");
        wait(&mut h, 20);
    }
    // The card again, or why not.
    let deadline = Instant::now() + Duration::from_secs(10);
    while *engine.compositor.lock().unwrap() == Compositor::Starting {
        assert!(Instant::now() < deadline, "the card never opened again");
        wait(&mut h, 20);
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

#[test]
fn an_ndi_or_omt_input_comes_from_the_menu_and_asks_for_its_source() {
    let mut h = desk(small_project());
    let before = project(&h).inputs.len();
    click(&mut h, "Ajouter une entrée");
    click(&mut h, "OMT");
    click(&mut h, "Autre source OMT…");
    let p = project(&h);
    assert_eq!(p.inputs.len(), before + 1);
    let added = p.inputs.last().unwrap().clone();
    assert_eq!(added.kind, Kind::Omt { source: String::new() });
    assert_eq!(h.state().editing, Some(added.id), "ses réglages s'ouvrent pour choisir la source");
    h.get_by_label("Nom exact");
    let rt = h.state().engine.rt(added.id).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !rt.status().is_some_and(|s| s.contains("Choisissez une source OMT")) {
        assert!(Instant::now() < deadline, "la carte doit dire quoi faire : {:?}", rt.status());
        wait(&mut h, 20);
    }
    end(h);
}

#[test]
fn network_outputs_start_and_stop_from_the_settings() {
    let _alone = NET_NAMES.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = desk(small_project());
    h.state_mut().settings_open = true;
    h.state_mut().settings_tab = 2;
    h.run_steps(3);
    let engine = h.state().engine.clone();
    click(&mut h, "Démarrer NDI / OMT");
    assert!(!engine.is_net());
    let said =
        |h: &Harness<'static, App>| h.state().message.as_ref().map(|(m, _)| m.clone()).unwrap_or_default();
    assert!(said(&h).contains("Cochez"), "rien de coché doit le dire : {}", said(&h));
    click(&mut h, "NDI");
    assert!(project(&h).ndi_out.enabled, "la case cochée doit rester dans le projet");
    click(&mut h, "Démarrer NDI / OMT");
    match crate::netvideo::Net::Ndi.available() {
        Ok(()) => {
            assert!(engine.is_net(), "{}", said(&h));
            assert_eq!(engine.net.lock().unwrap()[0].name, "LiveCraft");
            click(&mut h, "Arrêter NDI / OMT");
            assert!(!engine.is_net());
        }
        // Without the runtime, nothing starts and the reason is on screen.
        Err(missing) => {
            assert!(!engine.is_net());
            assert_eq!(said(&h), missing);
        }
    }
    end(h);
}

/// Both tests offer the same source names, and a name is taken for the whole
/// process: they run one after the other.
static NET_NAMES: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Stop then Start at once: the sender just stopped is still being torn down,
/// and the new one must not fail on its name ("nom déjà pris").
#[test]
fn a_network_output_starts_again_right_after_it_stops() {
    let _alone = NET_NAMES.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = desk(small_project());
    h.state_mut().settings_open = true;
    h.state_mut().settings_tab = 2;
    h.run_steps(3);
    let engine = h.state().engine.clone();
    let (ndi, omt) =
        (crate::netvideo::Net::Ndi.available().is_ok(), crate::netvideo::Net::Omt.available().is_ok());
    if !ndi && !omt {
        end(h);
        return;
    }
    if ndi {
        click(&mut h, "NDI");
    }
    if omt {
        click(&mut h, "OMT");
    }
    for round in 0..4 {
        click(&mut h, "Démarrer NDI / OMT");
        assert!(engine.is_net(), "tour {round} : le démarrage doit réussir juste après l'arrêt");
        click(&mut h, "Arrêter NDI / OMT");
        assert!(!engine.is_net());
    }
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

/// The reported bug: running the tests replaced the person's desk with a
/// test project, as a test desk saved itself where a real run does.
#[test]
fn a_test_desk_never_writes_the_persons_session() {
    let mut h = desk(small_project());
    assert!(h.state().session_file.is_none(), "a test desk keeps no session");
    let file = std::env::temp_dir().join(format!("livecraft-session-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&file);
    // Given a place, the same desk saves there once its autosave is due.
    h.state_mut().session_file = Some(file.clone());
    h.state_mut().last_autosave = Instant::now().checked_sub(Duration::from_secs(31)).unwrap();
    h.run_steps(2);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !file.exists() {
        assert!(Instant::now() < deadline, "the autosave writes where it is told");
        sleep(Duration::from_millis(20));
    }
    end(h);
    let _ = std::fs::remove_file(&file);
}

/// What a double-clicked .livecraft file does: the installer's file type
/// starts LiveCraft with `--project <file>`.
/// Opening a project by name replaces the desk, which the session file held:
/// one copy is kept, and saving a project never leaves it half written.
#[test]
fn opening_a_project_keeps_the_session_and_saving_swaps_atomically() {
    let dir = std::env::temp_dir().join(format!("livecraft-backup-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let session = dir.join("session.json");
    assert!(!super::back_up_session(&session), "nothing to keep");
    std::fs::write(&session, b"first").unwrap();
    assert!(super::back_up_session(&session));
    std::fs::write(&session, b"second").unwrap();
    assert!(super::back_up_session(&session));
    assert_eq!(std::fs::read(dir.join("session.prev.json")).unwrap(), b"second", "the last one is kept");

    let file = dir.join("show.livecraft");
    std::fs::write(&file, b"old").unwrap();
    super::bars::write_beside_then_swap(&file, b"new").unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), b"new");
    assert!(!dir.join("show.livecraft.tmp").exists(), "no leftover");
    let missing = dir.join("no").join("show.livecraft");
    assert!(super::bars::write_beside_then_swap(&missing, b"x").is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_project_opened_from_explorer_saves_back_to_its_own_file() {
    let dir = std::env::temp_dir().join(format!("livecraft-start-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("emission.livecraft");
    let mut on_disk = small_project();
    on_disk.inputs.iter_mut().find(|i| i.id == 1).unwrap().name = "Caméra du public".into();
    std::fs::write(&file, serde_json::to_vec(&on_disk).unwrap()).unwrap();
    let start = |path: std::path::PathBuf| {
        let path = path.to_string_lossy().into_owned();
        let mut h = Harness::builder()
            .with_size(vec2(2200.0, 1100.0))
            .with_accessibility_check(false)
            .build_eframe(move |cc| App::start(cc, saved(Project::blank()), None, "", Some(path), false));
        h.run_steps(4);
        h
    };

    let mut h = start(file.clone());
    assert!(project(&h).inputs.iter().any(|i| i.name == "Caméra du public"), "the file, not the session");
    h.get_by_label("emission");
    click(&mut h, "Entrée CAM 2");
    // The output bar's record button has the same name; this is the top bar's.
    h.get_all_by_label("Enregistrer")
        .min_by(|a, b| a.rect().top().total_cmp(&b.rect().top()))
        .unwrap()
        .click();
    h.run_steps(3);
    assert!(!h.state().engine.is_recording());
    let back: Project = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(back.preview, Some(2), "saved into the file it came from, without asking where");
    end(h);

    let h = start(dir.join("absente.livecraft"));
    assert!(project(&h).inputs.is_empty(), "the session's desk stays");
    let said = h.state().message.as_ref().map(|(m, _)| m.clone()).unwrap_or_default();
    assert!(said.starts_with("Projet illisible"), "{said}");
    end(h);
    let _ = std::fs::remove_dir_all(&dir);
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

// Hardware decoding exists on Windows only.
#[cfg(windows)]
#[test]
fn turning_off_the_card_moves_running_streams_to_software_only() {
    // A listening stream waits for a sender, so its decoder stays as started.
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let url = format!("tcp://127.0.0.1:{port}?listen=1");
    let mut p = small_project();
    p.inputs.push(Input::new(40, "Flux", Kind::Network { url, buffer_ms: 0 }));
    let mut h = desk(p);
    let engine = h.state().engine.clone();
    let on_card = |e: &Engine| e.rt(40).unwrap().hardware.load(Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !on_card(&engine) {
        assert!(Instant::now() < deadline, "a stream starts on the card");
        wait(&mut h, 20);
    }
    let clock = engine.rt(14).unwrap();
    h.state_mut().settings_open = true;
    h.state_mut().settings_tab = 1;
    h.run_steps(3);

    click(&mut h, "Décodage par la carte graphique");
    assert!(!project(&h).hardware_decoding, "saved with the project");
    wait(&mut h, 300);
    assert!(!on_card(&engine), "the running stream must restart in software");
    assert!(Arc::ptr_eq(&clock, &engine.rt(14).unwrap()), "a clock has nothing to decode and must not blink");

    click(&mut h, "Décodage par la carte graphique");
    assert!(project(&h).hardware_decoding);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !on_card(&engine) {
        assert!(Instant::now() < deadline, "back on the card");
        wait(&mut h, 20);
    }
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

/// A scene added from the menu used to flash its settings and close them at
/// once, as that frame's copy of the project did not have it yet.
#[test]
fn an_input_added_from_the_menu_keeps_its_settings_open() {
    let mut h = desk(small_project());
    click(&mut h, "Ajouter une entrée");
    click(&mut h, "Scène (couches)");
    let added = project(&h).inputs.last().map(|i| i.id);
    assert!(added.is_some_and(|id| id > 16), "a scene was added");
    h.run_steps(3);
    assert_eq!(h.state().editing, added);
    end(h);
}

/// Made from the menu, pointed at a camera and given its time in the settings
/// that open with it: the delay then shows that camera. Its list of sources
/// never offers the delay itself.
#[test]
fn a_video_delay_is_added_from_the_menu_and_shows_its_source() {
    let mut h = desk(small_project());
    click(&mut h, "Ajouter une entrée");
    click(&mut h, "Retard vidéo");
    let added = project(&h).inputs.iter().find(|i| matches!(i.kind, Kind::VideoDelay { .. })).map(|i| i.id);
    let delay = added.expect("the menu adds a video delay");
    h.run_steps(3);
    assert_eq!(h.state().editing, Some(delay), "its settings open at once");
    let named_delay = |h: &Harness<'static, App>| h.query_all_by_label("Retard vidéo").count();
    let before = named_delay(&h);
    click(&mut h, "Source du retard");
    assert_eq!(named_delay(&h), before, "not offered as its own source");
    click(&mut h, "CAM 2");
    h.get_by_role_and_label(egui::accesskit::Role::Slider, "Retard").set_value_accesskit(250.0);
    h.run_steps(3);
    let kind = project(&h).inputs.iter().find(|i| i.id == delay).unwrap().kind.clone();
    assert_eq!(kind, Kind::VideoDelay { source: Some(2), ms: 250 });
    let (engine, camera) = (h.state().engine.clone(), h.state().engine.rt(2).unwrap());
    settle(&mut h, "the delay shows the moving camera", |_| {
        let shown = engine.rt(delay).and_then(|r| r.latest());
        shown.is_some_and(|f| !Arc::ptr_eq(&f, &camera.latest().unwrap()))
            && engine.rt(delay).is_some_and(|r| r.seq() > 10)
    });
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

fn preview_red_at(h: &Harness<'static, App>, x: f32, y: f32) -> Option<bool> {
    let f = h.state().engine.preview.lock().unwrap().clone()?;
    let i = (((y * f.h as f32) as usize) * f.w + (x * f.w as f32) as usize) * 4;
    Some(f.data[i] > 150 && f.data[i + 1] < 80 && f.data[i + 2] < 80)
}

/// Made from the menu, given a presenter from its settings, then sent from
/// the wide shot to the close-up with the card's button: the camera must
/// move there over time, not cut, and the preview must show it.
#[test]
fn a_virtual_set_is_made_from_the_menu_and_its_camera_glides_between_presets() {
    let mut p = Project::blank();
    p.out_w = 320;
    p.out_h = 180;
    p.inputs = vec![Input::new(1, "Présentatrice", Kind::Solid { color: [200, 30, 30] })];
    let mut h = desk(p);
    click(&mut h, "Ajouter une entrée");
    click(&mut h, "Plateau virtuel");
    let set = project(&h).inputs.iter().find(|i| matches!(i.kind, Kind::VirtualSet { .. })).map(|i| i.id);
    let set = set.expect("the menu adds a virtual set");
    assert_eq!(h.state().editing, Some(set), "its settings open at once");
    click(&mut h, "Ajouter un présentateur");
    let layers = project(&h).inputs.iter().find(|i| i.id == set).unwrap().layers.clone();
    assert_eq!(layers.iter().map(|l| l.input).collect::<Vec<_>>(), vec![1]);
    h.state_mut().editing = None;
    h.run_steps(2);

    click(&mut h, "Entrée Plateau virtuel");
    let deadline = Instant::now() + Duration::from_secs(5);
    // Wide shot: the presenter stands in the middle, the decor beside her.
    while preview_red_at(&h, 0.5, 0.5) != Some(true) {
        assert!(Instant::now() < deadline, "the set never reached the preview");
        wait(&mut h, 20);
    }
    assert_eq!(preview_red_at(&h, 0.1, 0.5), Some(false));

    click(&mut h, "Caméra 3 Plateau virtuel");
    let moving = h.state().with(|s| s.set_moves.contains_key(&set));
    assert!(moving, "the camera moves rather than cuts");
    let kind = project(&h).inputs.iter().find(|i| i.id == set).unwrap().kind.clone();
    assert!(matches!(kind, Kind::VirtualSet { camera: 2, .. }));
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.state().with(|s| s.set_moves.contains_key(&set)) || preview_red_at(&h, 0.1, 0.5) != Some(true) {
        assert!(Instant::now() < deadline, "the close-up never arrived");
        wait(&mut h, 20);
    }
    end(h);
}

/// Generous: other tests and builds share the machine, and a DVD title takes a moment to open.
fn until(h: &mut Harness<'static, App>, what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "attendu sans succès : {what}");
        wait(h, 30);
    }
}

/// Dropping a disc's VIDEO_TS folder makes a DVD input, whose card and
/// settings step through chapters and switch titles.
#[test]
fn a_dropped_dvd_folder_plays_and_steps_through_chapters() {
    use crate::playtest::{BLUE, GREEN, RED, centre, near};
    let dir = std::env::temp_dir().join(format!("livecraft-dvd-ui-{}", std::process::id()));
    let Some(disc) = crate::playtest::dvd_fixture(&dir.to_string_lossy()).unwrap() else {
        eprintln!("ignoré : dvdauthor ou le démultiplexeur dvdvideo manque");
        return;
    };
    // An empty desk, so the new card is on screen to be clicked.
    let mut p = Project::blank();
    p.out_w = 320;
    p.out_h = 180;
    let mut h = desk(p);
    h.state_mut().add_file(&std::path::Path::new(&disc).join("VIDEO_TS"));
    h.run_steps(3);
    let input = project(&h).inputs.last().cloned().unwrap();
    assert_eq!(input.kind, Kind::Dvd { path: disc.clone(), title: 1 }, "the disc, not the VIDEO_TS folder");
    assert_eq!(input.name, "disque");
    let rt = h.state().engine.rt(input.id).unwrap();
    until(&mut h, "trois chapitres en rouge", || {
        rt.items.load(Ordering::Relaxed) == 3 && near(centre(&rt), RED)
    });
    let on = |rt: &crate::sources::InputRt, n: u64, colour| {
        rt.item.load(Ordering::Relaxed) == n && near(centre(rt), colour)
    };
    click(&mut h, "Chapitre suivant disque");
    until(&mut h, "chapitre 2 en vert", || on(&rt, 1, GREEN));
    click(&mut h, "Chapitre précédent disque");
    until(&mut h, "retour au chapitre 1", || on(&rt, 0, RED));
    h.state_mut().editing = Some(input.id);
    h.state_mut().edit_tab = 0;
    h.run_steps(3);
    click(&mut h, "Chapitre 3");
    until(&mut h, "chapitre 3 en bleu", || on(&rt, 2, BLUE));
    click(&mut h, "Titre du DVD");
    click(&mut h, "Titre 2 : 1 chapitre(s), 00:02");
    let engine = h.state().engine.clone();
    until(&mut h, "le titre 2 en blanc", || {
        engine.rt(input.id).is_some_and(|rt| near(centre(&rt), [255, 255, 255]))
    });
    assert_eq!(project(&h).inputs.last().unwrap().kind, Kind::Dvd { path: disc, title: 2 });
    end(h);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Text on screen that only the French catalog holds: after a switch to English, any of it
/// still showing was kept from before the switch.
fn french_left(h: &Harness<'static, App>) -> Vec<String> {
    use crate::i18n::{Lang, entries};
    let english: std::collections::HashSet<&str> = entries(Lang::En).map(|(_, text)| text).collect();
    let english_of: HashMap<&str, &str> = entries(Lang::En).collect();
    let french_only: std::collections::HashSet<&str> = entries(Lang::Fr)
        // Language names stay in their own language on purpose.
        .filter(|(key, _)| !key.starts_with("language."))
        .filter(|(key, text)| english_of.get(key) != Some(text) && !english.contains(text))
        .map(|(_, text)| text)
        .collect();
    let found = std::cell::RefCell::new(Vec::new());
    let _ = h
        .query_all_by(|node| {
            let shown = if node.role() == egui::accesskit::Role::Label { node.value() } else { node.label() };
            if let Some(text) = shown.filter(|t| french_only.contains(t.as_str())) {
                found.borrow_mut().push(text);
            }
            false
        })
        .count();
    found.into_inner()
}

/// The language is picked in the settings and changes the whole window at once; the window
/// that was open stays open on its tab, and the choice is what gets saved.
#[test]
fn choosing_english_in_the_settings_changes_the_window_at_once() {
    // Demo names follow the language they were created in; English ones keep the search
    // below about what the window itself says.
    crate::i18n::set_language(crate::i18n::Lang::En);
    let mut h = desk(small_project());
    click(&mut h, "Réglages");
    click(&mut h, "Options");
    assert!(h.query_by_label("Coupe").is_some());
    click(&mut h, "English");
    assert!(h.state().settings_open, "the settings window is still open");
    assert_eq!(h.state().settings_tab, 1, "on the same tab");
    assert_eq!(h.state().saved.language, "en");
    assert_eq!(french_left(&h), Vec::<String>::new(), "French left on screen");
    assert!(h.query_by_label("Coupe").is_none());
    click(&mut h, "Input CAM 3");
    click(&mut h, "Cut");
    assert_eq!(project(&h).program, Some(3), "the English buttons work");
    click(&mut h, "Français");
    assert_eq!(h.state().saved.language, "fr");
    assert!(h.query_by_label("Coupe").is_some(), "and back");
    end(h);
}

/// Sessions saved before the choice existed open in the system's language.
#[test]
fn an_older_session_without_a_language_still_opens() {
    let saved: Saved = serde_json::from_str(r#"{"record_dir":"D:/videos"}"#).unwrap();
    assert_eq!(saved.record_dir, "D:/videos");
    assert_eq!(saved.language, "");
    let again: Saved =
        serde_json::from_str(&serde_json::to_string(&Saved { language: "en".into(), ..saved }).unwrap())
            .unwrap();
    assert_eq!(again.language, "en");
}

/// For what sits low in a scrolled window: a click lands where it is shown.
fn click_shown(h: &mut Harness<'static, App>, label: &str) {
    h.get_by_label(label).scroll_to_me();
    h.run_steps(3);
    click(h, label);
}

fn settle(h: &mut Harness<'static, App>, what: &str, ok: impl Fn(&Harness<'static, App>) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ok(h) {
        assert!(Instant::now() < deadline, "{what}");
        wait(h, 40);
    }
}

/// A plug-in picked from the list joins the input's chain and really treats
/// its sound: turning its gain down turns the meter down by as much, and
/// bypassing or removing it gives the sound back.
#[test]
fn a_plugin_added_in_the_audio_settings_treats_the_sound() {
    use crate::plugins::{Owner, tests::test_plugin};
    const TONE: u64 = 16;
    let mut h = desk(small_project());
    h.state().engine.plugins.set_catalog(vec![test_plugin()]);
    let peak = |h: &Harness<'static, App>| h.state().engine.rt(TONE).unwrap().peak();
    let chain = |h: &Harness<'static, App>| {
        project(h).inputs.iter().find(|i| i.id == TONE).unwrap().audio.plugins.clone()
    };
    settle(&mut h, "the test tone plays", |h| peak(h) > 0.01);
    let untreated = peak(&h);

    h.state_mut().editing = Some(TONE);
    h.state_mut().edit_tab = 8;
    h.run_steps(3);
    click_shown(&mut h, "Ajouter un plug-in");
    click(&mut h, "LiveCraft Test Gain");
    let slots = chain(&h);
    assert_eq!(slots.len(), 1);
    assert_eq!(slots[0].id, "org.livecraft.test-gain");
    let key = slots[0].key;
    let view = move |h: &Harness<'static, App>| h.state().engine.plugins.view(Owner::Input(TONE), key);
    settle(&mut h, "the plug-in starts", |h| view(h).is_some_and(|v| v.running));
    h.run_steps(3);
    h.get_by_label("Latence : 32 échantillons (0.7 ms)");

    // As a screen reader or a typed value would set it.
    h.get_by_label("Gain").set_value_accesskit(-20.0);
    h.run_steps(3);
    let gain_db = |h: &Harness<'static, App>| view(h).unwrap().params[0].value as f32;
    settle(&mut h, "the plug-in takes the new gain", |h| (gain_db(h) + 20.0).abs() < 0.01);
    let expected = untreated * 0.1;
    settle(&mut h, "the meter follows the plug-in's gain", |h| (peak(h) - expected).abs() < expected * 0.1);
    settle(&mut h, "the state reaches the project", |h| !chain(h)[0].state.is_empty());

    click_shown(&mut h, "Actif LiveCraft Test Gain");
    assert!(chain(&h)[0].bypass);
    settle(&mut h, "bypass gives the sound back", |h| (peak(h) - untreated).abs() < untreated * 0.05);

    click_shown(&mut h, "Retirer LiveCraft Test Gain");
    assert!(chain(&h).is_empty());
    settle(&mut h, "the plug-in is let go", |h| view(h).is_none());
    end(h);
}
