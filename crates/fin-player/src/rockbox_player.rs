//! Local audio-only renderer backed by [`rockbox_playback::Player`].
//!
//! The heavy lifting — queue, transport, shuffle/repeat, exact-position
//! resume, ReplayGain, crossfade, the full Rockbox DSP chain (10-band EQ,
//! tone controls, …) and HTTP(S) streaming — all lives in the
//! `rockbox-playback` engine. This module is a thin adapter that maps fin's
//! [`Renderer`] trait onto that engine.
//!
//! The engine keys everything off track paths / URLs, so we hand it each
//! item's `stream_url` and keep a parallel `Vec<QueueItem>` alongside it.
//! That parallel list is what lets fin keep showing server-provided titles
//! and artwork in [`state`](Renderer::state) — metadata the engine (which
//! only knows the decoded audio tags) has no way to reproduce.
//!
//! `Player` owns a `cpal` output stream, which makes it neither `Send` nor
//! `Sync`, whereas [`Renderer`] must be both. So the `Player` lives on a
//! dedicated worker thread and every operation is a `FnOnce(&Player)` closure
//! sent over a channel — the same shape the old symphonia worker used. Only
//! the `Sender` and a metadata mirror (both `Send + Sync`) are held in the
//! struct.
//!
//! mpv is not involved anywhere in this path — it is reserved for video by
//! the sibling `LocalRenderer` dispatcher.

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use parking_lot::Mutex;
use tracing::error;

use rockbox_playback::{
    CrossfadeMode as RbCrossfadeMode, CrossfadeSettings as RbCrossfade, EqBand as RbEqBand,
    MixMode, PlaybackState as RbState, Player, PlayerConfig, RepeatMode as RbRepeat,
    ReplayGainMode as RbReplayGainMode, ToneControls, EQ_BANDS,
};

use fin_config::EqBand;

use crate::crossfade::{CrossfadeMode, CrossfadeSettings};
use crate::persist::{PersistedQueue, Persister};
use crate::queue::{QueueItem, RepeatMode};
use crate::renderer::{PlaybackState, PlaybackStatus, Renderer, RendererKind};
use crate::replaygain::{ReplayGainMode, ReplayGainSettings};

/// A `Send` snapshot of the engine's live status, plucked off the worker
/// thread (which is the only place the `!Send` `Player` can be touched).
#[derive(Clone, Copy)]
struct EngineStatus {
    state: RbState,
    index: Option<usize>,
    position: Duration,
    duration: Duration,
    volume: f32,
    shuffle: bool,
    repeat: RbRepeat,
}

/// Work handed to the player thread. Every op is expressed as a closure over
/// `&Player` so each `Renderer` method captures only plain `Send` data.
enum Command {
    /// Run an operation against the engine.
    Op(Box<dyn FnOnce(&Player) + Send>),
    /// Reply with the current engine status (`None` if there is no engine).
    Status(mpsc::Sender<Option<EngineStatus>>),
    /// Join cleanly.
    Quit,
}

/// Display-only mirror of state the `rockbox-playback` engine either doesn't
/// expose in fin's own types, or that we want to surface without a decode.
/// The queue *order* and playhead live in the engine — this is metadata.
#[derive(Default)]
struct Mirror {
    /// The queue as fin knows it, in engine order — the source of the
    /// titles/artwork/`is_video`/`content_type` shown in the UI.
    items: Vec<QueueItem>,
    /// Settings echoed back on `state()` so the TUI can render badges
    /// without a separate query path. These mirror the last values pushed
    /// into the engine.
    replaygain: ReplayGainSettings,
    crossfade: CrossfadeSettings,
    eq_enabled: bool,
    eq_band_count: usize,
    bass_db: i32,
    treble_db: i32,
}

