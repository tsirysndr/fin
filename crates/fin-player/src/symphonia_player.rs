//! Local audio-only renderer backed by symphonia + cpal.
//!
//! The stack is deliberately minimal: reqwest streams the HTTP body into an
//! in-memory buffer, symphonia probes the container and decodes packets, and
//! cpal owns the OS audio output. mpv is not involved anywhere in this path —
//! it is reserved for video by the sibling `LocalRenderer` dispatcher.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use parking_lot::{Condvar, Mutex};
use rb::{Producer, RbConsumer, RbInspector, RbProducer, SpscRb, RB};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::{Time, TimeBase};
use tracing::{debug, error, warn};

use fin_config::EqBand;
use rockbox_dsp::{
    eq_band_setting, Dsp, EQ_NUM_BANDS, REPLAYGAIN_ALBUM, REPLAYGAIN_OFF, REPLAYGAIN_TRACK,
};

use crate::voice_dsp::VoiceDsp;

/// Uniform "process interleaved stereo i16 → interleaved stereo i16"
/// interface so `decode_one_packet` can accept either the process-wide
/// audio DSP or the crossfade-only voice DSP without duplicating logic.
trait DspProcess {
    fn process_stereo(&mut self, input: &[i16], out: &mut Vec<i16>);
}

impl DspProcess for Dsp {
    fn process_stereo(&mut self, input: &[i16], out: &mut Vec<i16>) {
        self.process(input, out);
    }
}

impl DspProcess for VoiceDsp {
    fn process_stereo(&mut self, input: &[i16], out: &mut Vec<i16>) {
        self.process(input, out);
    }
}

use crate::crossfade::{fade_at, CrossfadeMode, CrossfadeSettings};
use crate::persist::{PersistedQueue, Persister};
use crate::queue::{PlaybackQueue, QueueItem};
use crate::renderer::{PlaybackState, PlaybackStatus, Renderer, RendererKind};
use crate::replaygain::{ReplayGainInfo, ReplayGainMode, ReplayGainSettings};

/// A local, audio-only renderer. Streams the HTTP body, decodes with symphonia,
/// and pushes float samples to the default cpal output device.
///
/// The renderer is fire-and-forget from the async side: every method sends a
/// command on a `std::sync::mpsc` channel to a dedicated worker thread that
/// owns the cpal stream and the symphonia decoder.
pub struct SymphoniaPlayer {
    queue: PlaybackQueue,
    state: Arc<Mutex<PlaybackState>>,
    cmd_tx: mpsc::Sender<PlayerCommand>,
    // Kept so the worker is joined on Drop.
    worker: Mutex<Option<JoinHandle<()>>>,
}

enum PlayerCommand {
    Play {
        items: Vec<QueueItem>,
        start_index: usize,
    },
    Enqueue(Vec<QueueItem>),
    PlayNext(Vec<QueueItem>),
    Pause,
    Resume,
    Stop,
    Next,
    Previous,
    Seek(f64),
    SetVolume(f32),
    SetShuffle(bool),
    SetRepeat(crate::queue::RepeatMode),
    /// Populate queue + shuffle/repeat + pending seek from an on-disk
    /// snapshot. Doesn't start playback — the next Resume/Play does.
    Restore(PersistedQueue),
    RemoveAt(usize),
    SetReplayGain(ReplayGainSettings),
    SetCrossfade(CrossfadeSettings),
    SetEq {
        enabled: bool,
        bands: Vec<EqBand>,
    },
    SetTone {
        bass_db: i32,
        treble_db: i32,
        bass_cutoff_hz: i32,
        treble_cutoff_hz: i32,
    },
    Quit,
}

impl SymphoniaPlayer {
    pub fn new() -> Self {
        Self::with_persist(None)
    }

    /// Same as `new`, but with a queue-persistence path — the worker will
    /// write a snapshot of the current queue + shuffle/repeat/position to
    /// `path` on every relevant event.
    pub fn with_persist(persist_path: Option<std::path::PathBuf>) -> Self {
        let queue = PlaybackQueue::new();
        let state = Arc::new(Mutex::new(PlaybackState::default()));
        let (cmd_tx, cmd_rx) = mpsc::channel::<PlayerCommand>();
        let persister = persist_path.map(Persister::spawn);

        let worker_state = state.clone();
        let worker_queue = queue.clone();
        let worker_persister = persister.clone();
        let worker = thread::Builder::new()
            .name("fin-symphonia".into())
            .spawn(move || {
                if let Err(e) = run_worker(cmd_rx, worker_state, worker_queue, worker_persister) {
                    error!(error = ?e, "symphonia worker exited");
                }
            })
            .expect("spawn symphonia worker thread");

        Self {
            queue,
            state,
            cmd_tx,
            worker: Mutex::new(Some(worker)),
        }
    }

    pub fn queue_handle(&self) -> PlaybackQueue {
        self.queue.clone()
    }

    fn send(&self, cmd: PlayerCommand) -> Result<()> {
        self.cmd_tx
            .send(cmd)
            .map_err(|_| anyhow!("symphonia worker channel closed"))
    }
}

impl Default for SymphoniaPlayer {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Renderer for SymphoniaPlayer {
    fn kind(&self) -> RendererKind {
        RendererKind::Mpv
    }

    async fn play(&self, items: Vec<QueueItem>, start_index: usize) -> Result<()> {
        self.send(PlayerCommand::Play { items, start_index })
    }

    async fn enqueue(&self, items: Vec<QueueItem>) -> Result<()> {
        self.send(PlayerCommand::Enqueue(items))
    }

    async fn play_next(&self, items: Vec<QueueItem>) -> Result<()> {
        self.send(PlayerCommand::PlayNext(items))
    }

    async fn pause(&self) -> Result<()> {
        self.send(PlayerCommand::Pause)
    }

    async fn resume(&self) -> Result<()> {
        self.send(PlayerCommand::Resume)
    }

    async fn stop(&self) -> Result<()> {
        self.send(PlayerCommand::Stop)
    }

    async fn next(&self) -> Result<()> {
        self.send(PlayerCommand::Next)
    }

    async fn previous(&self) -> Result<()> {
        self.send(PlayerCommand::Previous)
    }

    async fn seek(&self, position_secs: f64) -> Result<()> {
        self.send(PlayerCommand::Seek(position_secs))
    }

    async fn set_volume(&self, volume: f32) -> Result<()> {
        self.send(PlayerCommand::SetVolume(volume))
    }

    async fn set_shuffle(&self, on: bool) -> Result<()> {
        self.send(PlayerCommand::SetShuffle(on))
    }

    async fn set_repeat(&self, mode: crate::queue::RepeatMode) -> Result<()> {
        self.send(PlayerCommand::SetRepeat(mode))
    }

    async fn restore(&self, snapshot: PersistedQueue) -> Result<()> {
        self.send(PlayerCommand::Restore(snapshot))
    }

    async fn remove_from_queue(&self, index: usize) -> Result<()> {
        self.send(PlayerCommand::RemoveAt(index))
    }

    async fn set_replaygain(&self, settings: ReplayGainSettings) -> Result<()> {
        self.send(PlayerCommand::SetReplayGain(settings))
    }

    async fn set_crossfade(&self, settings: CrossfadeSettings) -> Result<()> {
        self.send(PlayerCommand::SetCrossfade(settings))
    }

    async fn set_eq(&self, enabled: bool, bands: Vec<EqBand>) -> Result<()> {
        self.send(PlayerCommand::SetEq { enabled, bands })
    }

    async fn set_tone(
        &self,
        bass_db: i32,
        treble_db: i32,
        bass_cutoff_hz: i32,
        treble_cutoff_hz: i32,
    ) -> Result<()> {
        self.send(PlayerCommand::SetTone {
            bass_db,
            treble_db,
            bass_cutoff_hz,
            treble_cutoff_hz,
        })
    }

    fn state(&self) -> PlaybackState {
        self.state.lock().clone()
    }
}

impl Drop for SymphoniaPlayer {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(PlayerCommand::Quit);
        if let Some(h) = self.worker.lock().take() {
            let _ = h.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming HTTP media source
// ---------------------------------------------------------------------------

/// Progressive, in-memory buffer of an HTTP response. Reads block until the
/// backing fetcher thread has enough bytes; if the fetcher has finished the
/// buffer behaves like `Cursor<&[u8]>`.
struct StreamingSource {
    buf: Arc<Mutex<SharedBuf>>,
    ready: Arc<Condvar>,
    pos: u64,
}

struct SharedBuf {
    data: Vec<u8>,
    done: bool,
    error: Option<String>,
    total: Option<u64>,
    /// Set by the worker thread to signal the fetcher to bail out — e.g. the
    /// user pressed Stop or moved to the next track before we're done.
    cancelled: bool,
}

impl Read for StreamingSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let mut guard = self.buf.lock();
        loop {
            let len = guard.data.len() as u64;
            if self.pos < len {
                let start = self.pos as usize;
                let n = std::cmp::min(out.len(), guard.data.len() - start);
                out[..n].copy_from_slice(&guard.data[start..start + n]);
                self.pos += n as u64;
                return Ok(n);
            }
            if guard.done {
                return Ok(0);
            }
            if let Some(err) = guard.error.clone() {
                return Err(io::Error::new(io::ErrorKind::Other, err));
            }
            if guard.cancelled {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            self.ready.wait(&mut guard);
        }
    }
}

impl Seek for StreamingSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::Current(n) => self.pos as i64 + n,
            SeekFrom::End(n) => {
                let guard = self.buf.lock();
                let len = if let Some(t) = guard.total {
                    t
                } else if guard.done {
                    guard.data.len() as u64
                } else {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "seek from end on live stream with unknown length",
                    ));
                };
                len as i64 + n
            }
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        self.pos = target as u64;
        Ok(self.pos)
    }
}

