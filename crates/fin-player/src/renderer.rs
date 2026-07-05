use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use fin_config::EqBand;

use crate::crossfade::CrossfadeSettings;
use crate::persist::PersistedQueue;
use crate::queue::{QueueItem, RepeatMode};
use crate::replaygain::ReplayGainSettings;

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
    #[serde(default)]
    pub shuffle: bool,
    #[serde(default)]
    pub repeat: RepeatMode,
    /// Whatever ReplayGain settings the renderer is currently honoring —
    /// mirrored on state so the TUI can show a badge without a separate
    /// query path.
    #[serde(default)]
    pub replaygain: ReplayGainSettings,
    #[serde(default)]
    pub crossfade: CrossfadeSettings,
    /// Whether the Rockbox EQ is currently active. Mirrored on state so the
    /// TUI can render an indicator without a separate query path.
    #[serde(default)]
    pub eq_enabled: bool,
    #[serde(default)]
    pub eq_band_count: usize,
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
            shuffle: false,
            repeat: RepeatMode::Off,
            replaygain: ReplayGainSettings::default(),
            crossfade: CrossfadeSettings::default(),
            eq_enabled: false,
            eq_band_count: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RendererKind {
    Mpv,
    Chromecast,
    Upnp,
}

impl RendererKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Mpv => "mpv",
            Self::Chromecast => "chromecast",
            Self::Upnp => "upnp",
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

    /// Turn shuffle on or off. Default is a no-op — Chromecast and UPnP
    /// renderers have their own device-side queue models that we haven't
    /// wired shuffle into yet.
    async fn set_shuffle(&self, _on: bool) -> anyhow::Result<()> {
        Ok(())
    }

    /// Set the repeat mode. See `set_shuffle` for why the default is a no-op.
    async fn set_repeat(&self, _mode: RepeatMode) -> anyhow::Result<()> {
        Ok(())
    }

    /// Populate the queue + playhead from a persisted snapshot without
    /// starting playback. The next `resume()` (or explicit `play`) kicks
    /// the loaded track, seeking to `snapshot.position_secs` on load.
    async fn restore(&self, _snapshot: PersistedQueue) -> anyhow::Result<()> {
        Ok(())
    }

    /// Remove one entry from the queue by index without disrupting the
    /// currently-playing track — unless that item is the one being removed,
    /// in which case playback advances to the next entry (or stops if the
    /// queue is now empty). Default is a no-op.
    async fn remove_from_queue(&self, _index: usize) -> anyhow::Result<()> {
        Ok(())
    }

    /// Update ReplayGain settings (mode, preamp, clip prevention). Non-local
    /// renderers currently no-op — device-side receivers apply their own
    /// loudness normalization.
    async fn set_replaygain(&self, _settings: ReplayGainSettings) -> anyhow::Result<()> {
        Ok(())
    }

    /// Update crossfade settings (mode + duration). Only the local
    /// SymphoniaPlayer implements this; Chromecast + UPnP receivers each
    /// manage their own track transitions.
    async fn set_crossfade(&self, _settings: CrossfadeSettings) -> anyhow::Result<()> {
        Ok(())
    }

    /// Enable/disable the Rockbox 10-band equalizer and load the band
    /// coefficients. `bands` is truncated to `EQ_NUM_BANDS`. Only the local
    /// SymphoniaPlayer implements this — non-local receivers each apply
    /// their own EQ (or none).
    async fn set_eq(&self, _enabled: bool, _bands: Vec<EqBand>) -> anyhow::Result<()> {
        Ok(())
    }

    fn state(&self) -> PlaybackState;
}
