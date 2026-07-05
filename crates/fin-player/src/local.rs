//! Local renderer — routes each item to the right backend:
//! audio → symphonia (never mpv), video → mpv.
//!
//! To the caller this looks like a single `Renderer`. Internally we keep a
//! merged queue view so pause/next/prev/etc. hit whichever backend is
//! currently sourcing sound to the speakers or pixels to the screen.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use parking_lot::Mutex;
use tracing::debug;

use fin_config::EqBand;

use crate::crossfade::CrossfadeSettings;
use crate::mpv::MpvRenderer;
use crate::persist::PersistedQueue;
use crate::queue::{QueueItem, RepeatMode};
use crate::renderer::{PlaybackState, PlaybackStatus, Renderer, RendererKind};
use crate::replaygain::ReplayGainSettings;
use crate::symphonia_player::SymphoniaPlayer;

/// Which backend is currently sourcing playback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Active {
    None,
    Audio,
    Video,
}

pub struct LocalRenderer {
    audio: Arc<SymphoniaPlayer>,
    video: Arc<MpvRenderer>,
    active: Arc<Mutex<Active>>,
}

impl LocalRenderer {
    pub fn new() -> Self {
        Self::with_persist(None)
    }

    /// Build a LocalRenderer whose SymphoniaPlayer persists its queue to
    /// `queue_path`. mpv's video queue is transient by design — a video
    /// process spawns per session and doesn't persist.
    pub fn with_persist(queue_path: Option<PathBuf>) -> Self {
        Self {
            audio: Arc::new(SymphoniaPlayer::with_persist(queue_path)),
            video: Arc::new(MpvRenderer::new(None)),
            active: Arc::new(Mutex::new(Active::None)),
        }
    }

    fn set_active(&self, a: Active) {
        *self.active.lock() = a;
    }

    fn get_active(&self) -> Active {
        *self.active.lock()
    }

    /// Split a heterogeneous queue by media kind. The current API doesn't let
    /// us interleave audio and video across two backends, so when a queue
    /// contains both we play the first item's kind and hand the same list to
    /// that backend — filtering out mismatched items. In practice a queue is
    /// almost always uniform (all audio from an album, or a single video).
    fn dispatch_target(items: &[QueueItem], start_index: usize) -> Active {
        items
            .get(start_index)
            .map(|i| {
                if i.is_video {
                    Active::Video
                } else {
                    Active::Audio
                }
            })
            .unwrap_or(Active::None)
    }

    async fn stop_other(&self, keep: Active) {
        // Only the currently-active backend has a running stream/process, so
        // we only stop the opposite one to avoid churn.
        match (self.get_active(), keep) {
            (Active::Audio, Active::Video) => {
                let _ = self.audio.stop().await;
            }
            (Active::Video, Active::Audio) => {
                let _ = self.video.stop().await;
            }
            _ => {}
        }
    }
}

impl Default for LocalRenderer {
    fn default() -> Self {
        Self::new()
    }
}

fn filter_kind(items: Vec<QueueItem>, want_video: bool) -> Vec<QueueItem> {
    items
        .into_iter()
        .filter(|i| i.is_video == want_video)
        .collect()
}

#[async_trait]
impl Renderer for LocalRenderer {
    fn kind(&self) -> RendererKind {
        RendererKind::Mpv
    }

    async fn play(&self, items: Vec<QueueItem>, start_index: usize) -> Result<()> {
        let target = Self::dispatch_target(&items, start_index);
        self.stop_other(target).await;
        match target {
            Active::Audio => {
                let filtered = filter_kind(items, false);
                let idx = start_index.min(filtered.len().saturating_sub(1));
                debug!(items = filtered.len(), "local: routing audio → symphonia");
                self.set_active(Active::Audio);
                self.audio.play(filtered, idx).await
            }
            Active::Video => {
                let filtered = filter_kind(items, true);
                let idx = start_index.min(filtered.len().saturating_sub(1));
                debug!(items = filtered.len(), "local: routing video → mpv");
                self.set_active(Active::Video);
                self.video.play(filtered, idx).await
            }
            Active::None => Ok(()),
        }
    }