impl MediaSource for StreamingSource {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        self.buf.lock().total
    }
}

/// Spawn a background thread that downloads the URL body into a shared
/// buffer. The returned `StreamingSource` can be handed to symphonia
/// straight away — reads will block until enough data is available.
fn spawn_streaming_source(url: &str) -> Result<StreamingSource> {
    let buf = Arc::new(Mutex::new(SharedBuf {
        data: Vec::new(),
        done: false,
        error: None,
        total: None,
        cancelled: false,
    }));
    let ready = Arc::new(Condvar::new());

    let buf_c = buf.clone();
    let ready_c = ready.clone();
    let url = url.to_string();

    thread::Builder::new()
        .name("fin-fetch".into())
        .spawn(move || {
            let client = match reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    let mut g = buf_c.lock();
                    g.error = Some(format!("http client: {e}"));
                    g.done = true;
                    ready_c.notify_all();
                    return;
                }
            };
            let mut resp = match client.get(&url).send().and_then(|r| r.error_for_status()) {
                Ok(r) => r,
                Err(e) => {
                    let mut g = buf_c.lock();
                    g.error = Some(format!("fetch: {e}"));
                    g.done = true;
                    ready_c.notify_all();
                    return;
                }
            };
            if let Some(len) = resp.content_length() {
                buf_c.lock().total = Some(len);
            }
            let mut chunk = vec![0u8; 32 * 1024];
            loop {
                match resp.read(&mut chunk) {
                    Ok(0) => {
                        let mut g = buf_c.lock();
                        g.done = true;
                        ready_c.notify_all();
                        return;
                    }
                    Ok(n) => {
                        let mut g = buf_c.lock();
                        if g.cancelled {
                            g.done = true;
                            ready_c.notify_all();
                            return;
                        }
                        g.data.extend_from_slice(&chunk[..n]);
                        ready_c.notify_all();
                    }
                    Err(e) => {
                        let mut g = buf_c.lock();
                        g.error = Some(format!("read: {e}"));
                        g.done = true;
                        ready_c.notify_all();
                        return;
                    }
                }
            }
        })
        .context("spawn HTTP fetch thread")?;

    Ok(StreamingSource { buf, ready, pos: 0 })
}

// ---------------------------------------------------------------------------
// Worker thread — owns cpal stream + symphonia decoder
// ---------------------------------------------------------------------------

/// Everything the worker keeps alive while a track is playing.
struct Track {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    // Source (decoded) audio spec — what symphonia hands us. `source_channels`
    // lives on the Resampler; only `source_sr` is used for duration reporting.
    source_sr: u32,
    // Output (cpal) audio spec — what the OS device consumes.
    output_sr: u32,
    output_channels: usize,
    time_base: Option<TimeBase>,
    total_frames: Option<u64>,
    // OUTPUT frames pushed to the ring so far — the ring holds output-rate
    // samples, so this is what "position" is computed against.
    produced_output_frames: u64,
    // Backing samples buffer we reuse packet-to-packet.
    sample_buf: Option<SampleBuffer<f32>>,
    // Resamples + channel-converts source → output.
    resampler: Resampler,
    // Producer half of the ring buffer feeding the cpal callback.
    producer: Producer<f32>,
    // Kept so we can query occupancy (`Producer` alone can't).
    ring: Arc<SpscRb<f32>>,
    // Cancellation flag for the fetcher — flipped when we drop the track early.
    fetch_cancel: Arc<Mutex<SharedBuf>>,
    // Kept alive alongside the decoder — dropping this stops OS output.
    _stream: cpal::Stream,
    // Item metadata for state reporting.
    item: QueueItem,
    // Latest volume applied to the samples we push.
    volume: Arc<AtomicU32>,
    paused: Arc<AtomicBool>,
    // Instant we started the current track — used only as a tie-breaker in logs.
    _started: Instant,
    // ReplayGain tags read from the track. The gain itself is applied by
    // the Rockbox DSP's pre-gain (PGA) stage — these tags are handed to
    // the audio DSP config once this track becomes current (see
    // `rg_gains_pushed`).
    replaygain_info: ReplayGainInfo,
    // Fallback linear multiplier for samples the Rockbox PGA can't touch:
    // the crossfade-incoming path (routed through the voice DSP config,
    // which has no PGA stage), non-stereo output (DSP skipped entirely),
    // and the first primed packet. Recomputed on SetReplayGain.
    replaygain_linear: f32,
    // True once this track's RG tags have been pushed into the audio DSP.
    // Reset per-track so promotion / track changes re-push.
    rg_gains_pushed: bool,
    // True once the decoder has returned EndOfStream (no more packets).
    // Used by the crossfade path — we can't advance the queue on EOF
    // anymore because that's the promotion step's job.
    ended: bool,
    // Set when this track is the "incoming" side of a crossfade. The push
    // loop scales samples by fade_in(progress); progress is measured off
    // this track's own produced_output_frames counter (which starts at 0).
    // `Some(OverlapContext)` means "I'm fading in". None means "full gain".
    overlap_incoming: Option<OverlapContext>,
    // Same for the outgoing side. `start_frame` records
    // `produced_output_frames` at overlap start so we can compute
    // progress = (produced - start) / length.
    overlap_outgoing: Option<OverlapContext>,
}

/// Per-frame fade envelope parameters. Baked in `push_samples_with_volume`.
#[derive(Debug, Clone, Copy)]
struct OverlapContext {
    mode: CrossfadeMode,
    length_frames: u64,
    start_frame: u64,
}

impl Track {
    /// Output frames currently sitting in the ring, waiting to be played out.
    fn ring_pending_frames(&self) -> usize {
        let used = self.ring.count();
        used / self.output_channels.max(1)
    }

    fn ring_free_slots(&self) -> usize {
        self.ring.slots_free()
    }

    fn position_secs(&self) -> f64 {
        // Playhead: output frames consumed by cpal / output_sr = wall-clock seconds.
        let played = self
            .produced_output_frames
            .saturating_sub(self.ring_pending_frames() as u64);
        played as f64 / self.output_sr as f64
    }

    fn duration_secs(&self) -> f64 {
        if let (Some(tb), Some(n)) = (self.time_base, self.total_frames) {
            let t = tb.calc_time(n);
            t.seconds as f64 + t.frac
        } else if let Some(n) = self.total_frames {
            n as f64 / self.source_sr as f64
        } else {
            0.0
        }
    }
}

/// Linear-interpolating resampler + channel converter. Stateful so it can
/// stitch samples across packet boundaries without clicks.
struct Resampler {
    src_sr: u32,
    dst_sr: u32,
    src_ch: usize,
    dst_ch: usize,
    /// Fractional source-frame position of the next output frame we owe,
    /// relative to the START of the next input packet.
    src_pos_frac: f64,
    /// Last channel-converted source frame — kept so the first output frame
    /// of a new packet can interpolate against it.
    last_frame: Vec<f32>,
    have_last: bool,
}

impl Resampler {
    fn new(src_sr: u32, dst_sr: u32, src_ch: usize, dst_ch: usize) -> Self {
        Self {
            src_sr,
            dst_sr,
            src_ch,
            dst_ch,
            src_pos_frac: 0.0,
            last_frame: vec![0.0; dst_ch.max(1)],
            have_last: false,
        }
    }

    fn convert_channels(&self, input: &[f32]) -> Vec<f32> {
        if self.src_ch == self.dst_ch {
            return input.to_vec();
        }
        let n_frames = input.len() / self.src_ch.max(1);
        let mut out = Vec::with_capacity(n_frames * self.dst_ch);
        for f in 0..n_frames {
            for c in 0..self.dst_ch {
                let s = if self.src_ch == 2 && self.dst_ch == 1 {
                    (input[f * 2] + input[f * 2 + 1]) * 0.5
                } else if self.src_ch == 1 {
                    // Mono → broadcast to every output channel.
                    input[f]
                } else if c < self.src_ch {
                    // Take first N source channels, drop the rest.
                    input[f * self.src_ch + c]
                } else if self.src_ch >= 2 {
                    // Duplicate front L/R across missing outputs.
                    input[f * self.src_ch + (c % 2)]
                } else {
                    0.0
                };
                out.push(s);
            }
        }
        out
    }

