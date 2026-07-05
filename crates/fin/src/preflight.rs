use std::process::Command;

/// Check whether mpv is on `$PATH`. Audio playback goes through the built-in
/// symphonia + cpal path and doesn't need mpv, so a missing binary is only
/// a problem when the user tries to play video. We surface a hint on the way
/// in rather than failing outright.
pub fn probe_mpv() -> bool {
    Command::new("mpv")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn mpv_install_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "brew install mpv"
    } else if cfg!(target_os = "linux") {
        "sudo apt install mpv   # or:  sudo pacman -S mpv"
    } else if cfg!(target_os = "windows") {
        "winget install mpv    # or:  scoop install mpv"
    } else {
        "install mpv from https://mpv.io"
    }
}
