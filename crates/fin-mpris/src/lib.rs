//! fin on the desktop session bus — an **MPRIS** `org.mpris.MediaPlayer2`
//! player, Linux only.
//!
//! The mirror image of `fin-mediarenderer` for the local desktop: instead of
//! LAN control points, this lets media keys, GNOME/KDE applets, waybar,
//! `playerctl` & co. drive whatever renderer the TUI currently holds. It
//! shares the same swappable renderer cell, so switching to a Chromecast in
//! the Devices screen transparently redirects Play/Pause from the desktop to
//! the cast session.
//!
//! Like the UPnP GENA path there is no event bus to subscribe to — a small
//! task polls `Renderer::state()` and diffs snapshots into D-Bus
//! `PropertiesChanged` / `Seeked` signals.

#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use mpris_server::zbus::{self, fdo};
use mpris_server::{
    LoopStatus, Metadata, PlaybackRate, PlaybackStatus as MprisStatus, PlayerInterface, Property,
    RootInterface, Server, Signal, Time, TrackId, Volume,
};
use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use fin_player::{PlaybackState, PlaybackStatus, QueueItem, Renderer, RepeatMode};

/// Same shape as `fin_mediarenderer::RendererCell` — the swappable handle
/// the TUI, the UPnP MediaRenderer and this MPRIS player all share.
pub type RendererCell = Arc<Mutex<Arc<dyn Renderer>>>;

/// How often the notify task snapshots the renderer. Position itself is
/// *not* pushed over D-Bus (clients extrapolate from rate), so 500 ms only
/// bounds how quickly track/status/volume changes reach applets.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Position drift beyond which we assume a real seek happened (vs. decode
/// jitter or a remote renderer's 500 ms status cadence) and emit `Seeked`.
const SEEK_JUMP_SECS: f64 = 2.0;

/// The `org.mpris.MediaPlayer2.<suffix>` implementation handed to zbus.
pub struct Player {
    renderer: RendererCell,
}

impl Player {
    fn renderer(&self) -> Arc<dyn Renderer> {
        self.renderer.lock().clone()
    }

    fn state(&self) -> PlaybackState {
        self.renderer().state()
    }
}

fn to_fdo(e: anyhow::Error) -> fdo::Error {
    fdo::Error::Failed(e.to_string())
}

fn to_zbus(e: anyhow::Error) -> zbus::Error {
    zbus::Error::from(to_fdo(e))
}

fn mpris_status(status: PlaybackStatus) -> MprisStatus {
    match status {
        PlaybackStatus::Playing | PlaybackStatus::Buffering => MprisStatus::Playing,
        PlaybackStatus::Paused => MprisStatus::Paused,
        PlaybackStatus::Idle | PlaybackStatus::Stopped => MprisStatus::Stopped,
    }
}

fn loop_status(repeat: RepeatMode) -> LoopStatus {
    match repeat {
        RepeatMode::Off => LoopStatus::None,
        RepeatMode::One => LoopStatus::Track,
        RepeatMode::All => LoopStatus::Playlist,
    }
}

fn repeat_mode(status: LoopStatus) -> RepeatMode {
    match status {
        LoopStatus::None => RepeatMode::Off,
        LoopStatus::Track => RepeatMode::One,
        LoopStatus::Playlist => RepeatMode::All,
    }
}

/// Item ids are Jellyfin GUIDs / Subsonic ids / `upnp-cast:<n>` — squash
/// anything that isn't a valid D-Bus path character so the trackid stays a
/// well-formed object path.
fn track_id(item: &QueueItem) -> TrackId {
    let mut safe: String = item
        .id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if safe.is_empty() {
        safe.push('_');
    }
    TrackId::try_from(format!("/rs/tsirysndr/fin/track/{safe}")).unwrap_or(TrackId::NO_TRACK)
}

fn metadata(state: &PlaybackState) -> Metadata {
    let Some(item) = &state.now_playing else {
        // Spec: an empty map with the NoTrack id tells clients to clear.
        return Metadata::builder().trackid(TrackId::NO_TRACK).build();
    };
    let mut b = Metadata::builder()
        .trackid(track_id(item))
        .title(item.title.clone());
    // `subtitle` is the artist for audio, series/season for video — the
    // closest thing to xesam:artist either way.
    if !item.subtitle.is_empty() {
        b = b.artist([item.subtitle.clone()]);
    }
    let duration = if state.duration_secs > 0.0 {
        state.duration_secs
    } else {
        item.duration_secs.unwrap_or(0) as f64
    };
    if duration > 0.0 {
        b = b.length(Time::from_micros((duration * 1_000_000.0) as i64));
    }
    if let Some(url) = &item.image_url {
        b = b.art_url(url.clone());
    }
    b.build()
}

impl RootInterface for Player {
    async fn raise(&self) -> fdo::Result<()> {
        // A TUI has no window to raise.
        Ok(())
    }

    async fn quit(&self) -> fdo::Result<()> {
        Ok(())
    }