    /// Resample the input (already channel-converted, interleaved at dst_ch)
    /// from src_sr to dst_sr. Returns interleaved output at dst_sr / dst_ch.
    fn resample(&mut self, input: Vec<f32>) -> Vec<f32> {
        if self.src_sr == self.dst_sr {
            if !input.is_empty() {
                let n = input.len() / self.dst_ch.max(1);
                for c in 0..self.dst_ch {
                    self.last_frame[c] = input[(n - 1) * self.dst_ch + c];
                }
                self.have_last = true;
            }
            return input;
        }
        let n_frames = input.len() / self.dst_ch.max(1);
        if n_frames == 0 {
            return Vec::new();
        }
        let ratio = self.src_sr as f64 / self.dst_sr as f64;
        let mut out = Vec::new();
        let mut src_pos = self.src_pos_frac;
        loop {
            let idx = src_pos.floor() as isize;
            let frac = (src_pos - src_pos.floor()) as f32;
            let next_idx = idx + 1;
            if next_idx < 0 || (next_idx as usize) >= n_frames {
                break;
            }
            for c in 0..self.dst_ch {
                let a = if idx < 0 {
                    if self.have_last {
                        self.last_frame[c]
                    } else {
                        0.0
                    }
                } else {
                    input[(idx as usize) * self.dst_ch + c]
                };
                let b = input[(next_idx as usize) * self.dst_ch + c];
                out.push(a * (1.0 - frac) + b * frac);
            }
            src_pos += ratio;
        }
        // Save state for the next packet.
        self.src_pos_frac = src_pos - n_frames as f64;
        for c in 0..self.dst_ch {
            self.last_frame[c] = input[(n_frames - 1) * self.dst_ch + c];
        }
        self.have_last = true;
        out
    }

    fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let ch_converted = self.convert_channels(input);
        self.resample(ch_converted)
    }
}