    async fn enqueue(&self, items: Vec<QueueItem>) -> Result<()> {
        // Split items and enqueue on whichever backend matches. If nothing is
        // playing yet, kick off playback on whichever kind we saw first.
        let (audio_items, video_items): (Vec<_>, Vec<_>) =
            items.into_iter().partition(|i| !i.is_video);
        let active = self.get_active();
        match active {
            Active::Audio => {
                if !audio_items.is_empty() {
                    self.audio.enqueue(audio_items).await?;
                }
                if !video_items.is_empty() {
                    debug!(
                        "dropping {} video item(s) — audio already active",
                        video_items.len()
                    );
                }
                Ok(())
            }
            Active::Video => {
                if !video_items.is_empty() {
                    self.video.enqueue(video_items).await?;
                }
                if !audio_items.is_empty() {
                    debug!(
                        "dropping {} audio item(s) — video already active",
                        audio_items.len()
                    );
                }
                Ok(())
            }
            Active::None => {
                if !audio_items.is_empty() {
                    self.set_active(Active::Audio);
                    self.audio.play(audio_items, 0).await
                } else if !video_items.is_empty() {
                    self.set_active(Active::Video);
                    self.video.play(video_items, 0).await
                } else {
                    Ok(())
                }
            }
        }
    }

    async fn play_next(&self, items: Vec<QueueItem>) -> Result<()> {
        let (audio_items, video_items): (Vec<_>, Vec<_>) =
            items.into_iter().partition(|i| !i.is_video);
        match self.get_active() {
            Active::Audio => {
                if !audio_items.is_empty() {
                    self.audio.play_next(audio_items).await?;
                }
                Ok(())
            }
            Active::Video => {
                if !video_items.is_empty() {
                    self.video.play_next(video_items).await?;
                }
                Ok(())
            }
            Active::None => {
                if !audio_items.is_empty() {
                    self.set_active(Active::Audio);
                    self.audio.play(audio_items, 0).await
                } else if !video_items.is_empty() {
                    self.set_active(Active::Video);
                    self.video.play(video_items, 0).await
                } else {
                    Ok(())
                }
            }
        }
    }

    async fn pause(&self) -> Result<()> {
        match self.get_active() {
            Active::Audio => self.audio.pause().await,
            Active::Video => self.video.pause().await,
            Active::None => Ok(()),
        }
    }

    async fn resume(&self) -> Result<()> {
        match self.get_active() {
            Active::Audio => self.audio.resume().await,
            Active::Video => self.video.resume().await,
            Active::None => Ok(()),
        }
    }

    async fn stop(&self) -> Result<()> {
        let a = self.get_active();
        self.set_active(Active::None);
        match a {
            Active::Audio => self.audio.stop().await,
            Active::Video => self.video.stop().await,
            Active::None => Ok(()),
        }
    }

    async fn next(&self) -> Result<()> {
        match self.get_active() {
            Active::Audio => self.audio.next().await,
            Active::Video => self.video.next().await,
            Active::None => Ok(()),
        }
    }

    async fn previous(&self) -> Result<()> {
        match self.get_active() {
            Active::Audio => self.audio.previous().await,
            Active::Video => self.video.previous().await,
            Active::None => Ok(()),
        }
    }

    async fn seek(&self, position_secs: f64) -> Result<()> {
        match self.get_active() {
            Active::Audio => self.audio.seek(position_secs).await,
            Active::Video => self.video.seek(position_secs).await,
            Active::None => Ok(()),
        }
    }

    async fn set_volume(&self, volume: f32) -> Result<()> {
        // Volume changes on both — the user's expectation is a single global slider.
        let _ = self.audio.set_volume(volume).await;
        let _ = self.video.set_volume(volume).await;
        Ok(())
    }

