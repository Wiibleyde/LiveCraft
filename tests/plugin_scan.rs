//! Plug-in files are read by LiveCraft started again with `--scan-clap`, so a
//! plug-in that crashes while being read cannot take the show down. This
//! runs that side for real.

use std::path::{Path, PathBuf};
use std::process::Command;

fn test_plugin() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let target = root.join("target").join("clap-fixture");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .args(["build", "--quiet", "--manifest-path"])
        .arg(root.join("fixtures").join("clap-test-gain").join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&target)
        .status()
        .expect("cargo");
    assert!(status.success(), "the test plug-in did not build");
    let file = if cfg!(windows) {
        "clap_test_gain.dll"
    } else if cfg!(target_os = "macos") {
        "libclap_test_gain.dylib"
    } else {
        "libclap_test_gain.so"
    };
    target.join("debug").join(file)
}

fn scan(file: &Path) -> serde_json::Value {
    let out = Command::new(env!("CARGO_BIN_EXE_livecraft")).arg("--scan-clap").arg(file).output().unwrap();
    assert!(out.status.success());
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn a_plugin_file_is_described_by_a_separate_process() {
    let answer = scan(&test_plugin());
    let found = answer["Ok"].as_array().expect("a list of plug-ins");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["id"], "org.livecraft.test-gain");
    assert_eq!(found[0]["name"], "LiveCraft Test Gain");
    assert!(found[0]["features"].as_array().unwrap().iter().any(|f| f == "audio-effect"));
}

#[test]
fn a_file_that_is_no_plugin_is_reported_not_fatal() {
    let answer = scan(Path::new(env!("CARGO_BIN_EXE_livecraft")));
    assert!(answer["Err"].is_string(), "{answer}");
}