fn run_worker(
    rx: mpsc::Receiver<PlayerCommand>,
    state: Arc<Mutex<PlaybackState>>,
    queue: PlaybackQueue,
    persister: Option<Persister>,
) -> Result<()> {
    let host = cpal::default_host();
    let volume = Arc::new(AtomicU32::new(f32::to_bits(1.0)));
    let paused = Arc::new(AtomicBool::new(false));

    let mut track: Option<Track> = None;
    // Playhead position we still owe to a freshly-loaded track, e.g. from a
    // restored queue. Consumed the first time we load a track after being
    // set, then cleared.
    let mut pending_seek: Option<f64> = None;
    // Rate-limit position-only persist writes.
    let mut last_position_persist = Instant::now();
    // Current ReplayGain settings — applied to each track we load.
    let mut rg_settings = ReplayGainSettings::default();
    let mut xf_settings = CrossfadeSettings::default();
    // Rockbox DSP pipeline for the CURRENT track only. Initialised lazily
    // on the first track load (we need the device output rate). The
    // instance is a process-wide singleton (`CODEC_IDX_AUDIO`), so we
    // never own two at once — the crossfade `next` track bypasses it and
    // uses the plain resampler. Besides EQ/tone this config also owns the
    // PGA stage, which applies ReplayGain in fixed point.
    let mut dsp: Option<Dsp> = None;
    // Second DSP instance bound to Rockbox's `CODEC_IDX_VOICE`. Rockbox
    // shares EQ / tone coefficients across configs but keeps biquad delay
    // lines per-config — so running the crossfade next track through here
    // gives it the same filter curve as `dsp` without churning `dsp`'s
    // delay lines (the source of the "small noises" during overlap).
    let mut voice_dsp: Option<VoiceDsp> = None;
    let mut eq_enabled = false;
    let mut eq_bands: Vec<EqBand> = Vec::new();
    // Tone (bass/treble) shelf gains in whole dB, plus optional cutoff
    // overrides in Hz (0 = Rockbox defaults). Applied to the Dsp singleton
    // like EQ — set once on init, updated live via SetTone.
    let mut tone_bass_db: i32 = 0;
    let mut tone_treble_db: i32 = 0;
    let mut tone_bass_cutoff_hz: i32 = 0;
    let mut tone_treble_cutoff_hz: i32 = 0;
    // The "incoming" track being preloaded during a crossfade overlap.
    // `Some` only during an active overlap. When the fade-in completes,
    // this promotes to `track` and the previous `track` is dropped.
    let mut next: Option<Track> = None;
    // Whether the pending promotion should also `queue.advance()`. True for
    // end-of-track preloads (the next-in-queue item was peeked but not yet
    // committed). False for user-triggered Play/jump-to-queue crossfades,
    // where `queue.replace()` has already positioned current_index at the
    // incoming track — advancing again would shift the now-playing marker
    // one row past the actual playing track.
    let mut promote_advances_queue: bool = false;

    'main: loop {
        // Consume commands. If nothing's playing, block; otherwise poll.
        let cmd = if track.is_some() {
            match rx.try_recv() {
                Ok(c) => Some(c),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => break 'main,
            }
        } else {
            match rx.recv() {
                Ok(c) => Some(c),
                Err(_) => break 'main,
            }
        };

        if let Some(cmd) = cmd {
            match cmd {
                PlayerCommand::Play { items, start_index } => {
                    // If crossfade is active and something's playing, load
                    // the new track as an incoming next and fade the old
                    // one out — same feel as end-of-track crossfade.
                    let can_crossfade = xf_settings.mode.is_active()
                        && track
                            .as_ref()
                            .map(|t| !t.ended && !t.paused.load(Ordering::Relaxed))
                            .unwrap_or(false);
                    if let Some(nt) = next.take() {
                        drop(nt);
                    }
                    queue.replace(items, start_index);
                    sync_queue_meta(&state, &queue);
                    paused.store(false, Ordering::Relaxed);
                    pending_seek = None;

                    match (can_crossfade, queue.current()) {
                        (true, Some(item)) => {
                            match load_track(
                                &host,
                                item.clone(),
                                volume.clone(),
                                paused.clone(),
                                rg_settings,
                            ) {
                                Ok(mut nt) => {
                                    let length_frames = (xf_settings.duration_secs as f64
                                        * nt.output_sr as f64)
                                        as u64;
                                    let mode = xf_settings.mode;
                                    let ct = track.as_mut().unwrap();
                                    nt.overlap_incoming = Some(OverlapContext {
                                        mode,
                                        length_frames,
                                        start_frame: 0,
                                    });
                                    ct.overlap_outgoing = Some(OverlapContext {
                                        mode,
                                        length_frames,
                                        start_frame: ct.produced_output_frames,
                                    });
                                    debug!(
                                        mode = ?mode,
                                        length_frames,
                                        title = %item.title,
                                        "crossfade on user Play"
                                    );
                                    // queue.replace(...) already positioned
                                    // current_index at the incoming track,
                                    // so promotion must NOT advance it.
                                    promote_advances_queue = false;
                                    if let Some(vd) = voice_dsp.as_mut() {
                                        vd.flush();
                                    }
                                    next = Some(nt);
                                }
                                Err(e) => {
                                    warn!(?e, "crossfade Play preload failed; hard-cutting");
                                    stop_current(&mut track);
                                    track = load_and_maybe_seek(
                                        &host,
                                        item,
                                        &volume,
                                        &paused,
                                        &state,
                                        &queue,
                                        &mut pending_seek,
                                        rg_settings,
                                    );
                                }
                            }
                        }
                        (false, Some(item)) => {
                            stop_current(&mut track);
                            track = load_and_maybe_seek(
                                &host,
                                item,
                                &volume,
                                &paused,
                                &state,
                                &queue,
                                &mut pending_seek,
                                rg_settings,
                            );
                        }
                        (_, None) => {
                            stop_current(&mut track);
                            mark_idle(&state);
                        }
                    }
                    persist_now(&persister, &queue, &state);
                }
                PlayerCommand::Enqueue(items) => {
                    let was_empty = queue.is_empty();
                    queue.append(items);
                    sync_queue_meta(&state, &queue);
                    if was_empty && track.is_none() {
                        if let Some(item) = queue.current() {
                            track = load_and_maybe_seek(
                                &host,
                                item,
                                &volume,
                                &paused,
                                &state,
                                &queue,
                                &mut pending_seek,
                                rg_settings,
                            );
                        }
                    }
                    persist_now(&persister, &queue, &state);
                }
                PlayerCommand::PlayNext(items) => {
                    let was_empty = queue.is_empty();
                    queue.insert_next(items);
                    sync_queue_meta(&state, &queue);
                    if was_empty && track.is_none() {
                        if let Some(item) = queue.current() {
                            track = load_and_maybe_seek(
                                &host,
                                item,
                                &volume,
                                &paused,
                                &state,
                                &queue,
                                &mut pending_seek,
                                rg_settings,
                            );
                        }
                    }
                    persist_now(&persister, &queue, &state);
                }
                PlayerCommand::Pause => {
                    paused.store(true, Ordering::Relaxed);
                    let mut s = state.lock();
                    if s.now_playing.is_some() {
                        s.status = PlaybackStatus::Paused;
                    }
                    drop(s);
                    persist_now(&persister, &queue, &state);
                }
                PlayerCommand::Resume => {
                    paused.store(false, Ordering::Relaxed);
                    // If we're idle but the queue has a current item (e.g.
                    // just after a restore), start playing it now. Any
                    // pending_seek from restore is applied on load.
                    if track.is_none() {
                        if let Some(item) = queue.current() {
                            track = load_and_maybe_seek(
                                &host,
                                item,
                                &volume,
                                &paused,
                                &state,
                                &queue,
                                &mut pending_seek,
                                rg_settings,
                            );
                        }
                    }
                    let mut s = state.lock();
                    if s.now_playing.is_some() {
                        s.status = PlaybackStatus::Playing;
                    }
                }
                PlayerCommand::Stop => {
                    stop_current(&mut track);
                    if let Some(nt) = next.take() {
                        drop(nt);
                    }
                    queue.clear();
                    pending_seek = None;
                    let mut s = state.lock();
                    s.queue.clear();
                    s.current_index = None;
                    s.now_playing = None;
                    s.status = PlaybackStatus::Stopped;
                    s.position_secs = 0.0;
                    s.duration_secs = 0.0;
                    drop(s);
                    persist_now(&persister, &queue, &state);
                }
                PlayerCommand::Next => {
                    // If a crossfade is already ramping in, snap-promote it
                    // instead of loading fresh. Feels responsive, and it
                    // preserves the buffered next-track audio.
                    stop_current(&mut track);
                    if let Some(mut nt) = next.take() {
                        nt.overlap_incoming = None;
                        nt.overlap_outgoing = None;
                        track = Some(nt);
                        // Only advance the queue for end-approach overlaps
                        // — Play-based overlaps already have current_index
                        // pointing at the incoming track.
                        if promote_advances_queue {
                            queue.advance();
                        }
                        promote_advances_queue = false;
                        sync_queue_meta(&state, &queue);
                        persist_now(&persister, &queue, &state);
                        continue;
                    }
                    queue.advance();
                    sync_queue_meta(&state, &queue);
                    if let Some(item) = queue.current() {
                        track = load_and_maybe_seek(
                            &host,
                            item,
                            &volume,
                            &paused,
                            &state,
                            &queue,
                            &mut pending_seek,
                            rg_settings,
                        );
                    } else {
                        mark_idle(&state);
                    }
                    persist_now(&persister, &queue, &state);
                }
                PlayerCommand::Previous => {
                    stop_current(&mut track);
                    if let Some(nt) = next.take() {
                        drop(nt);
                    }
                    queue.back();
                    sync_queue_meta(&state, &queue);
                    if let Some(item) = queue.current() {
                        track = load_and_maybe_seek(
                            &host,
                            item,
                            &volume,
                            &paused,
                            &state,
                            &queue,
                            &mut pending_seek,
                            rg_settings,
                        );
                    } else {
                        mark_idle(&state);
                    }
                    persist_now(&persister, &queue, &state);
                }
                PlayerCommand::Seek(pos_secs) => {
                    if let Some(ref mut t) = track {
                        seek_track(t, pos_secs);
                    } else {
                        // No track yet — record for the next load (used after Restore).
                        pending_seek = Some(pos_secs.max(0.0));
                    }
                    persist_now(&persister, &queue, &state);
                }
                PlayerCommand::SetVolume(v) => {
                    let clamped = v.clamp(0.0, 1.5);
                    volume.store(f32::to_bits(clamped), Ordering::Relaxed);
                    state.lock().volume = clamped;
                }
                PlayerCommand::SetShuffle(on) => {
                    queue.set_shuffle(on);
                    sync_queue_meta(&state, &queue);
                    persist_now(&persister, &queue, &state);
                }
                PlayerCommand::SetRepeat(mode) => {
                    queue.set_repeat(mode);
                    sync_queue_meta(&state, &queue);
                    persist_now(&persister, &queue, &state);
                }
                PlayerCommand::Restore(snapshot) => {
                    stop_current(&mut track);
                    // Filter to audio-only — video items in a saved queue are
                    // legitimate but the SymphoniaPlayer can't play them. The
                    // LocalRenderer's dispatcher already partitions on load.
                    let audio_items: Vec<QueueItem> = snapshot
                        .items
                        .iter()
                        .filter(|i| !i.is_video)
                        .cloned()
                        .collect();
                    if !audio_items.is_empty() {
                        // Remap the saved index into the filtered list.
                        let target_id = snapshot
                            .current_index
                            .and_then(|i| snapshot.items.get(i))
                            .map(|it| it.id.clone());
                        let new_idx = target_id
                            .and_then(|id| audio_items.iter().position(|it| it.id == id))
                            .unwrap_or(0);
                        queue.replace(audio_items, new_idx);
                    }
                    queue.set_shuffle(snapshot.shuffle);
                    queue.set_repeat(snapshot.repeat);
                    pending_seek = if snapshot.position_secs > 0.0 {
                        Some(snapshot.position_secs)
                    } else {
                        None
                    };
                    sync_queue_meta(&state, &queue);
                    // Restore leaves playback paused so the user hits Space
                    // when they're ready. The pending_seek is applied to
                    // whichever track loads next.
                    paused.store(true, Ordering::Relaxed);
                    let mut s = state.lock();
                    s.status = PlaybackStatus::Paused;
                    s.position_secs = snapshot.position_secs;
                    drop(s);
                    // Do NOT overwrite the snapshot on restore itself.
                }
                PlayerCommand::SetReplayGain(settings) => {
                    rg_settings = settings;
                    // The DSP stashes per-track gains internally, so a
                    // settings change alone recomputes the pre-gain — no
                    // need to re-push the current track's tags.
                    if let Some(ref mut d) = dsp {
                        apply_replaygain_to_dsp(d, settings);
                    }
                    // Refresh the f32 fallback multipliers (crossfade
                    // incoming / non-stereo paths).
                    if let Some(ref mut t) = track {
                        t.replaygain_linear = t.replaygain_info.linear_gain(settings);
                    }
                    if let Some(ref mut nt) = next {
                        nt.replaygain_linear = nt.replaygain_info.linear_gain(settings);
                    }
                    state.lock().replaygain = settings;
                    persist_now(&persister, &queue, &state);
                }
                PlayerCommand::SetEq { enabled, bands } => {
                    eq_enabled = enabled;
                    eq_bands = bands;
                    if let Some(ref mut d) = dsp {
                        apply_eq_to_dsp(d, eq_enabled, &eq_bands);
                    }
                    {
                        let mut s = state.lock();
                        s.eq_enabled = eq_enabled;
                        s.eq_band_count = eq_bands.len().min(EQ_NUM_BANDS);
                    }
                }
                PlayerCommand::SetTone {
                    bass_db,
                    treble_db,
                    bass_cutoff_hz,
                    treble_cutoff_hz,
                } => {
                    tone_bass_db = bass_db;
                    tone_treble_db = treble_db;
                    tone_bass_cutoff_hz = bass_cutoff_hz;
                    tone_treble_cutoff_hz = treble_cutoff_hz;
                    if let Some(ref mut d) = dsp {
                        apply_tone_to_dsp(
                            d,
                            tone_bass_db,
                            tone_treble_db,
                            tone_bass_cutoff_hz,
                            tone_treble_cutoff_hz,
                        );
                    }
                    let mut s = state.lock();
                    s.bass_db = bass_db;
                    s.treble_db = treble_db;
                }
                PlayerCommand::SetCrossfade(settings) => {
                    xf_settings = settings;
                    // If the mode was turned OFF mid-overlap, kill the
                    // pending next track so we stop mixing. The current
                    // track keeps playing normally.
                    if !settings.mode.is_active() {
                        if let Some(nt) = next.take() {
                            drop(nt);
                        }
                        if let Some(ref mut t) = track {
                            t.overlap_outgoing = None;
                            t.overlap_incoming = None;
                        }
                    }
                    state.lock().crossfade = settings;
                }
                PlayerCommand::RemoveAt(idx) => {
                    let cur = queue.current_index();
                    let removing_current = cur == Some(idx);
                    queue.remove(idx);
                    sync_queue_meta(&state, &queue);
                    if removing_current {
                        // Was playing this exact track — stop it and load
                        // whatever the new current is, or go idle.
                        stop_current(&mut track);
                        pending_seek = None;
                        if let Some(item) = queue.current() {
                            track = load_and_maybe_seek(
                                &host,
                                item,
                                &volume,
                                &paused,
                                &state,
                                &queue,
                                &mut pending_seek,
                                rg_settings,
                            );
                        } else {
                            mark_idle(&state);
                        }
                    }
                    persist_now(&persister, &queue, &state);
                }
                PlayerCommand::Quit => break 'main,
            }
        }

        // Once every ~3 s while playing, checkpoint the current position so
        // a crash / kill leaves us close to where we were.
        if track.is_some()
            && !paused.load(Ordering::Relaxed)
            && last_position_persist.elapsed() >= Duration::from_secs(3)
        {
            persist_now(&persister, &queue, &state);
            last_position_persist = Instant::now();
        }

        // Decode current — same shape as before, but doesn't advance on
        // EOF anymore. That's now the promotion step's job (below).
        if let Some(ref mut t) = track {
            if t.paused.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(25));
                update_position(&state, t);
                continue;
            }

            // Lazy-init the Dsp singleton on the first track load — we need
            // the device output rate, which comes from the track itself.
            if dsp.is_none() {
                let mut d = Dsp::new(t.output_sr);
                // My Resampler already delivers samples at the device rate,
                // so keep Dsp's input rate = output rate — no internal
                // resample, just the EQ / tone stack.
                d.set_input_frequency(t.output_sr);
                apply_eq_to_dsp(&mut d, eq_enabled, &eq_bands);
                apply_tone_to_dsp(
                    &mut d,
                    tone_bass_db,
                    tone_treble_db,
                    tone_bass_cutoff_hz,
                    tone_treble_cutoff_hz,
                );
                apply_replaygain_to_dsp(&mut d, rg_settings);
                dsp = Some(d);
                // Spin up the second config now too — cheap, and it means
                // the crossfade preload path never blocks on init.
                if voice_dsp.is_none() {
                    let mut vd = VoiceDsp::new(t.output_sr);
                    vd.set_input_frequency(t.output_sr);
                    voice_dsp = Some(vd);
                }
            }

            // Hand this track's RG tags to the audio DSP's PGA stage the
            // first time it decodes as current — covers fresh loads and
            // crossfade promotions alike. Pushed even with RG off so a
            // later mode toggle picks them up without a reload.
            if !t.rg_gains_pushed {
                if let Some(ref mut d) = dsp {
                    apply_replaygain_gains_to_dsp(d, &t.replaygain_info);
                    t.rg_gains_pushed = true;
                }
            }

            let free = t.ring_free_slots();
            if free >= 8192 && !t.ended {
                // Current track always uses the AUDIO Rockbox config; the
                // next track (during crossfade) uses the VOICE config
                // below. Coefficients are global, so the EQ curve matches;
                // only the biquad delay lines differ. Skip both configs
                // when no stage is doing anything to avoid the f32↔i16
                // conversion hops. ReplayGain counts as a stage here — it
                // runs in the audio config's PGA.
                let dsp_active = eq_enabled
                    || tone_bass_db != 0
                    || tone_treble_db != 0
                    || rg_settings.mode.is_active();
                let dsp_arg: Option<&mut dyn DspProcess> = if dsp_active {
                    dsp.as_mut().map(|d| d as &mut dyn DspProcess)
                } else {
                    None
                };
                match decode_one_packet(t, dsp_arg, true) {
                    DecodeStep::Pushed => update_position(&state, t),
                    DecodeStep::EndOfStream => {
                        debug!(item = %t.item.title, "current track ended");
                        t.ended = true;
                    }
                    DecodeStep::Error(e) => {
                        warn!(error = %e, item = %t.item.title, "current decode error");
                        t.ended = true;
                    }
                }
            }
        }

        // Decode next in parallel — it has its own ring buffer, so no
        // backpressure conflict with current. During crossfade the next
        // track routes through the dedicated VOICE DSP config so the
        // AUDIO config's biquad delay lines are undisturbed — that's what
        // caused the "small noises" during overlap in the previous
        // shared-DSP version. Coefficients are still global, so the EQ
        // curve is identical on both sides. The voice config has NO PGA
        // stage (Rockbox hardwires ReplayGain to the audio config), so
        // this side gets its RG via the f32 fallback multiplier instead
        // — `rg_in_dsp = false` below.
        if let Some(ref mut nt) = next {
            let free = nt.ring_free_slots();
            if free >= 8192 && !nt.ended {
                let dsp_active = eq_enabled || tone_bass_db != 0 || tone_treble_db != 0;
                let dsp_arg: Option<&mut dyn DspProcess> = if dsp_active {
                    voice_dsp.as_mut().map(|d| d as &mut dyn DspProcess)
                } else {
                    None
                };
                match decode_one_packet(nt, dsp_arg, false) {
                    DecodeStep::Pushed => {}
                    DecodeStep::EndOfStream => nt.ended = true,
                    DecodeStep::Error(e) => {
                        warn!(error = %e, "crossfade next decode error");
                        nt.ended = true;
                    }
                }
            }
        }

        // Should we trigger a crossfade preload? Only when:
        // - a mode is selected
        // - we don't already have a next track
        // - the current track has a known duration
        // - the remaining decoder-time is within the overlap window
        if next.is_none() && xf_settings.mode.is_active() {
            if let Some(ref mut ct) = track {
                let duration = ct.duration_secs();
                let decoded_secs = ct.produced_output_frames as f64 / ct.output_sr as f64;
                let remaining = duration - decoded_secs;
                if duration > 0.0 && remaining <= xf_settings.duration_secs as f64 && !ct.ended {
                    if let Some(next_item) = queue.peek_next_item() {
                        // Attempt to load next. On failure we silently skip
                        // crossfade for this transition; the current track's
                        // natural EOF path will handle end-of-stream.
                        match load_track(
                            &host,
                            next_item.clone(),
                            volume.clone(),
                            paused.clone(),
                            rg_settings,
                        ) {
                            Ok(mut nt) => {
                                let length_frames =
                                    (xf_settings.duration_secs as f64 * nt.output_sr as f64) as u64;
                                let ctx = OverlapContext {
                                    mode: xf_settings.mode,
                                    length_frames,
                                    start_frame: 0,
                                };
                                nt.overlap_incoming = Some(ctx);
                                ct.overlap_outgoing = Some(OverlapContext {
                                    mode: xf_settings.mode,
                                    length_frames,
                                    start_frame: ct.produced_output_frames,
                                });
                                debug!(
                                    mode = ?xf_settings.mode,
                                    length_frames,
                                    next = %next_item.title,
                                    "crossfade triggered"
                                );
                                // End-approach preload — the peeked item is
                                // NOT yet the queue's current, so promotion
                                // must advance.
                                promote_advances_queue = true;
                                // Fresh biquad state for the incoming track
                                // — no delay-line pollution from a prior
                                // preload that never got promoted.
                                if let Some(vd) = voice_dsp.as_mut() {
                                    vd.flush();
                                }
                                next = Some(nt);
                            }
                            Err(e) => {
                                warn!(?e, title = %next_item.title, "crossfade preload failed");
                            }
                        }
                    }
                }
            }
        }

        // Promote next → current when the fade-in has completed OR when the
        // outgoing track's fade-out has run its full length.
        let should_promote = match (&track, &next) {
            (Some(ct), Some(nt)) => {
                let nt_progress_done = nt
                    .overlap_incoming
                    .map(|ctx| nt.produced_output_frames >= ctx.length_frames)
                    .unwrap_or(false);
                let ct_faded_out = ct
                    .overlap_outgoing
                    .map(|ctx| ct.produced_output_frames >= ctx.start_frame + ctx.length_frames)
                    .unwrap_or(false);
                nt_progress_done || ct_faded_out || ct.ended
            }
            _ => false,
        };
        if should_promote {
            debug!(
                advance = promote_advances_queue,
                "crossfade complete, promoting next → current"
            );
            stop_current(&mut track);
            if let Some(mut nt) = next.take() {
                nt.overlap_incoming = None;
                nt.overlap_outgoing = None;
                track = Some(nt);
                if promote_advances_queue {
                    queue.advance();
                }
                sync_queue_meta(&state, &queue);
                persist_now(&persister, &queue, &state);
                // The AUDIO DSP was tuned to the OUTGOING track's samples.
                // The newly-promoted track will now route through it, so
                // reset both configs' delay lines to zero — cheaper and
                // cleaner than letting stale state resonate through the
                // first packet or two.
                if let Some(d) = dsp.as_mut() {
                    d.flush();
                }
                if let Some(vd) = voice_dsp.as_mut() {
                    vd.flush();
                }
            }
            promote_advances_queue = false;
        }

        // Current ended and there's no next to promote — advance the queue
        // and load the next item straight (no crossfade).
        let current_dry = track
            .as_ref()
            .map(|t| t.ended && t.ring_pending_frames() == 0)
            .unwrap_or(false);
        if current_dry && next.is_none() {
            debug!("current track drained, advancing queue");
            stop_current(&mut track);
            queue.advance();
            sync_queue_meta(&state, &queue);
            if let Some(item) = queue.current() {
                track = load_and_maybe_seek(
                    &host,
                    item,
                    &volume,
                    &paused,
                    &state,
                    &queue,
                    &mut pending_seek,
                    rg_settings,
                );
            } else {
                mark_idle(&state);
            }
            persist_now(&persister, &queue, &state);
        }

        // If we didn't decode anything this tick, take a short nap to
        // avoid pegging the CPU.
        let idle = track.as_ref().map(|t| t.ended).unwrap_or(true)
            || track
                .as_ref()
                .map(|t| t.ring_free_slots() < 8192)
                .unwrap_or(true);
        if idle {
            thread::sleep(Duration::from_millis(5));
        }
    }

    stop_current(&mut track);
    // Flush one final snapshot so the exit position is persisted.
    persist_now(&persister, &queue, &state);
    Ok(())
}