    async fn can_quit(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn set_fullscreen(&self, _fullscreen: bool) -> zbus::Result<()> {
        Ok(())
    }

    async fn can_set_fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn can_raise(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn has_track_list(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn identity(&self) -> fdo::Result<String> {
        Ok("fin".to_string())
    }

    async fn desktop_entry(&self) -> fdo::Result<String> {
        Ok("fin".to_string())
    }

    async fn supported_uri_schemes(&self) -> fdo::Result<Vec<String>> {
        // Everything playable comes from the configured media server, not
        // arbitrary URIs — OpenUri is unsupported.
        Ok(Vec::new())
    }

    async fn supported_mime_types(&self) -> fdo::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

impl PlayerInterface for Player {
    async fn next(&self) -> fdo::Result<()> {
        self.renderer().next().await.map_err(to_fdo)
    }

    async fn previous(&self) -> fdo::Result<()> {
        self.renderer().previous().await.map_err(to_fdo)
    }

    async fn pause(&self) -> fdo::Result<()> {
        self.renderer().pause().await.map_err(to_fdo)
    }

    async fn play_pause(&self) -> fdo::Result<()> {
        let r = self.renderer();
        match r.state().status {
            PlaybackStatus::Playing | PlaybackStatus::Buffering => r.pause().await.map_err(to_fdo),
            _ => r.resume().await.map_err(to_fdo),
        }
    }

    async fn stop(&self) -> fdo::Result<()> {
        self.renderer().stop().await.map_err(to_fdo)
    }

    async fn play(&self) -> fdo::Result<()> {
        // `resume` also kicks a restored-but-not-started queue, so it covers
        // both the paused and the freshly-launched case.
        self.renderer().resume().await.map_err(to_fdo)
    }

    async fn seek(&self, offset: Time) -> fdo::Result<()> {
        let r = self.renderer();
        let state = r.state();
        let target = state.position_secs + offset.as_micros() as f64 / 1_000_000.0;
        // Spec: seeking past the end of the track acts like Next.
        if state.duration_secs > 0.0 && target > state.duration_secs {
            return r.next().await.map_err(to_fdo);
        }
        r.seek(target.max(0.0)).await.map_err(to_fdo)
    }

    async fn set_position(&self, track: TrackId, position: Time) -> fdo::Result<()> {
        let r = self.renderer();
        let state = r.state();
        // Spec: a stale trackid means the client raced a track change —
        // silently ignore rather than seeking the wrong song.
        let Some(current) = &state.now_playing else {
            return Ok(());
        };
        if track != track_id(current) {
            return Ok(());
        }
        let target = position.as_micros() as f64 / 1_000_000.0;
        if target < 0.0 || (state.duration_secs > 0.0 && target > state.duration_secs) {
            return Ok(());
        }
        r.seek(target).await.map_err(to_fdo)
    }

    async fn open_uri(&self, _uri: String) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported(
            "fin only plays items from the configured media server".to_string(),
        ))
    }

    async fn playback_status(&self) -> fdo::Result<MprisStatus> {
        Ok(mpris_status(self.state().status))
    }

    async fn loop_status(&self) -> fdo::Result<LoopStatus> {
        Ok(loop_status(self.state().repeat))
    }

    async fn set_loop_status(&self, status: LoopStatus) -> zbus::Result<()> {
        self.renderer()
            .set_repeat(repeat_mode(status))
            .await
            .map_err(to_zbus)
    }

    async fn rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }

    async fn set_rate(&self, _rate: PlaybackRate) -> zbus::Result<()> {
        // Only 1.0 is supported (min == max), so this is allowed to no-op.
        Ok(())
    }

    async fn shuffle(&self) -> fdo::Result<bool> {
        Ok(self.state().shuffle)
    }

    async fn set_shuffle(&self, shuffle: bool) -> zbus::Result<()> {
        self.renderer().set_shuffle(shuffle).await.map_err(to_zbus)
    }

    async fn metadata(&self) -> fdo::Result<Metadata> {
        Ok(metadata(&self.state()))
    }

    async fn volume(&self) -> fdo::Result<Volume> {
        Ok(self.state().volume as f64)
    }

    async fn set_volume(&self, volume: Volume) -> zbus::Result<()> {
        self.renderer()
            .set_volume(volume.clamp(0.0, 1.0) as f32)
            .await
            .map_err(to_zbus)
    }

    async fn position(&self) -> fdo::Result<Time> {
        Ok(Time::from_micros(
            (self.state().position_secs * 1_000_000.0) as i64,
        ))
    }

    async fn minimum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }

    async fn maximum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }

    async fn can_go_next(&self) -> fdo::Result<bool> {
        Ok(!self.state().queue.is_empty())
    }

    async fn can_go_previous(&self) -> fdo::Result<bool> {
        Ok(!self.state().queue.is_empty())
    }

    async fn can_play(&self) -> fdo::Result<bool> {
        let state = self.state();
        Ok(state.now_playing.is_some() || !state.queue.is_empty())
    }

    async fn can_pause(&self) -> fdo::Result<bool> {
        Ok(self.state().now_playing.is_some())
    }

    async fn can_seek(&self) -> fdo::Result<bool> {
        Ok(self.state().now_playing.is_some())
    }

    async fn can_control(&self) -> fdo::Result<bool> {
        Ok(true)
    }
}

