//! Background persistence of the playback queue.
//!
//! The `SymphoniaPlayer` worker sends a `PersistedQueue` snapshot whenever
//! the queue mutates or every few seconds while playing. A dedicated writer
//! thread coalesces bursts (rapid consecutive mutations only produce one
//! disk write) and atomically renames a `.tmp` into place, so a crash mid-
//! write can never leave a truncated `queue.json` on disk.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::queue::{QueueItem, RepeatMode};

/// The on-disk snapshot format. Every field is `#[serde(default)]` so an
/// older `queue.json` written by a previous fin version keeps loading.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistedQueue {
    #[serde(default)]
    pub items: Vec<QueueItem>,
    #[serde(default)]
    pub current_index: Option<usize>,
    #[serde(default)]
    pub shuffle: bool,
    #[serde(default)]
    pub repeat: RepeatMode,
    /// Playhead within the currently-playing track. Restored via a "pending
    /// seek" on the next track load so the user picks up exactly where they
    /// left off.
    #[serde(default)]
    pub position_secs: f64,
}

/// Read `queue.json`. Silent-none on any error — a missing / corrupt file
/// just means "start with an empty queue", not a fatal condition.
pub fn load(path: &Path) -> Option<PersistedQueue> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// A cloneable handle that forwards snapshots to a background writer thread.
/// Dropping the last clone closes the channel and the writer exits cleanly.
#[derive(Clone)]
pub struct Persister {
    tx: mpsc::Sender<PersistedQueue>,
}

impl Persister {
    pub fn spawn(path: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel::<PersistedQueue>();
        thread::Builder::new()
            .name("fin-queue-persist".into())
            .spawn(move || run_writer(rx, path))
            .expect("spawn queue persist thread");
        Self { tx }
    }

    pub fn queue_write(&self, snap: PersistedQueue) {
        let _ = self.tx.send(snap);
    }
}

fn run_writer(rx: mpsc::Receiver<PersistedQueue>, path: PathBuf) {
    loop {
        let mut latest = match rx.recv() {
            Ok(s) => s,
            Err(_) => return, // all Persister handles dropped
        };
        // Coalesce rapid bursts.
        while let Ok(more) = rx.try_recv() {
            latest = more;
        }
        // Debounce for 100 ms — a queue mutation followed by an immediate
        // status update collapses into a single write.
        thread::sleep(Duration::from_millis(100));
        while let Ok(more) = rx.try_recv() {
            latest = more;
        }
        if let Err(e) = write_atomically(&path, &latest) {
            warn!(error = ?e, path = ?path, "queue persist failed");
        }
    }
}

fn write_atomically(path: &Path, snap: &PersistedQueue) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(snap)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