enum DecodeStep {
    Pushed,
    EndOfStream,
    Error(String),
}

/// Decode one packet, run it through the given DSP config (if any) and
/// push it to the ring. `rg_in_dsp` says whether that config applies
/// ReplayGain in its PGA stage (true only for the AUDIO config) — when it
/// does, the f32 fallback multiplier is skipped so gain isn't applied
/// twice.
fn decode_one_packet(
    t: &mut Track,
    dsp: Option<&mut dyn DspProcess>,
    rg_in_dsp: bool,
) -> DecodeStep {
    let packet = match t.format.next_packet() {
        Ok(p) => p,
        Err(symphonia::core::errors::Error::IoError(e))
            if e.kind() == io::ErrorKind::UnexpectedEof =>
        {
            return DecodeStep::EndOfStream;
        }
        Err(symphonia::core::errors::Error::ResetRequired) => {
            return DecodeStep::EndOfStream;
        }
        Err(e) => return DecodeStep::Error(e.to_string()),
    };
    if packet.track_id() != t.track_id {
        return DecodeStep::Pushed;
    }
    let audio_buf = match t.decoder.decode(&packet) {
        Ok(b) => b,
        Err(symphonia::core::errors::Error::DecodeError(_)) => return DecodeStep::Pushed,
        Err(e) => return DecodeStep::Error(e.to_string()),
    };
    let spec = *audio_buf.spec();
    let cap = audio_buf.capacity() as u64;
    let sample_buf = t
        .sample_buf
        .get_or_insert_with(|| SampleBuffer::<f32>::new(cap, spec));
    sample_buf.copy_interleaved_ref(audio_buf);
    let src_samples = sample_buf.samples();
    // Convert channels + sample rate to what cpal is actually consuming.
    let mut out_samples = t.resampler.process(src_samples);
    if out_samples.is_empty() {
        return DecodeStep::Pushed;
    }
    // Rockbox DSP post-processing — Dsp only handles interleaved stereo,
    // so for non-stereo output we skip it (and fall back to the f32
    // ReplayGain multiplier below).
    let mut rg_mult = t.replaygain_linear;
    if let Some(d) = dsp {
        if t.output_channels == 2 {
            out_samples = apply_dsp(d, &out_samples);
            if rg_in_dsp {
                // The audio config's PGA stage already applied ReplayGain
                // in fixed point.
                rg_mult = 1.0;
            }
            if out_samples.is_empty() {
                return DecodeStep::Pushed;
            }
        }
    }
    let out_frames = out_samples.len() / t.output_channels.max(1);
    push_samples_with_volume(t, &out_samples, rg_mult);
    t.produced_output_frames += out_frames as u64;
    DecodeStep::Pushed
}