/// A local, audio-only renderer that drives a `rockbox-playback` engine on a
/// dedicated worker thread.
pub struct RockboxPlayer {
    cmd_tx: mpsc::Sender<Command>,
    mirror: Mutex<Mirror>,
    /// Background writer for the metadata sidecar (fin's `queue.json`). The
    /// engine persists the queue + exact position itself via its resume
    /// file; this only records the display metadata the engine can't.
    persister: Option<Persister>,
    /// Kept so the worker is joined on `Drop`.
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl RockboxPlayer {
    pub fn new() -> Self {
        Self::with_persist(None)
    }

    /// Build a player that persists across restarts. `queue_path` is fin's
    /// metadata sidecar (`queue.json`); the engine's own resume file — which
    /// holds the queue order and the exact playhead — lives next to it.
    pub fn with_persist(queue_path: Option<PathBuf>) -> Self {
        let resume_file = queue_path
            .as_ref()
            .map(|p| p.with_file_name("rockbox-resume.m3u8"));

        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
        let worker = thread::Builder::new()
            .name("fin-rockbox".into())
            .spawn(move || run_worker(cmd_rx, resume_file))
            .expect("spawn rockbox-playback worker thread");

        Self {
            cmd_tx,
            mirror: Mutex::new(Mirror::default()),
            persister: queue_path.map(Persister::spawn),
            worker: Mutex::new(Some(worker)),
        }
    }

    /// Queue an operation to run against the engine on the worker thread.
    fn exec<F: FnOnce(&Player) + Send + 'static>(&self, f: F) {
        let _ = self.cmd_tx.send(Command::Op(Box::new(f)));
    }

    /// Block briefly for a fresh engine-status snapshot. The worker is
    /// almost always idle on its channel, so this round-trips immediately.
    fn engine_status(&self) -> Option<EngineStatus> {
        let (tx, rx) = mpsc::channel();
        self.cmd_tx.send(Command::Status(tx)).ok()?;
        rx.recv().ok().flatten()
    }

    /// Push a snapshot of the display metadata to the sidecar writer. Cheap
    /// and debounced downstream, so it's fine to call on every mutation.
    fn persist(&self) {
        let Some(persister) = &self.persister else {
            return;
        };
        let (index, position_secs, shuffle, repeat) = self
            .engine_status()
            .map(|s| {
                (
                    s.index,
                    s.position.as_secs_f64(),
                    s.shuffle,
                    from_rb_repeat(s.repeat),
                )
            })
            .unwrap_or((None, 0.0, false, RepeatMode::Off));
        persister.queue_write(PersistedQueue {
            items: self.mirror.lock().items.clone(),
            current_index: index,
            shuffle,
            repeat,
            position_secs,
        });
    }
}

impl Default for RockboxPlayer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for RockboxPlayer {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Command::Quit);
        if let Some(h) = self.worker.lock().take() {
            let _ = h.join();
        }
    }
}

/// The player thread: owns the `!Send` `Player` and services commands.
fn run_worker(rx: mpsc::Receiver<Command>, resume_file: Option<PathBuf>) {
    let config = PlayerConfig {
        resume_file,
        ..PlayerConfig::default()
    };
    let player = match Player::with_config(config) {
        Ok(p) => Some(p),
        Err(e) => {
            error!(error = ?e, "could not open audio output — local playback disabled");
            None
        }
    };

    for cmd in rx {
        match cmd {
            Command::Op(f) => {
                if let Some(p) = &player {
                    f(p);
                }
            }
            Command::Status(reply) => {
                let snapshot = player.as_ref().map(|p| {
                    let st = p.status();
                    EngineStatus {
                        state: st.state,
                        index: st.index,
                        position: st.position,
                        duration: st.duration,
                        volume: p.volume(),
                        shuffle: st.shuffle,
                        repeat: st.repeat,
                    }
                });
                let _ = reply.send(snapshot);
            }
            Command::Quit => break,
        }
    }
}

/// Collect the engine-facing track list (stream URLs) from fin queue items.
fn urls(items: &[QueueItem]) -> Vec<String> {
    items.iter().map(|i| i.stream_url.clone()).collect()
}

fn to_rb_repeat(mode: RepeatMode) -> RbRepeat {
    match mode {
        RepeatMode::Off => RbRepeat::Off,
        RepeatMode::One => RbRepeat::One,
        RepeatMode::All => RbRepeat::All,
    }
}

fn from_rb_repeat(mode: RbRepeat) -> RepeatMode {
    match mode {
        RbRepeat::Off => RepeatMode::Off,
        RbRepeat::One => RepeatMode::One,
        RbRepeat::All => RepeatMode::All,
    }
}

fn to_rb_replaygain_mode(mode: ReplayGainMode) -> RbReplayGainMode {
    match mode {
        ReplayGainMode::Off => RbReplayGainMode::Off,
        ReplayGainMode::Track => RbReplayGainMode::Track,
        ReplayGainMode::Album => RbReplayGainMode::Album,
    }
}

