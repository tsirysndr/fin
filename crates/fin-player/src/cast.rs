use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::oneshot;
use tracing::{debug, error, warn};

use rust_cast::channels::media::{
    IdleReason, Image, Media, Metadata, MovieMediaMetadata, MusicTrackMediaMetadata, PlayerState,
    StreamType,
};
use rust_cast::channels::receiver::CastDeviceApp;
use rust_cast::CastDevice as RcDevice;

use crate::discovery::CastDevice;
use crate::queue::{PlaybackQueue, QueueItem, RepeatMode};
use crate::renderer::{PlaybackState, PlaybackStatus, Renderer, RendererKind};

const DEFAULT_DESTINATION_ID: &str = "receiver-0";

enum CastCommand {
    Play {
        items: Vec<QueueItem>,
        start_index: usize,
        reply: oneshot::Sender<Result<()>>,
    },
    Enqueue {
        items: Vec<QueueItem>,
        reply: oneshot::Sender<Result<()>>,
    },
    PlayNext {
        items: Vec<QueueItem>,
        reply: oneshot::Sender<Result<()>>,
    },
    Pause(oneshot::Sender<Result<()>>),
    Resume(oneshot::Sender<Result<()>>),
    Stop(oneshot::Sender<Result<()>>),
    Next(oneshot::Sender<Result<()>>),
    Previous(oneshot::Sender<Result<()>>),
    Seek(f64, oneshot::Sender<Result<()>>),
    Volume(f32, oneshot::Sender<Result<()>>),
    Shutdown,
}

pub struct ChromecastRenderer {
    device: CastDevice,
    tx: std_mpsc::Sender<CastCommand>,
    state: Arc<Mutex<PlaybackState>>,
    queue: PlaybackQueue,
}

impl ChromecastRenderer {
    /// Connect to a Chromecast and launch the Default Media Receiver.
    pub async fn connect(device: CastDevice) -> Result<Self> {
        let state = Arc::new(Mutex::new(PlaybackState::default()));
        let queue = PlaybackQueue::new();
        let (tx, rx) = std_mpsc::channel::<CastCommand>();

        let dev_for_thread = device.clone();
        let state_for_thread = state.clone();
        let queue_for_thread = queue.clone();

        let (ready_tx, ready_rx) = oneshot::channel();

        thread::spawn(move || {
            if let Err(e) = cast_worker(
                dev_for_thread,
                rx,
                state_for_thread,
                queue_for_thread,
                ready_tx,
            ) {
                error!(error=?e, "chromecast worker exited with error");
            }
        });

        ready_rx
            .await
            .context("chromecast worker dropped ready signal")??;

        Ok(Self {
            device,
            tx,
            state,
            queue,
        })
    }

    pub fn device(&self) -> &CastDevice {
        &self.device
    }

    pub fn queue_handle(&self) -> PlaybackQueue {
        self.queue.clone()
    }

    async fn send(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<()>>) -> CastCommand,
    ) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(make(tx))
            .map_err(|_| anyhow!("chromecast worker dead"))?;
        rx.await.context("chromecast reply dropped")?
    }
}

impl Drop for ChromecastRenderer {
    fn drop(&mut self) {
        let _ = self.tx.send(CastCommand::Shutdown);
    }
}

fn build_media(item: &QueueItem) -> Media {
    let images: Vec<Image> = item
        .image_url
        .clone()
        .map(|u| vec![Image::new(u)])
        .unwrap_or_default();
    let metadata = if item.is_video {
        Metadata::Movie(MovieMediaMetadata {
            title: Some(item.title.clone()),
            subtitle: if item.subtitle.is_empty() {
                None
            } else {
                Some(item.subtitle.clone())
            },
            studio: None,
            release_date: None,
            images,
        })
    } else {
        Metadata::MusicTrack(MusicTrackMediaMetadata {
            album_name: None,
            title: Some(item.title.clone()),
            album_artist: None,
            artist: if item.subtitle.is_empty() {
                None
            } else {
                Some(item.subtitle.clone())
            },
            composer: None,
            track_number: None,
            disc_number: None,
            images,
            release_date: None,
        })
    };
    Media {
        content_id: item.stream_url.clone(),
        content_type: item.content_type.clone(),
        stream_type: StreamType::Buffered,
        duration: item.duration_secs.map(|d| d as f32),
        metadata: Some(metadata),
    }
}

struct Session {
    app_transport: String,
    session_id: String,
    media_session_id: Option<i32>,
}