/// Route interleaved-stereo f32 samples through a Rockbox DSP pipeline
/// with input rate == output rate (no internal resample; my Resampler
/// already ran). Loudness scales aside, this is just the EQ + tone stack.
fn apply_dsp(dsp: &mut dyn DspProcess, samples: &[f32]) -> Vec<f32> {
    // f32 → i16, saturating.
    let mut input = Vec::with_capacity(samples.len());
    for &s in samples {
        let v = (s * 32767.0)
            .round()
            .clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        input.push(v);
    }
    let mut out_i16: Vec<i16> = Vec::with_capacity(input.len());
    dsp.process_stereo(&input, &mut out_i16);
    // i16 → f32.
    out_i16.iter().map(|&s| s as f32 / 32768.0).collect()
}

/// Apply bass/treble shelf gains + cutoffs to the Dsp singleton. Cutoffs
/// of `0` fall back to Rockbox defaults (200 Hz bass, 3500 Hz treble).
/// The Dsp wrapper multiplies dB values by 10 internally.
fn apply_tone_to_dsp(
    dsp: &mut Dsp,
    bass_db: i32,
    treble_db: i32,
    bass_cutoff_hz: i32,
    treble_cutoff_hz: i32,
) {
    // Cutoffs must be set BEFORE gains — Dsp::set_tone runs the prescale
    // which recomputes filter coefficients from whatever cutoff is active.
    dsp.set_tone_cutoffs(bass_cutoff_hz, treble_cutoff_hz);
    dsp.set_tone(bass_db, treble_db);
}

/// Map fin's ReplayGain settings onto the Rockbox PGA stage: mode, clip
/// prevention, preamp. The per-track gains are pushed separately via
/// [`apply_replaygain_gains_to_dsp`]; the DSP stashes both and recomputes
/// the pre-gain whenever either changes.
fn apply_replaygain_to_dsp(dsp: &mut Dsp, settings: ReplayGainSettings) {
    let mode = match settings.mode {
        ReplayGainMode::Off => REPLAYGAIN_OFF,
        ReplayGainMode::Track => REPLAYGAIN_TRACK,
        ReplayGainMode::Album => REPLAYGAIN_ALBUM,
    };
    dsp.set_replaygain(mode, settings.prevent_clip, settings.preamp_db);
}

/// Hand a track's RG tags to the audio DSP config. Rockbox falls back
/// track ↔ album internally when the requested scope's tag is absent,
/// matching the old fin behavior.
fn apply_replaygain_gains_to_dsp(dsp: &mut Dsp, info: &ReplayGainInfo) {
    dsp.set_replaygain_gains(
        info.track_gain_db,
        info.album_gain_db,
        info.track_peak,
        info.album_peak,
    );
}

/// Apply an on/off toggle plus the current bands to the Dsp singleton.
/// Truncates to EQ_NUM_BANDS silently.
fn apply_eq_to_dsp(dsp: &mut Dsp, enabled: bool, bands: &[EqBand]) {
    for (i, band) in bands.iter().take(EQ_NUM_BANDS).enumerate() {
        dsp.set_eq_band_raw(
            i,
            eq_band_setting {
                cutoff: band.cutoff,
                q: band.q,
                gain: band.gain,
            },
        );
    }
    dsp.eq_enable(enabled);
}

fn push_samples_with_volume(t: &Track, samples: &[f32], rg_mult: f32) {
    // Effective scale = user volume × ReplayGain fallback multiplier ×
    // per-frame fade envelope. `rg_mult` is 1.0 when the Rockbox PGA
    // stage already gained these samples; otherwise it's the track's f32
    // fallback. Volume + RG are constant across the batch; the fade
    // multiplier walks per output frame using this track's own
    // produced_output_frames counter.
    let base = f32::from_bits(t.volume.load(Ordering::Relaxed)) * rg_mult;
    let ch = t.output_channels.max(1);
    let mut offset = 0;
    while offset < samples.len() {
        if t.paused.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(20));
            continue;
        }
        let end = std::cmp::min(offset + 4096, samples.len());
        let src = &samples[offset..end];
        let chunk: Vec<f32> = if t.overlap_outgoing.is_none() && t.overlap_incoming.is_none() {
            // Fast path: no fade active.
            src.iter().map(|s| s * base).collect()
        } else {
            let frames_in_chunk = src.len() / ch;
            // frame index in the OUTPUT-frame timeline this chunk starts at.
            let start_frame = t.produced_output_frames + (offset / ch) as u64;
            let mut out = Vec::with_capacity(src.len());
            for f in 0..frames_in_chunk {
                let global = start_frame + f as u64;
                let fade_mult = fade_multiplier(t, global);
                let mul = base * fade_mult;
                for c in 0..ch {
                    out.push(src[f * ch + c] * mul);
                }
            }
            out
        };
        match t.producer.write(&chunk) {
            Ok(n) => offset += n,
            Err(_) => thread::sleep(Duration::from_millis(5)),
        }
    }
}

/// Compute the fade multiplier this track should apply at output-frame
/// `global`. Combines both directions when set — a track shouldn't normally
/// have both simultaneously, but if it does they compose multiplicatively.
fn fade_multiplier(t: &Track, global: u64) -> f32 {
    let mut m = 1.0;
    if let Some(ctx) = t.overlap_outgoing {
        let relative = global.saturating_sub(ctx.start_frame);
        let progress = relative as f32 / ctx.length_frames.max(1) as f32;
        m *= fade_at(ctx.mode, progress).out;
    }
    if let Some(ctx) = t.overlap_incoming {
        let relative = global.saturating_sub(ctx.start_frame);
        let progress = relative as f32 / ctx.length_frames.max(1) as f32;
        m *= fade_at(ctx.mode, progress).incoming;
    }
    m
}

fn seek_track(t: &mut Track, pos_secs: f64) {
    let seconds = pos_secs.max(0.0) as u64;
    let frac = (pos_secs - seconds as f64).clamp(0.0, 1.0);
    let time = Time::new(seconds, frac);
    match t.format.seek(
        SeekMode::Coarse,
        SeekTo::Time {
            time,
            track_id: Some(t.track_id),
        },
    ) {
        Ok(seeked) => {
            let _ = t.decoder.reset();
            // Playhead is measured in output frames (what cpal has consumed).
            let played_secs = if let Some(tb) = t.time_base {
                let time = tb.calc_time(seeked.actual_ts);
                time.seconds as f64 + time.frac
            } else {
                pos_secs
            };
            t.produced_output_frames = (played_secs * t.output_sr as f64) as u64;
            // The resampler holds stale interpolation state; reset it.
            t.resampler.src_pos_frac = 0.0;
            t.resampler.have_last = false;
        }
        Err(e) => {
            warn!(error = ?e, "seek failed");
        }
    }
}

fn stop_current(track: &mut Option<Track>) {
    if let Some(t) = track.take() {
        // Signal the fetcher thread that we no longer care about the data.
        {
            let mut g = t.fetch_cancel.lock();
            g.cancelled = true;
        }
        // Drop t → cpal Stream drops → OS output stops.
        drop(t);
    }
}

fn mark_idle(state: &Arc<Mutex<PlaybackState>>) {
    let mut s = state.lock();
    s.status = PlaybackStatus::Idle;
    s.now_playing = None;
    s.position_secs = 0.0;
    s.duration_secs = 0.0;
}

