#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

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
    let args: Vec<String> = std::env::args().collect();
    let value_of = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    if let Some(dir) = value_of("--selftest") {
        std::process::exit(selftest::run(&dir));
    }
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
        Box::new(move |cc| Ok(Box::new(ui::App::new(cc, screenshot, open, project)))),
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
    // If it cannot start, this build still can.
    let status = std::process::Command::new(faster).args(std::env::args_os().skip(1)).status().ok()?;
    Some(status.code().unwrap_or(1))
}

/// Prints what opening a .vmix keeps and leaves, without starting anything.
fn import_report(file: &str) -> i32 {
    match vmix_import::load(file) {
        Ok((p, r)) => {
            let shown = p.inputs.iter().filter(|i| i.kind.has_picture()).count();
            println!(
                "{} : {} entrées ({shown} avec image), {} couches, {} incrustations, {}x{} à {} i/s",
                r.name, r.inputs, r.layers, r.overlays, p.out_w, p.out_h, p.fps
            );
            println!("  inconnus : {:?}", r.placeholders);
            println!("  fichiers absents : {}", r.missing_files.len());
            for n in &r.notes {
                println!("  note : {n}");
            }
            0
        }
        Err(e) => {
            println!("ECHEC : {e}");
            1
        }
    }
}
