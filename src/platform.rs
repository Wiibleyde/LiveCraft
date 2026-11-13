//! Everything that differs between Windows, Linux and macOS: how children are
//! started and ended, which ffmpeg device opens a camera or the screen, where
//! the browser and the user's folders are, how secrets are sealed, and how a
//! later start reaches the one running. The rest of the program asks here and
//! stays the same everywhere.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as os;

#[cfg(unix)]
mod unix;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as os;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as os;

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
compile_error!("LiveCraft runs on Windows, Linux and macOS");

pub use os::{
    camera_input, config_dir, find_browser, font_dirs, list_devices, list_screens, microphone_input,
    pictures_dir, screen_input, system_audio_input, videos_dir,
};

#[cfg(windows)]
pub use windows::{
    LocalListener, LocalStream, ProcessTree, connect_local, end_process_tree, has_exited, open_sealed,
    prepare_child, seal, tie_children_to_this_process,
};

#[cfg(all(unix, test))]
pub use unix::socket_path;
#[cfg(unix)]
pub use unix::{
    LocalListener, LocalStream, ProcessTree, connect_local, end_process_tree, has_exited, open_sealed,
    prepare_child, seal, tie_children_to_this_process,
};

/// The file name of a program: `ffmpeg` is `ffmpeg.exe` on Windows.
pub fn exe_name(base: &str) -> String {
    if cfg!(windows) { format!("{base}.exe") } else { base.to_string() }
}
