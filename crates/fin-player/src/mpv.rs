use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::time::sleep;
use tracing::{debug, error, warn};

use crate::queue::{PlaybackQueue, QueueItem};
use crate::renderer::{PlaybackState, PlaybackStatus, Renderer, RendererKind};

/// mpv renderer — spawns a headless mpv (or a windowed one, for video)
/// and drives it over the JSON IPC socket.
pub struct MpvRenderer {
    socket_path: PathBuf,
    force_window: bool,
    queue: PlaybackQueue,
    state: Arc<Mutex<PlaybackState>>,
    ipc: Arc<Mutex<Option<IpcHandle>>>,
    child: Arc<Mutex<Option<Child>>>,
}

#[derive(Clone)]
struct IpcHandle {
    tx: mpsc::UnboundedSender<IpcCommand>,
    req_id: Arc<AtomicU32>,
}

struct IpcCommand {
    payload: Value,
    reply: Option<oneshot::Sender<Result<Value>>>,
}

impl MpvRenderer {
    pub fn new(socket_path: Option<PathBuf>) -> Self {
        let socket_path = socket_path.unwrap_or_else(|| {
            let mut p = std::env::temp_dir();
            p.push(format!("fin-mpv-{}.sock", std::process::id()));
            p
        });
        Self {
            socket_path,
            force_window: true,
            queue: PlaybackQueue::new(),
            state: Arc::new(Mutex::new(PlaybackState::default())),
            ipc: Arc::new(Mutex::new(None)),
            child: Arc::new(Mutex::new(None)),
        }
    }

    pub fn queue_handle(&self) -> PlaybackQueue {
        self.queue.clone()
    }

    /// Ensures mpv is spawned and the IPC channel is ready. Idempotent —
    /// calling it a second time is a no-op.
    async fn ensure_running(&self) -> Result<()> {
        if self.ipc.lock().is_some() {
            return Ok(());
        }

        // If a stale socket exists, remove it first.
        let _ = std::fs::remove_file(&self.socket_path);

        let mut cmd = Command::new("mpv");
        cmd.arg("--idle=yes")
            .arg("--no-terminal")
            .arg("--force-window=".to_string() + if self.force_window { "immediate" } else { "no" })
            .arg("--keep-open=yes")
            .arg("--audio-display=no")
            .arg("--input-ipc-server=".to_string() + self.socket_path.to_string_lossy().as_ref())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let child = cmd.spawn().context(
            "failed to spawn mpv — is it installed? On macOS: `brew install mpv`, on Debian/Ubuntu: `sudo apt install mpv`",
        )?;
        *self.child.lock() = Some(child);

        // Wait for mpv to create the socket. We poll with a small backoff.
        let mut stream = None;
        for _ in 0..40 {
            match UnixStream::connect(&self.socket_path).await {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(_) => sleep(Duration::from_millis(75)).await,
            }
        }
        let stream = stream.context("timed out connecting to mpv IPC socket")?;

        let (tx, rx) = mpsc::unbounded_channel::<IpcCommand>();
        let req_id = Arc::new(AtomicU32::new(1));
        let handle = IpcHandle {
            tx: tx.clone(),
            req_id: req_id.clone(),
        };
        *self.ipc.lock() = Some(handle);

        let state = self.state.clone();
        let queue = self.queue.clone();
        tokio::spawn(async move {
            if let Err(e) = ipc_loop(stream, rx, state, queue).await {
                error!(error=?e, "mpv ipc loop exited");
            }
        });

        // Observe events we care about — send directly on the mpsc,
        // bypassing `cmd()` to avoid recursion through `ensure_running`.
        for prop in [
            "time-pos",
            "duration",
            "pause",
            "volume",
            "playlist-pos",
            "eof-reached",
        ] {
            let id = req_id.fetch_add(1, Ordering::SeqCst);
            let _ = tx.send(IpcCommand {
                payload: json!({
                    "command": ["observe_property", 1, prop],
                    "request_id": id,
                }),
                reply: None,
            });
        }
        Ok(())
    }