/// Map fin's simplified crossfade config onto the engine's fuller model.
/// fin exposes just off / crossfade / mixed with a single symmetric
/// duration; that becomes an "always" crossfade with equal fade-in/out
/// ramps and no lead-in/out delay.
fn to_rb_crossfade(settings: CrossfadeSettings) -> RbCrossfade {
    let dur = Duration::from_secs_f32(settings.duration_secs.max(0.0));
    match settings.mode {
        CrossfadeMode::Off => RbCrossfade {
            mode: RbCrossfadeMode::Off,
            ..RbCrossfade::default()
        },
        CrossfadeMode::Crossfade => RbCrossfade {
            mode: RbCrossfadeMode::Always,
            fade_out_duration: dur,
            fade_in_duration: dur,
            mix_mode: MixMode::Crossfade,
            ..RbCrossfade::default()
        },
        CrossfadeMode::Mixed => RbCrossfade {
            mode: RbCrossfadeMode::Always,
            fade_out_duration: dur,
            fade_in_duration: dur,
            mix_mode: MixMode::Mix,
            ..RbCrossfade::default()
        },
    }
}

/// fin `EqBand` stores Q and gain scaled ×10 (integer tenths); the engine
/// wants real units.
fn to_rb_eq_band(band: &EqBand) -> RbEqBand {
    RbEqBand {
        cutoff_hz: band.cutoff,
        q: band.q as f32 / 10.0,
        gain_db: band.gain as f32 / 10.0,
    }
}

#[async_trait]
impl Renderer for RockboxPlayer {
    fn kind(&self) -> RendererKind {
        RendererKind::Mpv
    }

    async fn play(&self, items: Vec<QueueItem>, start_index: usize) -> Result<()> {
        let track_urls = urls(&items);
        let len = items.len();
        self.mirror.lock().items = items;
        self.exec(move |p| {
            p.set_queue(track_urls);
            if start_index > 0 && start_index < len {
                p.skip_to(start_index);
            }
            p.play();
        });
        self.persist();
        Ok(())
    }

    async fn enqueue(&self, items: Vec<QueueItem>) -> Result<()> {
        let track_urls = urls(&items);
        self.mirror.lock().items.extend(items);
        self.exec(move |p| p.insert_tracks_last(track_urls));
        self.persist();
        Ok(())
    }

    async fn play_next(&self, items: Vec<QueueItem>) -> Result<()> {
        let track_urls = urls(&items);
        // Mirror the engine's placement: right after the current track.
        let at = self
            .engine_status()
            .and_then(|s| s.index)
            .map(|i| i + 1)
            .unwrap_or(0);
        {
            let mut mirror = self.mirror.lock();
            let at = at.min(mirror.items.len());
            for (offset, item) in items.into_iter().enumerate() {
                let pos = (at + offset).min(mirror.items.len());
                mirror.items.insert(pos, item);
            }
        }
        self.exec(move |p| p.insert_tracks_next(track_urls));
        self.persist();
        Ok(())
    }

    async fn pause(&self) -> Result<()> {
        self.exec(|p| p.pause());
        Ok(())
    }

    async fn resume(&self) -> Result<()> {
        self.exec(|p| p.play());
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.exec(|p| p.stop());
        Ok(())
    }

    async fn next(&self) -> Result<()> {
        self.exec(|p| p.next());
        Ok(())
    }

    async fn previous(&self) -> Result<()> {
        self.exec(|p| p.previous());
        Ok(())
    }

    async fn seek(&self, position_secs: f64) -> Result<()> {
        let pos = Duration::from_secs_f64(position_secs.max(0.0));
        self.exec(move |p| p.seek(pos));
        Ok(())
    }

    async fn set_volume(&self, volume: f32) -> Result<()> {
        let v = volume.clamp(0.0, 1.0);
        self.exec(move |p| p.set_volume(v));
        Ok(())
    }

    async fn set_shuffle(&self, on: bool) -> Result<()> {
        self.exec(move |p| p.set_shuffle(on));
        self.persist();
        Ok(())
    }

    async fn set_repeat(&self, mode: RepeatMode) -> Result<()> {
        let rb = to_rb_repeat(mode);
        self.exec(move |p| p.set_repeat(rb));
        self.persist();
        Ok(())
    }

