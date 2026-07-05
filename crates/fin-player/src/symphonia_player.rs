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

use crate::queue::{PlaybackQueue, QueueItem};
use crate::renderer::{PlaybackState, PlaybackStatus, Renderer, RendererKind};

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
    Quit,
}

impl SymphoniaPlayer {
    pub fn new() -> Self {
        let queue = PlaybackQueue::new();
        let state = Arc::new(Mutex::new(PlaybackState::default()));
        let (cmd_tx, cmd_rx) = mpsc::channel::<PlayerCommand>();

        let worker_state = state.clone();
        let worker_queue = queue.clone();
        let worker = thread::Builder::new()
            .name("fin-symphonia".into())
            .spawn(move || {
                if let Err(e) = run_worker(cmd_rx, worker_state, worker_queue) {
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

    Ok(StreamingSource {
        buf,
        ready,
        pos: 0,
    })
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
    // Total slot count of the ring (output_channels-interleaved).
    ring_capacity: usize,
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
                    if self.have_last { self.last_frame[c] } else { 0.0 }
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
) -> Result<()> {
    let host = cpal::default_host();
    let volume = Arc::new(AtomicU32::new(f32::to_bits(1.0)));
    let paused = Arc::new(AtomicBool::new(false));

    let mut track: Option<Track> = None;

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
                    stop_current(&mut track);
                    queue.replace(items, start_index);
                    sync_queue_meta(&state, &queue);
                    paused.store(false, Ordering::Relaxed);
                    if let Some(item) = queue.current() {
                        track = try_load_track(&host, item, &volume, &paused, &state, &queue);
                    } else {
                        mark_idle(&state);
                    }
                }
                PlayerCommand::Enqueue(items) => {
                    let was_empty = queue.is_empty();
                    queue.append(items);
                    sync_queue_meta(&state, &queue);
                    if was_empty && track.is_none() {
                        if let Some(item) = queue.current() {
                            track =
                                try_load_track(&host, item, &volume, &paused, &state, &queue);
                        }
                    }
                }
                PlayerCommand::PlayNext(items) => {
                    let was_empty = queue.is_empty();
                    queue.insert_next(items);
                    sync_queue_meta(&state, &queue);
                    if was_empty && track.is_none() {
                        if let Some(item) = queue.current() {
                            track =
                                try_load_track(&host, item, &volume, &paused, &state, &queue);
                        }
                    }
                }
                PlayerCommand::Pause => {
                    paused.store(true, Ordering::Relaxed);
                    let mut s = state.lock();
                    if s.now_playing.is_some() {
                        s.status = PlaybackStatus::Paused;
                    }
                }
                PlayerCommand::Resume => {
                    paused.store(false, Ordering::Relaxed);
                    let mut s = state.lock();
                    if s.now_playing.is_some() {
                        s.status = PlaybackStatus::Playing;
                    }
                }
                PlayerCommand::Stop => {
                    stop_current(&mut track);
                    queue.clear();
                    let mut s = state.lock();
                    s.queue.clear();
                    s.current_index = None;
                    s.now_playing = None;
                    s.status = PlaybackStatus::Stopped;
                    s.position_secs = 0.0;
                    s.duration_secs = 0.0;
                }
                PlayerCommand::Next => {
                    stop_current(&mut track);
                    queue.advance();
                    sync_queue_meta(&state, &queue);
                    if let Some(item) = queue.current() {
                        track = try_load_track(&host, item, &volume, &paused, &state, &queue);
                    } else {
                        mark_idle(&state);
                    }
                }
                PlayerCommand::Previous => {
                    stop_current(&mut track);
                    queue.back();
                    sync_queue_meta(&state, &queue);
                    if let Some(item) = queue.current() {
                        track = try_load_track(&host, item, &volume, &paused, &state, &queue);
                    } else {
                        mark_idle(&state);
                    }
                }
                PlayerCommand::Seek(pos_secs) => {
                    if let Some(ref mut t) = track {
                        seek_track(t, pos_secs);
                    }
                }
                PlayerCommand::SetVolume(v) => {
                    let clamped = v.clamp(0.0, 1.5);
                    volume.store(f32::to_bits(clamped), Ordering::Relaxed);
                    state.lock().volume = clamped;
                }
                PlayerCommand::Quit => break 'main,
            }
        }

        // Decode one packet's worth of audio if we can.
        if let Some(ref mut t) = track {
            if t.paused.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(25));
                update_position(&state, t);
                continue;
            }