fn sync_queue_meta(state: &Arc<Mutex<PlaybackState>>, queue: &PlaybackQueue) {
    let items = queue.items();
    let idx = queue.current_index();
    let shuffle = queue.shuffle_enabled();
    let repeat = queue.repeat_mode();
    let mut s = state.lock();
    s.queue = items.clone();
    s.current_index = idx;
    s.now_playing = idx.and_then(|i| items.get(i).cloned());
    s.shuffle = shuffle;
    s.repeat = repeat;
}

/// Send the current queue + state to the background persister. Cheap enough
/// to call from every mutating command handler; the persister debounces
/// bursts into a single write.
fn persist_now(
    persister: &Option<Persister>,
    queue: &PlaybackQueue,
    state: &Arc<Mutex<PlaybackState>>,
) {
    if let Some(p) = persister {
        let s = state.lock();
        let snap = PersistedQueue {
            items: queue.items(),
            current_index: queue.current_index(),
            shuffle: queue.shuffle_enabled(),
            repeat: queue.repeat_mode(),
            position_secs: s.position_secs,
        };
        drop(s);
        p.queue_write(snap);
    }
}

/// Load a track and, if a `pending_seek` is pending (e.g. from a restore),
/// apply it once. Returns None if loading failed after skipping the entire
/// remaining queue.
fn load_and_maybe_seek(
    host: &cpal::Host,
    item: QueueItem,
    volume: &Arc<AtomicU32>,
    paused: &Arc<AtomicBool>,
    state: &Arc<Mutex<PlaybackState>>,
    queue: &PlaybackQueue,
    pending_seek: &mut Option<f64>,
    rg_settings: ReplayGainSettings,
) -> Option<Track> {
    let mut track = try_load_track(host, item, volume, paused, state, queue, rg_settings)?;
    if let Some(secs) = pending_seek.take() {
        if secs > 0.0 {
            seek_track(&mut track, secs);
            state.lock().position_secs = secs;
        }
    }
    Some(track)
}

fn update_position(state: &Arc<Mutex<PlaybackState>>, t: &Track) {
    let mut s = state.lock();
    s.position_secs = t.position_secs();
    s.duration_secs = t.duration_secs();
    if !t.paused.load(Ordering::Relaxed) && s.now_playing.is_some() {
        s.status = PlaybackStatus::Playing;
    }
}

// ---------------------------------------------------------------------------
// Track loading
// ---------------------------------------------------------------------------

fn try_load_track(
    host: &cpal::Host,
    item: QueueItem,
    volume: &Arc<AtomicU32>,
    paused: &Arc<AtomicBool>,
    state: &Arc<Mutex<PlaybackState>>,
    queue: &PlaybackQueue,
    rg_settings: ReplayGainSettings,
) -> Option<Track> {
    {
        let mut s = state.lock();
        s.status = PlaybackStatus::Buffering;
        s.now_playing = Some(item.clone());
        s.position_secs = 0.0;
        s.duration_secs = item.duration_secs.map(|d| d as f64).unwrap_or(0.0);
    }
    match load_track(
        host,
        item.clone(),
        volume.clone(),
        paused.clone(),
        rg_settings,
    ) {
        Ok(t) => {
            let mut s = state.lock();
            s.status = PlaybackStatus::Playing;
            s.duration_secs = if s.duration_secs > 0.0 {
                s.duration_secs
            } else {
                t.duration_secs()
            };
            Some(t)
        }
        Err(e) => {
            warn!(error = ?e, title = %item.title, "audio load failed, skipping");
            // Skip past broken tracks without recursion.
            loop {
                queue.advance();
                sync_queue_meta(state, queue);
                match queue.current() {
                    None => {
                        mark_idle(state);
                        return None;
                    }
                    Some(next) => match load_track(
                        host,
                        next.clone(),
                        volume.clone(),
                        paused.clone(),
                        rg_settings,
                    ) {
                        Ok(t) => {
                            let mut s = state.lock();
                            s.status = PlaybackStatus::Playing;
                            return Some(t);
                        }
                        Err(e2) => {
                            warn!(error = ?e2, title = %next.title, "audio load failed, skipping");
                        }
                    },
                }
            }
        }
    }
}

fn load_track(
    host: &cpal::Host,
    item: QueueItem,
    volume: Arc<AtomicU32>,
    paused: Arc<AtomicBool>,
    rg_settings: ReplayGainSettings,
) -> Result<Track> {
    // 1. Kick off the HTTP fetch.
    let source = spawn_streaming_source(&item.stream_url)?;
    let fetch_cancel = source.buf.clone();

    // 2. Probe with symphonia.
    let mut hint = Hint::new();
    if let Some(ext) = ext_from_content_type(&item.content_type) {
        hint.with_extension(ext);
    }
    let mss = MediaSourceStream::new(Box::new(source), MediaSourceStreamOptions::default());
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions {
                enable_gapless: true,
                ..Default::default()
            },
            &MetadataOptions::default(),
        )
        .context("symphonia probe failed")?;
    let mut format = probed.format;

    let track_meta = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .cloned()
        .context("no decodable audio track")?;
    let track_id = track_meta.id;
    let time_base = track_meta.codec_params.time_base;
    let total_frames = track_meta.codec_params.n_frames;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track_meta.codec_params, &DecoderOptions::default())
        .context("no decoder for this codec")?;

    // 3. Decode the FIRST packet so we learn the ACTUAL sample rate and
    //    channel count from the decoder — the codec_params metadata can lie
    //    for formats like HE-AAC (SBR doubles the effective rate) and some
    //    Ogg streams where the header is optimistic.
    let (source_sr, source_channels, first_samples) =
        prime_decoder(&mut format, decoder.as_mut(), track_id)?;

    // 4. Open cpal at the device's DEFAULT config — that's the rate/channels
    //    the OS actually plays at. Asking cpal to reconfigure the device
    //    silently fails on some backends, which is what causes slowed audio.
    let device = host
        .default_output_device()
        .context("no default audio output device")?;
    let default_cfg = device
        .default_output_config()
        .context("no default output config")?;
    let output_sr = default_cfg.sample_rate().0;
    let output_channels = default_cfg.channels() as usize;

    debug!(
        source_sr,
        source_channels,
        output_sr,
        output_channels,
        codec = ?track_meta.codec_params.codec,
        "opening cpal output"
    );

    let (stream, producer, ring) = build_output_stream(&device, &default_cfg, paused.clone())?;

    let mut resampler = Resampler::new(source_sr, output_sr, source_channels, output_channels);

    // 4b. Read whatever ReplayGain tags this track carries. Done AFTER the
    //     first-packet decode primer above — several formats only expose
    //     metadata once the first packet has flown by. The tags are pushed
    //     into the Rockbox PGA when this track starts decoding as current;
    //     the linear value is the f32 fallback for DSP-bypassing paths.
    let replaygain_info = ReplayGainInfo::extract_from(&mut format);
    let replaygain_linear = replaygain_info.linear_gain(rg_settings);
    debug!(
        title = %item.title,
        track_gain = ?replaygain_info.track_gain_db,
        album_gain = ?replaygain_info.album_gain_db,
        linear = replaygain_linear,
        "replaygain resolved"
    );

    // 5. Prime the ring with the first packet's samples so playback starts
    //    immediately instead of waiting a full worker tick.
    let mut track = Track {
        format,
        decoder,
        track_id,
        source_sr,
        output_sr,
        output_channels,
        time_base,
        total_frames,
        produced_output_frames: 0,
        sample_buf: None,
        resampler: Resampler::new(source_sr, output_sr, source_channels, output_channels),
        producer,
        ring,
        fetch_cancel,
        _stream: stream,
        item,
        volume,
        paused,
        _started: Instant::now(),
        replaygain_info,
        replaygain_linear,
        rg_gains_pushed: false,
        ended: false,
        overlap_incoming: None,
        overlap_outgoing: None,
    };
    let first_out = resampler.process(&first_samples);
    if !first_out.is_empty() {
        let frames = first_out.len() / output_channels.max(1);
        // The primed packet never routes through the DSP, so it takes the
        // f32 ReplayGain fallback — same magnitude as the PGA gain the
        // following packets get, so there's no audible seam.
        push_samples_with_volume(&track, &first_out, track.replaygain_linear);
        track.produced_output_frames += frames as u64;
    }
    // Adopt the primed resampler state.
    track.resampler = resampler;
    Ok(track)
}

/// Decode packets until we get non-empty audio, then return its interleaved
/// f32 samples along with the actual sample rate and channel count.
fn prime_decoder(
    format: &mut Box<dyn FormatReader>,
    decoder: &mut dyn Decoder,
    track_id: u32,
) -> Result<(u32, usize, Vec<f32>)> {
    loop {
        let packet = format.next_packet().context("no audio packet in stream")?;
        if packet.track_id() != track_id {
            continue;
        }
        let audio_buf = match decoder.decode(&packet) {
            Ok(b) => b,
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(e) => return Err(anyhow!("decode primer: {e}")),
        };
        let spec = *audio_buf.spec();
        let cap = audio_buf.capacity() as u64;
        if cap == 0 {
            continue;
        }
        let mut sb = SampleBuffer::<f32>::new(cap, spec);
        sb.copy_interleaved_ref(audio_buf);
        let samples = sb.samples().to_vec();
        if samples.is_empty() {
            continue;
        }
        return Ok((spec.rate, spec.channels.count(), samples));
    }
}

