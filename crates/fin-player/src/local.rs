//! Local renderer — routes each item to the right backend:
//! audio → symphonia (never mpv), video → mpv.
//!
//! To the caller this looks like a single `Renderer`. Internally we keep a
//! merged queue view so pause/next/prev/etc. hit whichever backend is
//! currently sourcing sound to the speakers or pixels to the screen.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use parking_lot::Mutex;
use tracing::debug;

use crate::mpv::MpvRenderer;
use crate::queue::QueueItem;
use crate::renderer::{PlaybackState, PlaybackStatus, Renderer, RendererKind};
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
        Self {
            audio: Arc::new(SymphoniaPlayer::new()),
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
            .map(|i| if i.is_video { Active::Video } else { Active::Audio })
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
    items.into_iter().filter(|i| i.is_video == want_video).collect()
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
                    debug!("dropping {} video item(s) — audio already active", video_items.len());
                }
                Ok(())
            }
            Active::Video => {
                if !video_items.is_empty() {
                    self.video.enqueue(video_items).await?;
                }
                if !audio_items.is_empty() {
                    debug!("dropping {} audio item(s) — video already active", audio_items.len());
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
