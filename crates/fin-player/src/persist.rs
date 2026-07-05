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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{QueueItem, RepeatMode};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn item(id: &str) -> QueueItem {
        QueueItem {
            id: id.into(),
            title: id.into(),
            subtitle: String::new(),
            stream_url: format!("http://example/{id}"),
            image_url: None,
            duration_secs: Some(180),
            is_video: false,
            content_type: "audio/flac".into(),
        }
    }

    fn tmp_path(name: &str) -> std::path::PathBuf {
        // Unique-per-call so parallel tests don't step on each other.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "fin-persist-test-{name}-{}-{n}.json",
            std::process::id()
        ))
    }

    // ------------------------------------------------------------------
    // Load edge cases
    // ------------------------------------------------------------------

    #[test]
    fn load_returns_none_when_path_missing() {
        let path = tmp_path("missing");
        // Sanity: file truly doesn't exist.
        assert!(!path.exists());
        assert!(load(&path).is_none());
    }

    #[test]
    fn load_returns_none_on_malformed_json() {
        let path = tmp_path("bad-json");
        std::fs::write(&path, b"this is not JSON at all").unwrap();
        assert!(load(&path).is_none());
        let _ = std::fs::remove_file(&path);
    }

    // ------------------------------------------------------------------
    // Serde round-trip through write_atomically + load
    // ------------------------------------------------------------------

    #[test]
    fn write_then_load_round_trips_every_field() {
        let path = tmp_path("round-trip");
        let snap = PersistedQueue {
            items: vec![item("a"), item("b")],
            current_index: Some(1),
            shuffle: true,
            repeat: RepeatMode::All,
            position_secs: 42.5,
        };
        write_atomically(&path, &snap).expect("write");
        let restored = load(&path).expect("load");
        assert_eq!(restored.items.len(), 2);
        assert_eq!(restored.items[1].id, "b");
        assert_eq!(restored.current_index, Some(1));
        assert!(restored.shuffle);
        assert_eq!(restored.repeat, RepeatMode::All);
        assert!((restored.position_secs - 42.5).abs() < 1e-9);
        let _ = std::fs::remove_file(&path);
    }

    // ------------------------------------------------------------------
    // Backward compatibility: older writers didn't emit every field
    // ------------------------------------------------------------------

    #[test]
    fn load_accepts_partial_json_with_defaults() {
        // Emulate an older on-disk shape that predates the shuffle / repeat
        // / position_secs additions. Every new field has #[serde(default)],
        // so this must still parse into a zero-value snapshot.
        let path = tmp_path("legacy");
        std::fs::write(&path, br#"{"items": []}"#).unwrap();
        let snap = load(&path).expect("load");
        assert!(snap.items.is_empty());
        assert_eq!(snap.current_index, None);
        assert!(!snap.shuffle);
        assert_eq!(snap.repeat, RepeatMode::Off);
        assert_eq!(snap.position_secs, 0.0);
        let _ = std::fs::remove_file(&path);
    }

    // ------------------------------------------------------------------
    // Write is atomic — no stray .tmp left behind
    // ------------------------------------------------------------------

    #[test]
    fn write_atomically_removes_the_temp_file() {
        let path = tmp_path("atomic");
        write_atomically(&path, &PersistedQueue::default()).expect("write");
        // The .tmp sibling MUST be gone after the rename step.
        assert!(!path.with_extension("json.tmp").exists());
        assert!(path.exists());
        let _ = std::fs::remove_file(&path);
    }
}
