#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// First and apart, so that every module below can use `t!`.
#[macro_use]
mod i18n;

mod api;
mod audio;
mod browser;
mod desk;
mod dsp;
mod engine;
mod extras;
mod frame;
mod functions;
mod fx;
mod mixer;
mod model;
#[cfg(test)]
mod model_tests;
mod output;
mod outputs;
mod playback;
mod playtest;
mod replay;
mod secret;
mod selftest;
mod soak;
mod sources;
mod text;
mod titles;
mod transitions;
mod ui;
mod vmix_import;
mod widgets;

use eframe::egui;

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().collect();
    let value_of = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    if let Some(dir) = value_of("--selftest") {
        std::process::exit(selftest::run(&dir));
    }
    if let Some(dir) = value_of("--soak") {
        // The length is optional and follows the folder.
        let at = args.iter().position(|a| a == "--soak").map_or(0, |i| i + 2);
        let length = args.get(at).filter(|a| !a.starts_with("--"));
        std::process::exit(soak::run(&dir, length.map(String::as_str)));
    }
    // The command line speaks the system's language unless told otherwise; the window
    // then follows its saved choice, which `--lang` also changes.
    let lang = value_of("--lang").as_deref().and_then(i18n::Lang::from_code);
    i18n::set_language(lang.unwrap_or_else(i18n::Lang::system));
    if let Some(file) = value_of("--import-report") {
        std::process::exit(import_report(&file));
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