            // Backpressure: if the ring buffer is nearly full, don't decode.
            // We need at least one MAX_FRAMES_PER_PACKET * channels slots free.
            let free = t.ring_free_slots();
            if free < 8192 {
                thread::sleep(Duration::from_millis(5));
                update_position(&state, t);
                continue;
            }

            match decode_one_packet(t) {
                DecodeStep::Pushed => {
                    update_position(&state, t);
                }
                DecodeStep::EndOfStream => {
                    debug!(item = %t.item.title, "track ended");
                    // Let the ring buffer drain so the last audio isn't cut off.
                    drain_ring(t);
                    stop_current(&mut track);
                    queue.advance();
                    sync_queue_meta(&state, &queue);
                    if let Some(item) = queue.current() {
                        track = try_load_track(&host, item, &volume, &paused, &state, &queue);
                    } else {
                        mark_idle(&state);
                    }
                }
                DecodeStep::Error(e) => {
                    warn!(error = %e, item = %t.item.title, "decode error, skipping track");
                    stop_current(&mut track);
                    queue.advance();
                    sync_queue_meta(&state, &queue);
                    if let Some(item) = queue.current() {
                        track = try_load_track(&host, item, &volume, &paused, &state, &queue);
                    } else {
                        mark_idle(&state);
                    }
                }
            }
        }
    }

    stop_current(&mut track);
    Ok(())
}

enum DecodeStep {
    Pushed,
    EndOfStream,
    Error(String),
}

fn decode_one_packet(t: &mut Track) -> DecodeStep {
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
    let out_samples = t.resampler.process(src_samples);
    if out_samples.is_empty() {
        return DecodeStep::Pushed;
    }
    let out_frames = out_samples.len() / t.output_channels.max(1);
    push_samples_with_volume(t, &out_samples);
    t.produced_output_frames += out_frames as u64;
    DecodeStep::Pushed
}

fn push_samples_with_volume(t: &Track, samples: &[f32]) {
    let vol = f32::from_bits(t.volume.load(Ordering::Relaxed));
    let mut offset = 0;
    while offset < samples.len() {
        if t.paused.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(20));
            continue;
        }
        let end = std::cmp::min(offset + 4096, samples.len());
        let chunk: Vec<f32> = samples[offset..end].iter().map(|s| s * vol).collect();
        match t.producer.write(&chunk) {
            Ok(n) => offset += n,
            Err(_) => thread::sleep(Duration::from_millis(5)),
        }
    }
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

fn drain_ring(t: &mut Track) {
    let sr = t.output_sr.max(1);
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let free = t.ring_free_slots();
        if free >= t.ring_capacity.saturating_sub(t.output_channels) {
            break;
        }
        let pending = t.ring_pending_frames();
        let sleep_ms = ((pending as f64 / sr as f64) * 1000.0) as u64;
        thread::sleep(Duration::from_millis(sleep_ms.min(200).max(10)));
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
    let mut s = state.lock();
    s.queue = items.clone();
    s.current_index = idx;
    s.now_playing = idx.and_then(|i| items.get(i).cloned());
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
) -> Option<Track> {
    {
        let mut s = state.lock();
        s.status = PlaybackStatus::Buffering;
        s.now_playing = Some(item.clone());
        s.position_secs = 0.0;
        s.duration_secs = item.duration_secs.map(|d| d as f64).unwrap_or(0.0);
    }
    match load_track(host, item.clone(), volume.clone(), paused.clone()) {
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
                    Some(next) => match load_track(host, next.clone(), volume.clone(), paused.clone()) {
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
    let (source_sr, source_channels, first_samples) = prime_decoder(
        &mut format,
        decoder.as_mut(),
        track_id,
    )?;

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

    let (stream, producer, ring, ring_capacity) =
        build_output_stream(&device, &default_cfg, paused.clone())?;

    let mut resampler = Resampler::new(source_sr, output_sr, source_channels, output_channels);

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
        ring_capacity,
        fetch_cancel,
        _stream: stream,
        item,
        volume,
        paused,
        _started: Instant::now(),
    };
    let first_out = resampler.process(&first_samples);
    if !first_out.is_empty() {
        let frames = first_out.len() / output_channels.max(1);
        push_samples_with_volume(&track, &first_out);
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
        let packet = format
            .next_packet()
            .context("no audio packet in stream")?;
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
    let ct = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
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
) -> Result<(cpal::Stream, Producer<f32>, Arc<SpscRb<f32>>, usize)> {
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
    let ring_capacity = ring_frames * output_channels;
    let rb = Arc::new(SpscRb::<f32>::new(ring_capacity));
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
    Ok((stream, producer, rb, ring_capacity))
}