fn cast_worker(
    device: CastDevice,
    rx: std_mpsc::Receiver<CastCommand>,
    state: Arc<Mutex<PlaybackState>>,
    queue: PlaybackQueue,
    ready: oneshot::Sender<Result<()>>,
) -> Result<()> {
    let host = device.address.to_string();
    let rc = match RcDevice::connect_without_host_verification(host.clone(), device.port) {
        Ok(d) => d,
        Err(e) => {
            let _ = ready.send(Err(anyhow!("connect to {}: {}", device.display_name(), e)));
            return Ok(());
        }
    };

    if let Err(e) = rc.connection.connect(DEFAULT_DESTINATION_ID) {
        let _ = ready.send(Err(anyhow!("initial connect: {}", e)));
        return Ok(());
    }
    if let Err(e) = rc.heartbeat.ping() {
        let _ = ready.send(Err(anyhow!("heartbeat ping: {}", e)));
        return Ok(());
    }

    let app = match rc.receiver.launch_app(&CastDeviceApp::DefaultMediaReceiver) {
        Ok(a) => a,
        Err(e) => {
            let _ = ready.send(Err(anyhow!("launching default media receiver: {}", e)));
            return Ok(());
        }
    };
    if let Err(e) = rc.connection.connect(app.transport_id.as_str()) {
        let _ = ready.send(Err(anyhow!("connect to app: {}", e)));
        return Ok(());
    }

    let session = Arc::new(Mutex::new(Session {
        app_transport: app.transport_id.clone(),
        session_id: app.session_id.clone(),
        media_session_id: None,
    }));

    let _ = ready.send(Ok(()));

    let mut last_heartbeat = Instant::now();
    let mut last_status = Instant::now();

    // Auto-advance state — tracks whether the receiver was in the middle of
    // playing our media *last time we polled*. This is the signal we use to
    // detect end-of-track: when the receiver's active media disappears (or
    // reports IDLE/FINISHED) after having been Playing/Paused/Buffering, we
    // treat that as end-of-track and load the next queue item.
    //
    // The previous implementation polled with `Some(media_session_id)`.
    // Chromecast invalidates that session id the instant playback ends, so
    // subsequent calls returned an empty `entries` list and we never saw
    // the FINISHED signal — playback silently stopped after one track.
    let mut was_active_media = false;

    loop {
        match rx.recv_timeout(Duration::from_millis(300)) {
            Ok(CastCommand::Shutdown) => {
                let sess = session.lock();
                let _ = rc.receiver.stop_app(sess.session_id.as_str());
                return Ok(());
            }
            Ok(cmd) => handle_command(cmd, &rc, &session, &queue, &state),
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }

        if last_heartbeat.elapsed() > Duration::from_secs(4) {
            let _ = rc.heartbeat.ping();
            last_heartbeat = Instant::now();
        }

        if last_status.elapsed() > Duration::from_millis(500) {
            last_status = Instant::now();
            let dest = session.lock().app_transport.clone();

            // Query WITHOUT a session id — returns all active media on the
            // receiver. Empty entries means "nothing playing" which is the
            // end-of-track signal we care about.
            match rc.media.get_status(dest.as_str(), None) {
                Ok(status) => {
                    let mut ended = false;
                    if let Some(entry) = status.entries.first() {
                        // Keep our tracked session id in sync — Chromecast
                        // hands out a new one for each `load()`.
                        session.lock().media_session_id = Some(entry.media_session_id);

                        let mut s = state.lock();
                        s.position_secs = entry.current_time.unwrap_or(0.0) as f64;
                        if let Some(m) = &entry.media {
                            if let Some(d) = m.duration {
                                s.duration_secs = d as f64;
                            }
                        }
                        match entry.player_state {
                            PlayerState::Playing => {
                                s.status = PlaybackStatus::Playing;
                                was_active_media = true;
                            }
                            PlayerState::Paused => {
                                s.status = PlaybackStatus::Paused;
                                was_active_media = true;
                            }
                            PlayerState::Buffering => {
                                s.status = PlaybackStatus::Buffering;
                                was_active_media = true;
                            }
                            PlayerState::Idle => match entry.idle_reason {
                                Some(IdleReason::Finished) => {
                                    ended = true;
                                }
                                Some(IdleReason::Error) => {
                                    // Skip on error too so a single bad
                                    // stream doesn't hang the whole queue.
                                    ended = true;
                                }
                                _ => {
                                    s.status = PlaybackStatus::Idle;
                                }
                            },
                        }
                    } else if was_active_media {
                        // No active media *right now*, but we had media
                        // playing the last time we looked → the receiver
                        // finished the track and dropped the session.
                        ended = true;
                    }

                    if ended {
                        was_active_media = false;
                        if queue.advance().is_some() {
                            state.lock().current_index = queue.current_index();
                            if let Some(next) = queue.current() {
                                if let Err(e) = load_media(&rc, &session, &next, &state) {
                                    warn!(?e, "chromecast: failed to load next queue item");
                                    state.lock().status = PlaybackStatus::Idle;
                                }
                            }
                        } else {
                            let mut s = state.lock();
                            s.status = PlaybackStatus::Idle;
                            s.now_playing = None;
                            s.current_index = None;
                        }
                    }
                }
                Err(e) => {
                    debug!(?e, "chromecast: get_status failed");
                }
            }
        }
    }
}