fn ext_from_content_type(ct: &str) -> Option<&'static str> {
    let ct = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match ct.as_str() {
        "audio/mpeg" | "audio/mp3" => Some("mp3"),
        "audio/flac" | "audio/x-flac" => Some("flac"),
        "audio/aac" => Some("aac"),
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" => Some("m4a"),
        "audio/ogg" | "application/ogg" => Some("ogg"),
        "audio/opus" => Some("opus"),
        "audio/wav" | "audio/wave" | "audio/x-wav" => Some("wav"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// cpal output stream
// ---------------------------------------------------------------------------

/// Open the default output device using its declared default config — the
/// only config guaranteed to be honored by every cpal backend. The ring
/// buffer stores samples already resampled + channel-converted to this spec,
/// so the callback is a straight copy.
fn build_output_stream(
    device: &cpal::Device,
    default_cfg: &cpal::SupportedStreamConfig,
    paused: Arc<AtomicBool>,
) -> Result<(cpal::Stream, Producer<f32>, Arc<SpscRb<f32>>)> {
    if default_cfg.sample_format() != SampleFormat::F32 {
        return Err(anyhow!(
            "device default output is {:?}, only f32 is supported",
            default_cfg.sample_format()
        ));
    }
    let config: cpal::StreamConfig = default_cfg.clone().into();
    let output_sr = config.sample_rate.0;
    let output_channels = config.channels as usize;

    // ~250 ms of interleaved output-rate audio.
    let ring_frames = (output_sr as usize / 4).max(2048);
    let rb = Arc::new(SpscRb::<f32>::new(ring_frames * output_channels));
    let producer = rb.producer();
    let consumer = rb.consumer();

    let err_fn = |e| error!(error = %e, "cpal stream error");

    let stream = device
        .build_output_stream(
            &config,
            move |out: &mut [f32], _| {
                if paused.load(Ordering::Relaxed) {
                    for s in out.iter_mut() {
                        *s = 0.0;
                    }
                    return;
                }
                let n = consumer.read(out).unwrap_or(0);
                for s in &mut out[n..] {
                    *s = 0.0;
                }
            },
            err_fn,
            None,
        )
        .context("build cpal output stream")?;

    stream.play().context("start cpal stream")?;
    Ok((stream, producer, rb))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // ext_from_content_type
    // ------------------------------------------------------------------

    #[test]
    fn maps_common_audio_mimes_to_extensions() {
        assert_eq!(ext_from_content_type("audio/mpeg"), Some("mp3"));
        assert_eq!(ext_from_content_type("audio/flac"), Some("flac"));
        assert_eq!(ext_from_content_type("audio/x-flac"), Some("flac"));
        assert_eq!(ext_from_content_type("audio/mp4"), Some("m4a"));
        assert_eq!(ext_from_content_type("audio/ogg"), Some("ogg"));
        assert_eq!(ext_from_content_type("audio/opus"), Some("opus"));
        assert_eq!(ext_from_content_type("audio/wav"), Some("wav"));
    }

    #[test]
    fn ignores_content_type_parameters_and_case() {
        assert_eq!(
            ext_from_content_type("audio/MPEG; charset=binary"),
            Some("mp3")
        );
        assert_eq!(ext_from_content_type("  Audio/FLAC  "), Some("flac"));
    }

    #[test]
    fn unknown_mime_returns_none() {
        assert_eq!(ext_from_content_type("video/mp4"), None);
        assert_eq!(ext_from_content_type(""), None);
    }

    // ------------------------------------------------------------------
    // Resampler::convert_channels
    // ------------------------------------------------------------------

    #[test]
    fn channel_conversion_is_a_noop_for_matching_layouts() {
        let r = Resampler::new(48_000, 48_000, 2, 2);
        let input = vec![1.0, 2.0, 3.0, 4.0];
        assert_eq!(r.convert_channels(&input), input);
    }

    #[test]
    fn stereo_downmixes_to_mono_as_average() {
        let r = Resampler::new(48_000, 48_000, 2, 1);
        // Two frames: (L=1.0, R=3.0), (L=-1.0, R=1.0).
        let input = vec![1.0, 3.0, -1.0, 1.0];
        let out = r.convert_channels(&input);
        assert_eq!(out, vec![2.0, 0.0]);
    }

    #[test]
    fn mono_broadcasts_to_every_output_channel() {
        let r = Resampler::new(48_000, 48_000, 1, 2);
        let input = vec![0.5, -0.25];
        assert_eq!(r.convert_channels(&input), vec![0.5, 0.5, -0.25, -0.25]);
    }

    #[test]
    fn extra_output_channels_get_duplicated_front_pair() {
        // Stereo → 5.1 layout: c=0/1 pass through, c>=2 fall back to L/R pattern.
        let r = Resampler::new(48_000, 48_000, 2, 6);
        let input = vec![1.0, 2.0]; // one frame: L=1, R=2
        let out = r.convert_channels(&input);
        // 6 channels: L, R, then alternating L/R fill.
        assert_eq!(out, vec![1.0, 2.0, 1.0, 2.0, 1.0, 2.0]);
    }

    // ------------------------------------------------------------------
    // Resampler::resample
    // ------------------------------------------------------------------

    #[test]
    fn resample_is_a_noop_when_rates_match() {
        let mut r = Resampler::new(44_100, 44_100, 2, 2);
        let input: Vec<f32> = (0..20).map(|i| i as f32).collect();
        let out = r.resample(input.clone());
        assert_eq!(out, input);
    }

    #[test]
    fn downsample_48k_to_44_1k_produces_expected_frame_count() {
        // 48 kHz → 44.1 kHz means output_frames ≈ input_frames * 44100/48000.
        // The resampler only emits frames it can fully interpolate, so with a
        // single packet we get floor rather than round — but close enough.
        let mut r = Resampler::new(48_000, 44_100, 2, 2);
        let input_frames = 48_000; // one second of 48 kHz
        let input = vec![0.5f32; input_frames * 2];
        let out = r.resample(input);
        let out_frames = out.len() / 2;
        // One second at 44.1 kHz = 44100 frames. Allow ±2 for boundary rounding.
        assert!(
            (out_frames as i64 - 44_100).abs() <= 2,
            "downsample produced {} frames, expected ~44100",
            out_frames
        );
    }

    #[test]
    fn upsample_44_1k_to_48k_produces_expected_frame_count() {
        let mut r = Resampler::new(44_100, 48_000, 2, 2);
        let input_frames = 44_100;
        let input = vec![0.5f32; input_frames * 2];
        let out = r.resample(input);
        let out_frames = out.len() / 2;
        assert!(
            (out_frames as i64 - 48_000).abs() <= 2,
            "upsample produced {} frames, expected ~48000",
            out_frames
        );
    }

    #[test]
    fn cross_packet_output_matches_single_packet_output() {
        // Same total input, delivered as one packet vs two halves, should
        // yield essentially the same output stream. Boundary interpolation
        // uses `last_frame`, so results converge within a couple of samples.
        let sr_in = 48_000;
        let sr_out = 44_100;
        let n_frames = 4_800; // 100 ms
        let input: Vec<f32> = (0..n_frames * 2).map(|i| (i as f32).sin()).collect();

        let mut r1 = Resampler::new(sr_in, sr_out, 2, 2);
        let single = r1.resample(input.clone());

        let mut r2 = Resampler::new(sr_in, sr_out, 2, 2);
        let (a, b) = input.split_at(n_frames); // half the frames = n_frames/2 samples each
        let half1 = r2.resample(a.to_vec());
        let half2 = r2.resample(b.to_vec());
        let combined: Vec<f32> = half1.into_iter().chain(half2).collect();

        // Both paths should produce close-to-identical frame counts.
        let diff = (single.len() as i64 - combined.len() as i64).abs();
        assert!(
            diff <= 4,
            "single-packet and split-packet output differ by {} samples",
            diff
        );
    }

    #[test]
    fn resampler_saves_last_frame_across_calls_at_matching_rates() {
        // Even in the passthrough path we save last_frame so a subsequent
        // rate change would interpolate cleanly. Verifies the have_last
        // flag is set after any non-empty process().
        let mut r = Resampler::new(48_000, 48_000, 2, 2);
        let _ = r.resample(vec![0.0, 1.0, 2.0, 3.0]);
        assert!(r.have_last);
        assert_eq!(r.last_frame, vec![2.0, 3.0]);
    }

    // ------------------------------------------------------------------
    // Resampler::process (end-to-end: channel conv + rate conv)
    // ------------------------------------------------------------------

    #[test]
    fn process_downmixes_and_resamples_together() {
        let mut r = Resampler::new(48_000, 44_100, 2, 1);
        // 480 stereo frames at 48 kHz = 10 ms. After downmix + downsample we
        // expect ~441 mono frames.
        let input: Vec<f32> = (0..480).flat_map(|i| [i as f32, -(i as f32)]).collect();
        let out = r.process(&input);
        assert!(
            (out.len() as i64 - 441).abs() <= 2,
            "process yielded {} samples, expected ~441",
            out.len()
        );
    }
}
