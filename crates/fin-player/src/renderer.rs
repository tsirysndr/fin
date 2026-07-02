use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::queue::QueueItem;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlaybackStatus {
    Idle,
    Buffering,
    Playing,
    Paused,
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackState {
    pub status: PlaybackStatus,
    pub position_secs: f64,
    pub duration_secs: f64,
    pub volume: f32,
    pub now_playing: Option<QueueItem>,
    pub queue: Vec<QueueItem>,
    pub current_index: Option<usize>,
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            status: PlaybackStatus::Idle,
            position_secs: 0.0,
            duration_secs: 0.0,
            volume: 1.0,
            now_playing: None,
            queue: Vec::new(),
            current_index: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RendererKind {
    Mpv,
    Chromecast,
}

impl RendererKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Mpv => "mpv",
            Self::Chromecast => "chromecast",
        }
    }
}

/// Common interface implemented by both the local mpv renderer and the
/// Chromecast renderer. Every method is async because Chromecast operations
/// round-trip a TLS control connection.
#[async_trait]
pub trait Renderer: Send + Sync {
    fn kind(&self) -> RendererKind;

    /// Replace the queue and start playing at `start_index`.
    async fn play(&self, items: Vec<QueueItem>, start_index: usize) -> anyhow::Result<()>;

    /// Append items to the current queue without interrupting playback.
    async fn enqueue(&self, items: Vec<QueueItem>) -> anyhow::Result<()>;

    /// Insert items at the front of the queue (play next).
    async fn play_next(&self, items: Vec<QueueItem>) -> anyhow::Result<()>;

    async fn pause(&self) -> anyhow::Result<()>;
    async fn resume(&self) -> anyhow::Result<()>;
    async fn stop(&self) -> anyhow::Result<()>;
    async fn next(&self) -> anyhow::Result<()>;
    async fn previous(&self) -> anyhow::Result<()>;
    async fn seek(&self, position_secs: f64) -> anyhow::Result<()>;
    async fn set_volume(&self, volume: f32) -> anyhow::Result<()>;

    fn state(&self) -> PlaybackState;
}