fn handle_command(
    cmd: CastCommand,
    rc: &RcDevice,
    session: &Arc<Mutex<Session>>,
    queue: &PlaybackQueue,
    state: &Arc<Mutex<PlaybackState>>,
) {
    match cmd {
        CastCommand::Play {
            items,
            start_index,
            reply,
        } => {
            queue.replace(items.clone(), start_index);
            let index = queue.current_index().unwrap_or(0);
            let current = items.get(index).cloned();
            {
                let mut s = state.lock();
                s.queue = queue.items();
                s.current_index = queue.current_index();
                s.now_playing = current.clone();
                s.status = PlaybackStatus::Buffering;
            }
            let res = if let Some(item) = current {
                load_media(rc, session, &item, state)
            } else {
                Ok(())
            };
            let _ = reply.send(res);
        }
        CastCommand::Enqueue { items, reply } => {
            let was_empty = queue.is_empty();
            queue.append(items);
            let res = if was_empty {
                if let Some(item) = queue.current() {
                    load_media(rc, session, &item, state)
                } else {
                    Ok(())
                }
            } else {
                Ok(())
            };
            {
                let mut s = state.lock();
                s.queue = queue.items();
                s.current_index = queue.current_index();
                s.now_playing = queue.current();
            }
            let _ = reply.send(res);
        }
        CastCommand::PlayNext { items, reply } => {
            let was_empty = queue.is_empty();
            queue.insert_next(items);
            let res = if was_empty {
                if let Some(item) = queue.current() {
                    load_media(rc, session, &item, state)
                } else {
                    Ok(())
                }
            } else {
                Ok(())
            };
            {
                let mut s = state.lock();
                s.queue = queue.items();
                s.current_index = queue.current_index();
                s.now_playing = queue.current();
            }
            let _ = reply.send(res);
        }
        CastCommand::Pause(reply) => {
            let sess = session.lock();
            let media_id = sess.media_session_id;
            let dest = sess.app_transport.clone();
            drop(sess);
            let res = if let Some(id) = media_id {
                rc.media
                    .pause(dest.as_str(), id)
                    .map(|_| ())
                    .map_err(|e| anyhow!("pause: {}", e))
            } else {
                Ok(())
            };
            let _ = reply.send(res);
        }
        CastCommand::Resume(reply) => {
            let sess = session.lock();
            let media_id = sess.media_session_id;
            let dest = sess.app_transport.clone();
            drop(sess);
            let res = if let Some(id) = media_id {
                rc.media
                    .play(dest.as_str(), id)
                    .map(|_| ())
                    .map_err(|e| anyhow!("resume: {}", e))
            } else {
                Ok(())
            };
            let _ = reply.send(res);
        }
        CastCommand::Stop(reply) => {
            let sess = session.lock();
            let media_id = sess.media_session_id;
            let dest = sess.app_transport.clone();
            drop(sess);
            queue.clear();
            {
                let mut s = state.lock();
                s.queue.clear();
                s.now_playing = None;
                s.current_index = None;
                s.status = PlaybackStatus::Stopped;
            }
            let res = if let Some(id) = media_id {
                rc.media
                    .stop(dest.as_str(), id)
                    .map(|_| ())
                    .map_err(|e| anyhow!("stop: {}", e))
            } else {
                Ok(())
            };
            let _ = reply.send(res);
        }
        CastCommand::Next(reply) => {
            let res = if queue.advance().is_some() {
                state.lock().current_index = queue.current_index();
                if let Some(item) = queue.current() {
                    load_media(rc, session, &item, state)
                } else {
                    Ok(())
                }
            } else {
                Ok(())
            };
            let _ = reply.send(res);
        }
        CastCommand::Previous(reply) => {
            let res = if queue.back().is_some() {
                state.lock().current_index = queue.current_index();
                if let Some(item) = queue.current() {
                    load_media(rc, session, &item, state)
                } else {
                    Ok(())
                }
            } else {
                Ok(())
            };
            let _ = reply.send(res);
        }
        CastCommand::Seek(pos, reply) => {
            let sess = session.lock();
            let media_id = sess.media_session_id;
            let dest = sess.app_transport.clone();
            drop(sess);
            let res = if let Some(id) = media_id {
                rc.media
                    .seek(dest.as_str(), id, Some(pos as f32), None)
                    .map(|_| ())
                    .map_err(|e| anyhow!("seek: {}", e))
            } else {
                Ok(())
            };
            let _ = reply.send(res);
        }
        CastCommand::Volume(v, reply) => {
            let res = rc
                .receiver
                .set_volume(v.clamp(0.0, 1.0))
                .map(|_| ())
                .map_err(|e| anyhow!("volume: {}", e));
            if res.is_ok() {
                state.lock().volume = v.clamp(0.0, 1.0);
            }
            let _ = reply.send(res);
        }
        CastCommand::Shutdown => {
            let sess = session.lock();
            let _ = rc.receiver.stop_app(sess.session_id.as_str());
        }
    }
}

