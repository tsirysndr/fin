use std::process::Command;

use anyhow::{bail, Result};

/// Verify that mpv is on `$PATH`. fin uses mpv both as its local renderer and
/// as a fallback probe, so we refuse to start without it.
pub fn ensure_mpv() -> Result<()> {
    let ok = Command::new("mpv")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        return Ok(());
    }
    let hint = if cfg!(target_os = "macos") {
        "brew install mpv"
    } else if cfg!(target_os = "linux") {
        "sudo apt install mpv   # or:  sudo pacman -S mpv"
    } else if cfg!(target_os = "windows") {
        "winget install mpv    # or:  scoop install mpv"
    } else {
        "install mpv from https://mpv.io"
    };
    bail!(
        "mpv is required but was not found on PATH.\n\nInstall it and try again:\n  {}\n",
        hint
    );
}