/// Everything the notify task diffs between polls. Position is tracked
/// separately (it changes every tick by design).
#[derive(PartialEq)]
struct Snapshot {
    status: MprisStatus,
    // Track identity + the fields that can settle in *after* the id is known
    // (duration arrives once the demuxer opens the stream).
    track: Option<(String, String, u64, Option<String>)>,
    volume_milli: i32,
    shuffle: bool,
    repeat: RepeatMode,
    can_next: bool,
    can_play: bool,
    can_pause: bool,
}

impl Snapshot {
    fn of(state: &PlaybackState) -> Self {
        Self {
            status: mpris_status(state.status),
            track: state.now_playing.as_ref().map(|i| {
                let duration = if state.duration_secs > 0.0 {
                    state.duration_secs as u64
                } else {
                    i.duration_secs.unwrap_or(0)
                };
                (i.id.clone(), i.title.clone(), duration, i.image_url.clone())
            }),
            volume_milli: (state.volume * 1000.0).round() as i32,
            shuffle: state.shuffle,
            repeat: state.repeat,
            can_next: !state.queue.is_empty(),
            can_play: state.now_playing.is_some() || !state.queue.is_empty(),
            can_pause: state.now_playing.is_some(),
        }
    }
}

/// Handle to the running MPRIS server. Dropping (or `shutdown()`) aborts the
/// notify task, which drops the bus connection and releases the name.
pub struct MprisServer {
    task: JoinHandle<()>,
}

impl MprisServer {
    /// Register `org.mpris.MediaPlayer2.fin` on the session bus and start
    /// the notify task. Falls back to a pid-suffixed name when another fin
    /// instance already owns the plain one, per the MPRIS spec.
    pub async fn start(renderer: RendererCell) -> Result<Self> {
        let server = match Server::new(
            "fin",
            Player {
                renderer: renderer.clone(),
            },
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                debug!(
                    ?e,
                    "org.mpris.MediaPlayer2.fin unavailable — retrying with instance suffix"
                );
                Server::new(
                    &format!("fin.instance{}", std::process::id()),
                    Player { renderer },
                )
                .await
                .context("register MPRIS bus name")?
            }
        };
        info!(bus = %server.bus_name(), "MPRIS player up");
        let task = tokio::spawn(notify_loop(server));
        Ok(Self { task })
    }

    pub fn shutdown(self) {
        self.task.abort();
    }
}

async fn notify_loop(server: Server<Player>) {
    let state = server.imp().state();
    let mut last = Snapshot::of(&state);
    let mut last_pos = state.position_secs;
    let mut last_tick = Instant::now();
    let mut interval = tokio::time::interval(POLL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let state = server.imp().state();
        let snap = Snapshot::of(&state);

        let mut props = Vec::new();
        if snap.status != last.status {
            props.push(Property::PlaybackStatus(snap.status));
        }
        if snap.track != last.track {
            props.push(Property::Metadata(metadata(&state)));
        }
        if snap.volume_milli != last.volume_milli {
            props.push(Property::Volume(state.volume as f64));
        }
        if snap.shuffle != last.shuffle {
            props.push(Property::Shuffle(snap.shuffle));
        }
        if snap.repeat != last.repeat {
            props.push(Property::LoopStatus(loop_status(snap.repeat)));
        }
        if snap.can_next != last.can_next {
            props.push(Property::CanGoNext(snap.can_next));
            props.push(Property::CanGoPrevious(snap.can_next));
        }
        if snap.can_play != last.can_play {
            props.push(Property::CanPlay(snap.can_play));
        }
        if snap.can_pause != last.can_pause {
            props.push(Property::CanPause(snap.can_pause));
            props.push(Property::CanSeek(snap.can_pause));
        }

        // Clients extrapolate position from PlaybackStatus + Rate; only a
        // discontinuity on the *same* track warrants a Seeked signal. Track
        // changes reset the expectation instead (spec says starting at 0 is
        // the assumed default).
        let same_track = snap.track.as_ref().map(|t| &t.0) == last.track.as_ref().map(|t| &t.0);
        let expected = if last.status == MprisStatus::Playing {
            last_pos + last_tick.elapsed().as_secs_f64()
        } else {
            last_pos
        };
        if same_track && (state.position_secs - expected).abs() > SEEK_JUMP_SECS {
            let position = Time::from_micros((state.position_secs * 1_000_000.0) as i64);
            if let Err(e) = server.emit(Signal::Seeked { position }).await {
                warn!(?e, "MPRIS Seeked emit failed");
            }
        }

        if !props.is_empty() {
            if let Err(e) = server.properties_changed(props).await {
                warn!(?e, "MPRIS PropertiesChanged emit failed");
            }
        }

        last = snap;
        last_pos = state.position_secs;
        last_tick = Instant::now();
    }
}