fn load_media(
    rc: &RcDevice,
    session: &Arc<Mutex<Session>>,
    item: &QueueItem,
    state: &Arc<Mutex<PlaybackState>>,
) -> Result<()> {
    let media = build_media(item);
    let sess = session.lock();
    let dest = sess.app_transport.clone();
    let sid = sess.session_id.clone();
    drop(sess);
    let status = rc
        .media
        .load(dest.as_str(), sid.as_str(), &media)
        .map_err(|e| anyhow!("cast load: {}", e))?;
    if let Some(entry) = status.entries.first() {
        session.lock().media_session_id = Some(entry.media_session_id);
        let mut s = state.lock();
        s.duration_secs = entry.media.as_ref().and_then(|m| m.duration).unwrap_or(0.0) as f64;
        s.now_playing = Some(item.clone());
        s.status = PlaybackStatus::Playing;
    }
    Ok(())
}

#[async_trait]
impl Renderer for ChromecastRenderer {
    fn kind(&self) -> RendererKind {
        RendererKind::Chromecast
    }

    async fn play(&self, items: Vec<QueueItem>, start_index: usize) -> Result<()> {
        self.send(|reply| CastCommand::Play {
            items,
            start_index,
            reply,
        })
        .await
    }

    async fn enqueue(&self, items: Vec<QueueItem>) -> Result<()> {
        self.send(|reply| CastCommand::Enqueue { items, reply })
            .await
    }

    async fn play_next(&self, items: Vec<QueueItem>) -> Result<()> {
        self.send(|reply| CastCommand::PlayNext { items, reply })
            .await
    }

    async fn pause(&self) -> Result<()> {
        self.send(CastCommand::Pause).await
    }

    async fn resume(&self) -> Result<()> {
        self.send(CastCommand::Resume).await
    }

    async fn stop(&self) -> Result<()> {
        self.send(CastCommand::Stop).await
    }

    async fn next(&self) -> Result<()> {
        self.send(CastCommand::Next).await
    }

    async fn previous(&self) -> Result<()> {
        self.send(CastCommand::Previous).await
    }

    async fn seek(&self, position_secs: f64) -> Result<()> {
        self.send(|reply| CastCommand::Seek(position_secs, reply))
            .await
    }

    async fn set_volume(&self, volume: f32) -> Result<()> {
        self.send(|reply| CastCommand::Volume(volume, reply)).await
    }

    // Shuffle/repeat live entirely in our queue — the receiver only ever
    // sees one media item at a time, so no cast round-trip is needed. The
    // worker's auto-advance picks up the new order/mode on the next track end.
    async fn set_shuffle(&self, on: bool) -> Result<()> {
        self.queue.set_shuffle(on);
        let items = self.queue.items();
        let mut s = self.state.lock();
        s.shuffle = on;
        s.queue = items;
        s.current_index = self.queue.current_index();
        Ok(())
    }

    async fn set_repeat(&self, mode: RepeatMode) -> Result<()> {
        self.queue.set_repeat(mode);
        self.state.lock().repeat = mode;
        Ok(())
    }

    fn state(&self) -> PlaybackState {
        self.state.lock().clone()
    }
}