    async fn restore(&self, snapshot: PersistedQueue) -> Result<()> {
        let shuffle = snapshot.shuffle;
        let repeat = to_rb_repeat(snapshot.repeat);
        let seed_urls = urls(&snapshot.items);
        let seed_index = snapshot.current_index;
        self.mirror.lock().items = snapshot.items;

        self.exec(move |p| {
            // The engine restores the queue order + exact playhead from its
            // own resume file; we only need to reinstate shuffle/repeat.
            p.set_shuffle(shuffle);
            p.set_repeat(repeat);

            // `resume()` cues the engine's saved queue + position without
            // playing. If it has nothing (a fresh install, or a queue.json
            // left by an older fin with no engine resume file yet), fall back
            // to seeding the engine from the sidecar — the exact position is
            // lost on that one migration, but the queue is preserved.
            if p.resume().is_none() && !seed_urls.is_empty() {
                p.set_queue(seed_urls);
                if let Some(i) = seed_index {
                    if i > 0 {
                        p.skip_to(i);
                    }
                }
            }
        });
        Ok(())
    }

    async fn remove_from_queue(&self, index: usize) -> Result<()> {
        {
            let mut mirror = self.mirror.lock();
            if index < mirror.items.len() {
                mirror.items.remove(index);
            }
        }
        self.exec(move |p| p.remove(index));
        self.persist();
        Ok(())
    }

    async fn set_replaygain(&self, settings: ReplayGainSettings) -> Result<()> {
        let mode = to_rb_replaygain_mode(settings.mode);
        let preamp = settings.preamp_db;
        let clip = settings.prevent_clip;
        self.exec(move |p| p.set_replaygain(mode, preamp, clip));
        self.mirror.lock().replaygain = settings;
        Ok(())
    }

    async fn set_crossfade(&self, settings: CrossfadeSettings) -> Result<()> {
        let rb = to_rb_crossfade(settings);
        self.exec(move |p| p.set_crossfade(rb));
        self.mirror.lock().crossfade = settings;
        Ok(())
    }

    async fn set_eq(&self, enabled: bool, bands: Vec<EqBand>) -> Result<()> {
        let rb_bands: Vec<RbEqBand> = bands.iter().take(EQ_BANDS).map(to_rb_eq_band).collect();
        self.exec(move |p| {
            p.set_eq_enabled(enabled);
            for (i, band) in rb_bands.into_iter().enumerate() {
                p.set_eq_band(i, band);
            }
        });
        let mut mirror = self.mirror.lock();
        mirror.eq_enabled = enabled;
        mirror.eq_band_count = bands.len().min(EQ_BANDS);
        Ok(())
    }

    async fn set_tone(
        &self,
        bass_db: i32,
        treble_db: i32,
        bass_cutoff_hz: i32,
        treble_cutoff_hz: i32,
    ) -> Result<()> {
        self.exec(move |p| {
            p.set_tone(ToneControls {
                bass_db,
                treble_db,
                bass_cutoff_hz,
                treble_cutoff_hz,
            });
        });
        let mut mirror = self.mirror.lock();
        mirror.bass_db = bass_db;
        mirror.treble_db = treble_db;
        Ok(())
    }

    fn state(&self) -> PlaybackState {
        let mirror = self.mirror.lock();
        let base = PlaybackState {
            replaygain: mirror.replaygain,
            crossfade: mirror.crossfade,
            eq_enabled: mirror.eq_enabled,
            eq_band_count: mirror.eq_band_count,
            bass_db: mirror.bass_db,
            treble_db: mirror.treble_db,
            queue: mirror.items.clone(),
            ..PlaybackState::default()
        };

        let Some(st) = self.engine_status() else {
            return base;
        };

        let current_index = st.index.filter(|&i| i < mirror.items.len());
        let now_playing = current_index.and_then(|i| mirror.items.get(i).cloned());

        // The engine's duration is authoritative once the track is decoded;
        // before that, fall back to the server-provided length so the UI has
        // a sensible total to show.
        let engine_dur = st.duration.as_secs_f64();
        let duration_secs = if engine_dur > 0.0 {
            engine_dur
        } else {
            now_playing
                .as_ref()
                .and_then(|i| i.duration_secs)
                .map(|d| d as f64)
                .unwrap_or(0.0)
        };

        let status = match st.state {
            RbState::Playing => PlaybackStatus::Playing,
            RbState::Paused => PlaybackStatus::Paused,
            RbState::Stopped if mirror.items.is_empty() => PlaybackStatus::Idle,
            RbState::Stopped => PlaybackStatus::Stopped,
        };

        PlaybackState {
            status,
            position_secs: st.position.as_secs_f64(),
            duration_secs,
            volume: st.volume,
            now_playing,
            current_index,
            shuffle: st.shuffle,
            repeat: from_rb_repeat(st.repeat),
            ..base
        }
    }
}