    async fn cmd(&self, mut payload: Value) -> Result<Value> {
        self.ensure_running().await?;
        // Clone the handle out of the lock so we don't hold a MutexGuard
        // across the following `.await` (parking_lot guards aren't Send).
        let handle: IpcHandle = {
            let guard = self.ipc.lock();
            guard
                .as_ref()
                .cloned()
                .ok_or_else(|| anyhow!("mpv not running"))?
        };
        let id = handle.req_id.fetch_add(1, Ordering::SeqCst);
        if let Value::Object(ref mut m) = payload {
            m.insert("request_id".into(), Value::from(id));
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        handle
            .tx
            .send(IpcCommand {
                payload,
                reply: Some(reply_tx),
            })
            .map_err(|_| anyhow!("mpv ipc channel closed"))?;
        reply_rx.await.context("mpv ipc reply dropped")?
    }

    fn apply_queue_from_local(&self) {
        let items = self.queue.items();
        let idx = self.queue.current_index();
        let mut s = self.state.lock();
        s.queue = items.clone();
        s.current_index = idx;
        s.now_playing = idx.and_then(|i| items.get(i).cloned());
    }
}

async fn ipc_loop(
    stream: UnixStream,
    mut rx: mpsc::UnboundedReceiver<IpcCommand>,
    state: Arc<Mutex<PlaybackState>>,
    queue: PlaybackQueue,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let pending: Arc<Mutex<std::collections::HashMap<u32, oneshot::Sender<Result<Value>>>>> =
        Arc::new(Mutex::new(Default::default()));

    let pending_read = pending.clone();
    let state_r = state.clone();
    let queue_r = queue.clone();
    let read_task = tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            let value: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(id) = value.get("request_id").and_then(|v| v.as_u64()) {
                let sender = pending_read.lock().remove(&(id as u32));
                if let Some(tx) = sender {
                    let err = value
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("success");
                    if err == "success" {
                        let _ = tx.send(Ok(value.get("data").cloned().unwrap_or(Value::Null)));
                    } else {
                        let _ = tx.send(Err(anyhow!("mpv ipc error: {}", err)));
                    }
                }
                continue;
            }
            if let Some(event) = value.get("event").and_then(|v| v.as_str()) {
                handle_event(event, &value, &state_r, &queue_r);
            }
        }
    });

    while let Some(cmd) = rx.recv().await {
        let id = cmd
            .payload
            .get("request_id")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        if let Some(reply) = cmd.reply {
            pending.lock().insert(id, reply);
        }
        let mut serialized = serde_json::to_vec(&cmd.payload)?;
        serialized.push(b'\n');
        if let Err(e) = writer.write_all(&serialized).await {
            warn!(?e, "mpv ipc write failed");
            break;
        }
    }
    read_task.abort();
    Ok(())
}

fn handle_event(
    event: &str,
    value: &Value,
    state: &Arc<Mutex<PlaybackState>>,
    queue: &PlaybackQueue,
) {
    match event {
        "property-change" => {
            let name = value.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let data = value.get("data").cloned().unwrap_or(Value::Null);
            let mut s = state.lock();
            match name {
                "time-pos" => {
                    s.position_secs = data.as_f64().unwrap_or(s.position_secs);
                }
                "duration" => {
                    s.duration_secs = data.as_f64().unwrap_or(s.duration_secs);
                }
                "pause" => {
                    if let Some(p) = data.as_bool() {
                        s.status = if p {
                            PlaybackStatus::Paused
                        } else if s.now_playing.is_some() {
                            PlaybackStatus::Playing
                        } else {
                            PlaybackStatus::Idle
                        };
                    }
                }
                "volume" => {
                    if let Some(v) = data.as_f64() {
                        s.volume = (v / 100.0) as f32;
                    }
                }
                _ => {}
            }
        }
        "start-file" => {
            let mut s = state.lock();
            s.status = PlaybackStatus::Buffering;
        }
        "file-loaded" => {
            let mut s = state.lock();
            s.status = PlaybackStatus::Playing;
        }
        "end-file" => {
            queue.advance();
            let items = queue.items();
            let idx = queue.current_index();
            let mut s = state.lock();
            s.queue = items.clone();
            s.current_index = idx;
            s.now_playing = idx.and_then(|i| items.get(i).cloned());
            if s.now_playing.is_none() {
                s.status = PlaybackStatus::Idle;
                s.position_secs = 0.0;
                s.duration_secs = 0.0;
            }
        }
        "shutdown" => {
            let mut s = state.lock();
            s.status = PlaybackStatus::Stopped;
        }
        _ => {
            debug!(event, "mpv event");
        }
    }
}