    async fn set_shuffle(&self, on: bool) -> Result<()> {
        // Shuffle currently only affects the audio queue. mpv's video queue
        // handles this differently and we haven't wired shuffle in there.
        self.audio.set_shuffle(on).await
    }

    async fn set_repeat(&self, mode: RepeatMode) -> Result<()> {
        self.audio.set_repeat(mode).await
    }

    async fn restore(&self, snapshot: PersistedQueue) -> Result<()> {
        // Restore always goes to the audio path — that's where persistence
        // lives. If the snapshot has video items they're dropped by the
        // SymphoniaPlayer's Restore handler.
        self.set_active(Active::Audio);
        self.audio.restore(snapshot).await
    }

    async fn remove_from_queue(&self, index: usize) -> Result<()> {
        match self.get_active() {
            Active::Audio => self.audio.remove_from_queue(index).await,
            Active::Video => self.video.remove_from_queue(index).await,
            Active::None => Ok(()),
        }
    }

    async fn set_replaygain(&self, settings: ReplayGainSettings) -> Result<()> {
        // ReplayGain is applied in the audio decode path — mpv has its own
        // separate volume model for video that we don't touch here.
        self.audio.set_replaygain(settings).await
    }

    async fn set_crossfade(&self, settings: CrossfadeSettings) -> Result<()> {
        // Only audio-side has the dual-track crossfade wiring.
        self.audio.set_crossfade(settings).await
    }

    async fn set_eq(&self, enabled: bool, bands: Vec<EqBand>) -> Result<()> {
        // EQ runs in the audio decode path — mpv-driven video is not affected.
        self.audio.set_eq(enabled, bands).await
    }

    fn state(&self) -> PlaybackState {
        match self.get_active() {
            Active::Audio => self.audio.state(),
            Active::Video => self.video.state(),
            Active::None => PlaybackState {
                status: PlaybackStatus::Idle,
                ..Default::default()
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(id: &str) -> QueueItem {
        QueueItem {
            id: id.into(),
            title: id.into(),
            subtitle: String::new(),
            stream_url: format!("http://example/{id}.flac"),
            image_url: None,
            duration_secs: Some(120),
            is_video: false,
            content_type: "audio/flac".into(),
        }
    }

    fn video(id: &str) -> QueueItem {
        QueueItem {
            id: id.into(),
            title: id.into(),
            subtitle: String::new(),
            stream_url: format!("http://example/{id}.mp4"),
            image_url: None,
            duration_secs: Some(600),
            is_video: true,
            content_type: "video/mp4".into(),
        }
    }

    #[test]
    fn dispatch_picks_kind_of_item_at_start_index() {
        let items = vec![audio("a"), video("b"), audio("c")];
        assert_eq!(LocalRenderer::dispatch_target(&items, 0), Active::Audio);
        assert_eq!(LocalRenderer::dispatch_target(&items, 1), Active::Video);
        assert_eq!(LocalRenderer::dispatch_target(&items, 2), Active::Audio);
    }

    #[test]
    fn dispatch_on_empty_or_out_of_range_returns_none() {
        assert_eq!(LocalRenderer::dispatch_target(&[], 0), Active::None);
        let items = vec![audio("a")];
        assert_eq!(LocalRenderer::dispatch_target(&items, 5), Active::None);
    }

    #[test]
    fn filter_kind_keeps_only_matching_items() {
        let mixed = vec![audio("a"), video("b"), audio("c"), video("d")];
        let audio_only = filter_kind(mixed.clone(), false);
        assert_eq!(
            audio_only.iter().map(|i| i.id.clone()).collect::<Vec<_>>(),
            vec!["a", "c"]
        );
        let video_only = filter_kind(mixed, true);
        assert_eq!(
            video_only.iter().map(|i| i.id.clone()).collect::<Vec<_>>(),
            vec!["b", "d"]
        );
    }

    #[test]
    fn filter_kind_on_empty_returns_empty() {
        assert!(filter_kind(vec![], false).is_empty());
        assert!(filter_kind(vec![], true).is_empty());
    }
}
