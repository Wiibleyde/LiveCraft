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
mod replay;
mod sources;
mod text;
mod titles;
mod transitions;
mod vmix_import;

fn main() {}