#[async_trait]
impl Renderer for MpvRenderer {
    fn kind(&self) -> RendererKind {
        RendererKind::Mpv
    }

    async fn play(&self, items: Vec<QueueItem>, start_index: usize) -> Result<()> {
        self.ensure_running().await?;
        self.queue.replace(items.clone(), start_index);
        self.apply_queue_from_local();

        // Reset mpv's own playlist and load our items.
        self.cmd(json!({"command": ["playlist-clear"]})).await.ok();
        self.cmd(json!({"command": ["stop"]})).await.ok();

        for (i, item) in items.iter().enumerate() {
            let mode = if i == 0 { "replace" } else { "append" };
            self.cmd(json!({"command": ["loadfile", item.stream_url, mode]}))
                .await?;
        }
        if start_index > 0 {
            self.cmd(json!({"command": ["set_property", "playlist-pos", start_index]}))
                .await?;
        }
        self.cmd(json!({"command": ["set_property", "pause", false]}))
            .await?;
        Ok(())
    }

    async fn enqueue(&self, items: Vec<QueueItem>) -> Result<()> {
        self.ensure_running().await?;
        self.queue.append(items.clone());
        self.apply_queue_from_local();
        for item in items {
            self.cmd(json!({"command": ["loadfile", item.stream_url, "append-play"]}))
                .await?;
        }
        Ok(())
    }

    async fn play_next(&self, items: Vec<QueueItem>) -> Result<()> {
        self.ensure_running().await?;
        self.queue.insert_next(items.clone());
        self.apply_queue_from_local();
        // mpv's `loadfile` with `insert-next` is available since mpv 0.38.
        for item in items {
            self.cmd(json!({"command": ["loadfile", item.stream_url, "insert-next"]}))
                .await?;
        }
        Ok(())
    }

    async fn pause(&self) -> Result<()> {
        self.cmd(json!({"command": ["set_property", "pause", true]}))
            .await
            .map(|_| ())
    }

    async fn resume(&self) -> Result<()> {
        self.cmd(json!({"command": ["set_property", "pause", false]}))
            .await
            .map(|_| ())
    }

    async fn stop(&self) -> Result<()> {
        self.queue.clear();
        self.apply_queue_from_local();
        self.cmd(json!({"command": ["stop"]})).await.map(|_| ())
    }

    async fn next(&self) -> Result<()> {
        self.cmd(json!({"command": ["playlist-next", "weak"]}))
            .await
            .map(|_| ())
    }

    async fn previous(&self) -> Result<()> {
        self.cmd(json!({"command": ["playlist-prev", "weak"]}))
            .await
            .map(|_| ())
    }

    async fn seek(&self, position_secs: f64) -> Result<()> {
        self.cmd(json!({"command": ["seek", position_secs, "absolute"]}))
            .await
            .map(|_| ())
    }

    async fn set_volume(&self, volume: f32) -> Result<()> {
        let pct = (volume.clamp(0.0, 1.5) * 100.0) as i32;
        self.cmd(json!({"command": ["set_property", "volume", pct]}))
            .await
            .map(|_| ())
    }

    fn state(&self) -> PlaybackState {
        self.state.lock().clone()
    }
}

impl Drop for MpvRenderer {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.lock().take() {
            let _ = child.start_kill();
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}
