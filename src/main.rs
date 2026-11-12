#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// First and apart, so that every module below can use `t!`.
#[macro_use]
mod i18n;

mod api;
mod audio;
mod browser;
mod clap;
mod desk;
mod dsp;
mod engine;
mod extras;
mod frame;
mod functions;
mod fx;
mod livecheck;
mod mixer;
mod model;
#[cfg(test)]
mod model_tests;
mod output;
mod outputs;
mod playback;
mod playtest;
mod plugins;
mod replay;
mod secret;
mod selftest;
mod sets;
mod slides;
mod sources;
mod text;
mod titles;
mod transitions;
mod ui;
mod vmix_import;
mod widgets;

use eframe::egui;

fn main() -> eframe::Result {
    #[cfg(all(target_arch = "x86_64", not(target_feature = "avx2")))]
    if let Some(code) = hand_over_to_avx2_build() {
        std::process::exit(code);
    }
    announce_running();
    let args: Vec<String> = std::env::args().collect();
    let value_of = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    if let Some(dir) = value_of("--selftest") {
        std::process::exit(selftest::run(&dir));
    }
    if let Some(dir) = value_of("--livecheck") {
        std::process::exit(livecheck::run(&dir, value_of("--break").as_deref()));
    }
    // The command line speaks the system's language unless told otherwise; the window
    // then follows its saved choice, which `--lang` also changes.
    let lang = value_of("--lang").as_deref().and_then(i18n::Lang::from_code);
    i18n::set_language(lang.unwrap_or_else(i18n::Lang::system));
    if let Some(file) = value_of("--import-report") {
        std::process::exit(import_report(&file));
    }
    // Run by the program itself to look inside a plug-in file safely apart.
    if let Some(file) = value_of("--scan-clap") {
        std::process::exit(plugins::scan_one_and_print(&file));
    }
    // One thread per physical core: the second half would only fight over the
    // same units, and the window, ffmpeg and the browser need their share.
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let _ = rayon::ThreadPoolBuilder::new().num_threads((cores / 2).max(2)).build_global();
    let screenshot = value_of("--screenshot");
    let project = value_of("--project");
    let open = value_of("--open");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("LiveCraft")
            .with_inner_size([1600.0, 900.0])
            .with_min_inner_size([1100.0, 640.0])
            .with_maximized(true),
        ..Default::default()
    };
    eframe::run_native(
        "LiveCraft",
        options,
        Box::new(move |cc| Ok(Box::new(ui::App::new(cc, screenshot, open, project, lang)))),
    )
}

/// Releases ship two builds of the same program, as wider instructions make
/// the pixel loops much faster (numbers in docs/releasing.md) but crash
/// processors without them. This one runs anywhere; next to it,
/// `livecraft-avx2.exe` is built for x86-64-v3 and takes over when the
/// processor has every feature that level assumes.
#[cfg(all(target_arch = "x86_64", not(target_feature = "avx2")))]
fn hand_over_to_avx2_build() -> Option<i32> {
    use std::arch::is_x86_feature_detected as has;
    let able = has!("avx")
        && has!("avx2")
        && has!("bmi1")
        && has!("bmi2")
        && has!("f16c")
        && has!("fma")
        && has!("lzcnt")
        && has!("movbe")
        && has!("popcnt")
        && has!("sse4.1")
        && has!("sse4.2")
        && has!("ssse3")
        && has!("xsave");
    let faster = std::env::current_exe().ok()?.with_file_name("livecraft-avx2.exe");
    if !able || !faster.exists() {
        return None;
    }
    // The children of this process end with it, so closing or killing this
    // wrapper (the one the shortcut and Task Manager show) ends the real program.
    sources::tie_children_to_this_process();
    let started = std::time::Instant::now();
    // If it cannot start, or dies at once, this build still can.
    let status = std::process::Command::new(faster).args(std::env::args_os().skip(1)).status().ok()?;
    let code = status.code().unwrap_or(1);
    if crashed_at_start(code, started.elapsed()) {
        return None;
    }
    Some(code)
}

/// Windows reports a program that cannot run (illegal instruction 0xC000001D,
/// access violation, missing library, failed start-up...) with a status that
/// has both top bits set. Within this long, it is the processor or the install,
/// not something the person did.
#[cfg(any(test, all(target_arch = "x86_64", not(target_feature = "avx2"))))]
fn crashed_at_start(code: i32, after: std::time::Duration) -> bool {
    after < std::time::Duration::from_secs(2) && (code as u32) & 0xC000_0000 == 0xC000_0000
}

/// Lets the installer see that LiveCraft is running (`AppMutex`), so that an
/// upgrade or an uninstall asks to close it. It does not stop a second copy.
fn announce_running() {
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let name: Vec<u16> = "LiveCraft-running\0".encode_utf16().collect();
    // SAFETY: the name is NUL-terminated; the handle stays open until the process ends, on purpose.
    unsafe {
        CreateMutexW(std::ptr::null(), 0, name.as_ptr());
    }
}

/// Prints what opening a .vmix keeps and leaves, without starting anything.
fn import_report(file: &str) -> i32 {
    match vmix_import::load(file) {
        Ok((p, r)) => {
            let shown = p.inputs.iter().filter(|i| i.kind.has_picture()).count();
            println!(
                "{}",
                t!(
                    "main.report_summary",
                    name = r.name,
                    inputs = r.inputs,
                    shown = shown,
                    layers = r.layers,
                    overlays = r.overlays,
                    w = p.out_w,
                    h = p.out_h,
                    fps = p.fps
                )
            );
            println!("  {}", t!("main.report_unknown", list = format!("{:?}", r.placeholders)));
            println!("  {}", t!("main.report_missing", count = r.missing_files.len()));
            for n in &r.notes {
                println!("  {}", t!("main.report_note", note = n));
            }
            0
        }
        Err(e) => {
            println!("{}", t!("main.report_failed", error = e));
            1
        }
    }
}

#[cfg(test)]
mod hand_over_tests {
    use super::crashed_at_start;
    use std::time::Duration;

    #[test]
    fn a_build_that_dies_at_once_with_a_crash_status_hands_back() {
        let quick = Duration::from_millis(300);
        assert!(crashed_at_start(0xC000_001Du32 as i32, quick), "illegal instruction");
        assert!(crashed_at_start(0xC000_0005u32 as i32, quick), "access violation");
        assert!(crashed_at_start(0xC000_0142u32 as i32, quick), "library failed to start");
        assert!(!crashed_at_start(0, quick), "a normal exit is passed on");
        assert!(!crashed_at_start(1, quick), "an ordinary error is passed on");
        assert!(
            !crashed_at_start(0xC000_0005u32 as i32, Duration::from_secs(30)),
            "a crash after a long run is not a start-up failure"
        );
    }
}
