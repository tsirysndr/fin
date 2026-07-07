//! UPnP AV MediaRenderer support.
//!
//! Discovery is a plain SSDP `M-SEARCH` for
//! `urn:schemas-upnp-org:device:MediaRenderer:1`. Each response's `LOCATION`
//! header points at an XML device description; we fetch it, pull out
//! `friendlyName`, `modelName`, `manufacturer`, and the control URLs for the
//! `AVTransport` and (optional) `RenderingControl` services.
//!
//! Playback is driven with the standard AVTransport SOAP actions —
//! `SetAVTransportURI` + `Play` + `Pause` + `Stop` + `Seek` — and volume
//! goes through `RenderingControl::SetVolume` when the renderer exposes it.
//! A short poll loop calls `GetPositionInfo` + `GetTransportInfo` every
//! ~700ms to keep [`PlaybackState`] fresh and to auto-advance the queue when
//! the current track ends.
//!
//! The hand-rolled XML/SSDP parsing is intentional: UPnP description docs
//! and SOAP responses are small and well-shaped, and pulling in a full XML
//! crate for this would blow up the dependency tree for no real gain.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use tracing::{debug, error, warn};

use crate::queue::{PlaybackQueue, QueueItem};
use crate::renderer::{PlaybackState, PlaybackStatus, Renderer, RendererKind};

const SSDP_ADDR: &str = "239.255.255.250:1900";
const RENDERER_ST: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
const AV_TRANSPORT_SERVICE: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RENDERING_CONTROL_SERVICE: &str = "urn:schemas-upnp-org:service:RenderingControl:1";

#[derive(Debug, Clone)]
pub struct UpnpDevice {
    pub udn: String,
    pub friendly_name: String,
    pub model: String,
    pub manufacturer: String,
    /// IP address we heard the SSDP response from (used only for display).
    pub address: IpAddr,
    /// Description document URL.
    pub location: String,
    /// Absolute URL to POST AVTransport SOAP actions to.
    pub av_transport_control_url: String,
    /// Absolute URL to POST RenderingControl SOAP actions to. Some minimal
    /// renderers only expose AVTransport; volume control is then a no-op.
    pub rendering_control_url: Option<String>,
}

impl UpnpDevice {
    pub fn display_name(&self) -> String {
        if !self.friendly_name.is_empty() {
            self.friendly_name.clone()
        } else if !self.model.is_empty() {
            self.model.clone()
        } else {
            self.udn.clone()
        }
    }
}

/// Multicast an SSDP `M-SEARCH` for MediaRenderer devices and describe each
/// unique responder. Duplicates by UDN so a device that answers twice only
/// shows up once.
pub async fn discover_upnp_renderers(scan_for: Duration) -> Result<Vec<UpnpDevice>> {
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .context("bind UDP socket for SSDP")?;

    let request = format!(
        "M-SEARCH * HTTP/1.1\r\n\
         HOST: {SSDP_ADDR}\r\n\
         MAN: \"ssdp:discover\"\r\n\
         MX: 2\r\n\
         ST: {RENDERER_ST}\r\n\
         USER-AGENT: fin/1 UPnP/1.0\r\n\
         \r\n"
    );

    // Two M-SEARCH bursts: some routers/renderers drop the first packet, and
    // MX=2 gives responders up to 2s of jitter — a second burst still lands
    // inside the scan window and picks up anything that missed the first.
    let _ = socket.send_to(request.as_bytes(), SSDP_ADDR).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let _ = socket.send_to(request.as_bytes(), SSDP_ADDR).await;

    let mut locations: HashMap<String, IpAddr> = HashMap::new();
    let deadline = Instant::now() + scan_for;
    let mut buf = vec![0u8; 4096];
    while Instant::now() < deadline {
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(500));
        match timeout(remaining, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, addr))) => {
                let text = String::from_utf8_lossy(&buf[..n]);
                if let Some(loc) = header(&text, "LOCATION") {
                    locations.entry(loc).or_insert(addr.ip());
                }
            }
            _ => continue,
        }
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()?;

    // Dedup by UDN — the same device may respond via multiple interfaces
    // with the same LOCATION-relative-to-base but different source IPs.
    let mut by_udn: HashMap<String, UpnpDevice> = HashMap::new();
    for (loc, ip) in locations {
        match describe(&client, &loc, ip).await {
            Ok(dev) => {
                by_udn.entry(dev.udn.clone()).or_insert(dev);
            }
            Err(e) => debug!(?e, %loc, "UPnP describe failed"),
        }
    }

    let mut list: Vec<_> = by_udn.into_values().collect();
    list.sort_by(|a, b| a.display_name().cmp(&b.display_name()));
    Ok(list)
}

async fn describe(client: &reqwest::Client, location: &str, address: IpAddr) -> Result<UpnpDevice> {
    let body = client
        .get(location)
        .send()
        .await
        .with_context(|| format!("GET {location}"))?
        .error_for_status()?
        .text()
        .await?;
    let base = url::Url::parse(location).context("parsing LOCATION URL")?;

    let friendly = tag_text(&body, "friendlyName").unwrap_or_default();
    let model = tag_text(&body, "modelName").unwrap_or_default();
    let manufacturer = tag_text(&body, "manufacturer").unwrap_or_default();
    let udn = tag_text(&body, "UDN").unwrap_or_else(|| location.to_string());

    let av_transport = find_service_control(&body, "AVTransport", &base)
        .ok_or_else(|| anyhow!("device has no AVTransport service"))?;
    let rendering = find_service_control(&body, "RenderingControl", &base);

    Ok(UpnpDevice {
        udn,
        friendly_name: friendly,
        model,
        manufacturer,
        address,
        location: location.to_string(),
        av_transport_control_url: av_transport,
        rendering_control_url: rendering,
    })
}

fn header(text: &str, name: &str) -> Option<String> {
    for line in text.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case(name) {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// Extract the text content of the first `<tag>...</tag>` pair. Namespace
/// prefixes are handled by matching against the *local* name only. Case
/// insensitive on the tag name — some renderers use unusual casing.
fn tag_text(xml: &str, local_name: &str) -> Option<String> {
    let lower = xml.to_ascii_lowercase();
    let needle = local_name.to_ascii_lowercase();
    let mut cursor = 0;
    while cursor < lower.len() {
        let lt = lower[cursor..].find('<')?;
        let abs = cursor + lt + 1;
        // Skip closing tags and processing instructions.
        let rest = &lower[abs..];
        if rest.starts_with('/') || rest.starts_with('!') || rest.starts_with('?') {
            cursor = abs;
            continue;
        }
        let name_end = rest
            .find(|c: char| c == ' ' || c == '>' || c == '/' || c == '\t' || c == '\n')
            .unwrap_or(rest.len());
        let tag_name = &rest[..name_end];
        // Handle "prefix:name".
        let local = tag_name.rsplit(':').next().unwrap_or(tag_name);
        if local == needle {
            // Move past the opening tag's `>`.
            let gt = rest.find('>')?;
            let content_start = abs + gt + 1;
            // Match the corresponding closing tag by scanning for
            // `</...tag_name>`. Matching on the original case-preserving XML.
            let close_needle_lower = format!("</{tag_name}>");
            // Search in the lowercased view for correct index alignment.
            let close_rel = lower[content_start..].find(&close_needle_lower)?;
            let content_end = content_start + close_rel;
            return Some(decode_entities(xml[content_start..content_end].trim()));
        }
        cursor = abs + name_end;
    }
    None
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

fn encode_entities(s: &str) -> String {
    // Order matters — replace `&` first so we don't double-encode our own
    // subsequent replacements.
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Walk the description document's `<service>` blocks and return the absolute
/// `controlURL` for the service whose `serviceType` contains the given
/// substring (e.g. "AVTransport"). The `controlURL` is resolved against the
/// document's LOCATION so relative paths become absolute.
fn find_service_control(xml: &str, service_type: &str, base: &url::Url) -> Option<String> {
    let lower = xml.to_ascii_lowercase();
    let mut cursor = 0;
    while let Some(rel) = lower[cursor..].find("<service") {
        let start = cursor + rel;
        // Advance past the opening tag.
        let gt = lower[start..].find('>')? + start + 1;
        let end_rel = lower[gt..].find("</service>")?;
        let end = gt + end_rel;
        let block = &xml[gt..end];
        cursor = end + "</service>".len();
        if let Some(t) = tag_text(block, "serviceType") {
            if t.contains(service_type) {
                if let Some(ctrl) = tag_text(block, "controlURL") {
                    return base.join(&ctrl).ok().map(|u| u.to_string());
                }
            }
        }
    }
    None
}

// ─── Renderer ──────────────────────────────────────────────────────────────

enum UpnpCommand {
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

pub struct UpnpRenderer {
    device: UpnpDevice,
    tx: mpsc::UnboundedSender<UpnpCommand>,
    state: Arc<Mutex<PlaybackState>>,
    queue: PlaybackQueue,
}

impl UpnpRenderer {
    /// Establish a control session with the renderer. There's no persistent
    /// "connection" in UPnP — we validate the AVTransport endpoint by
    /// stopping any prior playback, then hand the worker task the initial
    /// state.
    pub async fn connect(device: UpnpDevice) -> Result<Self> {
        let state = Arc::new(Mutex::new(PlaybackState::default()));
        let queue = PlaybackQueue::new();
        let (tx, rx) = mpsc::unbounded_channel::<UpnpCommand>();

        // Best-effort reset — a stuck STOPPED/TRANSITIONING state left by
        // another controller can otherwise confuse our first Play() call.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()?;
        let _ = av_stop(&client, &device.av_transport_control_url).await;

        let dev = device.clone();
        let state_c = state.clone();
        let queue_c = queue.clone();
        tokio::spawn(async move {
            if let Err(e) = worker(dev, rx, state_c, queue_c, client).await {
                error!(error=?e, "upnp worker exited");
            }
        });

        Ok(Self {
            device,
            tx,
            state,
            queue,
        })
    }

    pub fn device(&self) -> &UpnpDevice {
        &self.device
    }

    pub fn queue_handle(&self) -> PlaybackQueue {
        self.queue.clone()
    }

    async fn send(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<()>>) -> UpnpCommand,
    ) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(make(tx))
            .map_err(|_| anyhow!("upnp worker dead"))?;
        rx.await.context("upnp reply dropped")?
    }
}

impl Drop for UpnpRenderer {
    fn drop(&mut self) {
        let _ = self.tx.send(UpnpCommand::Shutdown);
    }
}

async fn worker(
    device: UpnpDevice,
    mut rx: mpsc::UnboundedReceiver<UpnpCommand>,
    state: Arc<Mutex<PlaybackState>>,
    queue: PlaybackQueue,
    client: reqwest::Client,
) -> Result<()> {
    let mut poll = tokio::time::interval(Duration::from_millis(700));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Suppress the immediate first tick — we don't want to poll before the
    // first Play() has actually loaded a URI.
    poll.tick().await;

    // End-of-track detection mirrors the Chromecast worker: STOPPED /
    // NO_MEDIA_PRESENT counts as "ended" only if we were actively playing
    // in the previous poll window.
    let mut was_active = false;

    loop {
        tokio::select! {
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { break; };
                match cmd {
                    UpnpCommand::Shutdown => {
                        let _ = av_stop(&client, &device.av_transport_control_url).await;
                        break;
                    }
                    other => handle_command(other, &device, &client, &queue, &state).await,
                }
            }
            _ = poll.tick() => {
                if let Err(e) = poll_status(&client, &device, &queue, &state, &mut was_active).await {
                    debug!(?e, "upnp poll failed");
                }
            }
        }
    }
    Ok(())
}

async fn handle_command(
    cmd: UpnpCommand,
    device: &UpnpDevice,
    client: &reqwest::Client,
    queue: &PlaybackQueue,
    state: &Arc<Mutex<PlaybackState>>,
) {
    match cmd {
        UpnpCommand::Play {
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
                load_and_play(client, device, &item, state).await
            } else {
                Ok(())
            };
            let _ = reply.send(res);
        }
        UpnpCommand::Enqueue { items, reply } => {
            let was_empty = queue.is_empty();
            queue.append(items);
            let res = if was_empty {
                if let Some(item) = queue.current() {
                    load_and_play(client, device, &item, state).await
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
        UpnpCommand::PlayNext { items, reply } => {
            let was_empty = queue.is_empty();
            queue.insert_next(items);
            let res = if was_empty {
                if let Some(item) = queue.current() {
                    load_and_play(client, device, &item, state).await
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
        UpnpCommand::Pause(reply) => {
            let res = av_pause(client, &device.av_transport_control_url).await;
            let _ = reply.send(res);
        }
        UpnpCommand::Resume(reply) => {
            let res = av_play(client, &device.av_transport_control_url).await;
            let _ = reply.send(res);
        }
        UpnpCommand::Stop(reply) => {
            queue.clear();
            {
                let mut s = state.lock();
                s.queue.clear();
                s.now_playing = None;
                s.current_index = None;
                s.status = PlaybackStatus::Stopped;
            }
            let res = av_stop(client, &device.av_transport_control_url).await;
            let _ = reply.send(res);
        }
        UpnpCommand::Next(reply) => {
            let res = if queue.advance().is_some() {
                state.lock().current_index = queue.current_index();
                if let Some(item) = queue.current() {
                    load_and_play(client, device, &item, state).await
                } else {
                    Ok(())
                }
            } else {
                Ok(())
            };
            let _ = reply.send(res);
        }
        UpnpCommand::Previous(reply) => {
            let res = if queue.back().is_some() {
                state.lock().current_index = queue.current_index();
                if let Some(item) = queue.current() {
                    load_and_play(client, device, &item, state).await
                } else {
                    Ok(())
                }
            } else {
                Ok(())
            };
            let _ = reply.send(res);
        }
        UpnpCommand::Seek(pos, reply) => {
            let res = av_seek(client, &device.av_transport_control_url, pos).await;
            let _ = reply.send(res);
        }
        UpnpCommand::Volume(v, reply) => {
            let clamped = v.clamp(0.0, 1.0);
            let res = match &device.rendering_control_url {
                Some(url) => rc_set_volume(client, url, clamped).await,
                // No RenderingControl service — record the request in state
                // anyway so the TUI reflects the user's intent even if the
                // renderer can't act on it.
                None => Ok(()),
            };
            if res.is_ok() {
                state.lock().volume = clamped;
            }
            let _ = reply.send(res);
        }
        UpnpCommand::Shutdown => {}
    }
}

async fn poll_status(
    client: &reqwest::Client,
    device: &UpnpDevice,
    queue: &PlaybackQueue,
    state: &Arc<Mutex<PlaybackState>>,
    was_active: &mut bool,
) -> Result<()> {
    let ti = av_get_transport_info(client, &device.av_transport_control_url).await?;
    let pi = av_get_position_info(client, &device.av_transport_control_url).await;

    let mut ended = false;
    match ti.state.as_str() {
        "PLAYING" => {
            state.lock().status = PlaybackStatus::Playing;
            *was_active = true;
        }
        "PAUSED_PLAYBACK" | "PAUSED_RECORDING" => {
            state.lock().status = PlaybackStatus::Paused;
            *was_active = true;
        }
        "TRANSITIONING" => {
            state.lock().status = PlaybackStatus::Buffering;
            *was_active = true;
        }
        "STOPPED" | "NO_MEDIA_PRESENT" => {
            if *was_active {
                ended = true;
            } else {
                state.lock().status = PlaybackStatus::Idle;
            }
        }
        _ => {}
    }

    if let Ok(pi) = pi {
        let mut s = state.lock();
        if let Some(pos) = parse_hms(&pi.rel_time) {
            s.position_secs = pos;
        }
        if let Some(dur) = parse_hms(&pi.track_duration) {
            if dur > 0.0 {
                s.duration_secs = dur;
            }
        }
    }

    if ended {
        *was_active = false;
        if queue.advance().is_some() {
            state.lock().current_index = queue.current_index();
            if let Some(next) = queue.current() {
                if let Err(e) = load_and_play(client, device, &next, state).await {
                    warn!(?e, "upnp: failed to load next queue item");
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
    Ok(())
}

async fn load_and_play(
    client: &reqwest::Client,
    device: &UpnpDevice,
    item: &QueueItem,
    state: &Arc<Mutex<PlaybackState>>,
) -> Result<()> {
    let metadata = didl_lite(item);
    av_set_uri(
        client,
        &device.av_transport_control_url,
        &item.stream_url,
        &metadata,
    )
    .await?;
    av_play(client, &device.av_transport_control_url).await?;
    let mut s = state.lock();
    s.now_playing = Some(item.clone());
    s.status = PlaybackStatus::Playing;
    if let Some(d) = item.duration_secs {
        s.duration_secs = d as f64;
    }
    Ok(())
}

// ─── SOAP helpers ──────────────────────────────────────────────────────────

async fn soap_call(
    client: &reqwest::Client,
    control_url: &str,
    service: &str,
    action: &str,
    body: &str,
) -> Result<String> {
    let envelope = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
<s:Body><u:{action} xmlns:u=\"{service}\">{body}</u:{action}></s:Body>\
</s:Envelope>"
    );
    let resp = client
        .post(control_url)
        .header("Content-Type", "text/xml; charset=\"utf-8\"")
        .header("SOAPACTION", format!("\"{service}#{action}\""))
        .body(envelope)
        .send()
        .await
        .with_context(|| format!("POST {control_url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("SOAP {action} failed: {status} — {text}"));
    }
    Ok(resp.text().await.unwrap_or_default())
}

async fn av_set_uri(client: &reqwest::Client, url: &str, uri: &str, metadata: &str) -> Result<()> {
    let body = format!(
        "<InstanceID>0</InstanceID>\
<CurrentURI>{}</CurrentURI>\
<CurrentURIMetaData>{}</CurrentURIMetaData>",
        encode_entities(uri),
        encode_entities(metadata)
    );
    soap_call(
        client,
        url,
        AV_TRANSPORT_SERVICE,
        "SetAVTransportURI",
        &body,
    )
    .await?;
    Ok(())
}

async fn av_play(client: &reqwest::Client, url: &str) -> Result<()> {
    let body = "<InstanceID>0</InstanceID><Speed>1</Speed>";
    soap_call(client, url, AV_TRANSPORT_SERVICE, "Play", body).await?;
    Ok(())
}

async fn av_pause(client: &reqwest::Client, url: &str) -> Result<()> {
    let body = "<InstanceID>0</InstanceID>";
    soap_call(client, url, AV_TRANSPORT_SERVICE, "Pause", body).await?;
    Ok(())
}

async fn av_stop(client: &reqwest::Client, url: &str) -> Result<()> {
    let body = "<InstanceID>0</InstanceID>";
    soap_call(client, url, AV_TRANSPORT_SERVICE, "Stop", body).await?;
    Ok(())
}

async fn av_seek(client: &reqwest::Client, url: &str, secs: f64) -> Result<()> {
    let body = format!(
        "<InstanceID>0</InstanceID><Unit>REL_TIME</Unit><Target>{}</Target>",
        fmt_hms(secs.max(0.0))
    );
    soap_call(client, url, AV_TRANSPORT_SERVICE, "Seek", &body).await?;
    Ok(())
}

struct TransportInfo {
    state: String,
}

async fn av_get_transport_info(client: &reqwest::Client, url: &str) -> Result<TransportInfo> {
    let body = "<InstanceID>0</InstanceID>";
    let resp = soap_call(client, url, AV_TRANSPORT_SERVICE, "GetTransportInfo", body).await?;
    Ok(TransportInfo {
        state: tag_text(&resp, "CurrentTransportState").unwrap_or_default(),
    })
}

struct PositionInfo {
    track_duration: String,
    rel_time: String,
}

async fn av_get_position_info(client: &reqwest::Client, url: &str) -> Result<PositionInfo> {
    let body = "<InstanceID>0</InstanceID>";
    let resp = soap_call(client, url, AV_TRANSPORT_SERVICE, "GetPositionInfo", body).await?;
    Ok(PositionInfo {
        track_duration: tag_text(&resp, "TrackDuration").unwrap_or_default(),
        rel_time: tag_text(&resp, "RelTime").unwrap_or_default(),
    })
}

async fn rc_set_volume(client: &reqwest::Client, url: &str, volume: f32) -> Result<()> {
    // UPnP volume is 0..100 integer on the Master channel.
    let level = (volume.clamp(0.0, 1.0) * 100.0).round() as i32;
    let body = format!(
        "<InstanceID>0</InstanceID><Channel>Master</Channel><DesiredVolume>{level}</DesiredVolume>"
    );
    soap_call(client, url, RENDERING_CONTROL_SERVICE, "SetVolume", &body).await?;
    Ok(())
}

// ─── Small formatting helpers ──────────────────────────────────────────────

fn fmt_hms(secs: f64) -> String {
    let s = secs as u64;
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let sec = s % 60;
    format!("{h:01}:{m:02}:{sec:02}")
}

fn parse_hms(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() || s == "NOT_IMPLEMENTED" {
        return None;
    }
    let mut parts = s.split(':').map(|p| p.parse::<f64>().ok());
    let a = parts.next()??;
    let b = parts.next()??;
    let c = parts.next()??;
    Some(a * 3600.0 + b * 60.0 + c)
}

/// Minimal DIDL-Lite envelope so renderers that require metadata don't reject
/// the URI. Track title/subtitle map to `dc:title` and `upnp:artist`.
fn didl_lite(item: &QueueItem) -> String {
    let upnp_class = if item.is_video {
        "object.item.videoItem"
    } else {
        "object.item.audioItem.musicTrack"
    };
    let title = encode_entities(&item.title);
    let artist_line = if item.subtitle.is_empty() {
        String::new()
    } else {
        format!(
            "<upnp:artist>{}</upnp:artist><dc:creator>{}</dc:creator>",
            encode_entities(&item.subtitle),
            encode_entities(&item.subtitle)
        )
    };
    let image_line = match &item.image_url {
        Some(u) => format!(
            "<upnp:albumArtURI>{}</upnp:albumArtURI>",
            encode_entities(u)
        ),
        None => String::new(),
    };
    let protocol_info = format!("http-get:*:{}:*", item.content_type);
    let uri = encode_entities(&item.stream_url);
    format!(
        "<DIDL-Lite \
xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
<item id=\"0\" parentID=\"-1\" restricted=\"1\">\
<dc:title>{title}</dc:title>\
{artist_line}\
<upnp:class>{upnp_class}</upnp:class>\
{image_line}\
<res protocolInfo=\"{protocol_info}\">{uri}</res>\
</item></DIDL-Lite>"
    )
}

// ─── Renderer impl ─────────────────────────────────────────────────────────

#[async_trait]
impl Renderer for UpnpRenderer {
    fn kind(&self) -> RendererKind {
        RendererKind::Upnp
    }

    async fn play(&self, items: Vec<QueueItem>, start_index: usize) -> Result<()> {
        self.send(|reply| UpnpCommand::Play {
            items,
            start_index,
            reply,
        })
        .await
    }

    async fn enqueue(&self, items: Vec<QueueItem>) -> Result<()> {
        self.send(|reply| UpnpCommand::Enqueue { items, reply })
            .await
    }

    async fn play_next(&self, items: Vec<QueueItem>) -> Result<()> {
        self.send(|reply| UpnpCommand::PlayNext { items, reply })
            .await
    }

    async fn pause(&self) -> Result<()> {
        self.send(UpnpCommand::Pause).await
    }

    async fn resume(&self) -> Result<()> {
        self.send(UpnpCommand::Resume).await
    }

    async fn stop(&self) -> Result<()> {
        self.send(UpnpCommand::Stop).await
    }

    async fn next(&self) -> Result<()> {
        self.send(UpnpCommand::Next).await
    }

    async fn previous(&self) -> Result<()> {
        self.send(UpnpCommand::Previous).await
    }

    async fn seek(&self, position_secs: f64) -> Result<()> {
        self.send(|reply| UpnpCommand::Seek(position_secs, reply))
            .await
    }

    async fn set_volume(&self, volume: f32) -> Result<()> {
        self.send(|reply| UpnpCommand::Volume(volume, reply)).await
    }

    fn state(&self) -> PlaybackState {
        self.state.lock().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ssdp_header() {
        let sample = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nLOCATION: http://192.168.1.10:8080/desc.xml\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";
        assert_eq!(
            header(sample, "location").as_deref(),
            Some("http://192.168.1.10:8080/desc.xml")
        );
    }

    #[test]
    fn parses_tag_with_namespace() {
        let xml = "<root><a:friendlyName>Living Room</a:friendlyName></root>";
        assert_eq!(
            tag_text(xml, "friendlyName").as_deref(),
            Some("Living Room")
        );
    }

    #[test]
    fn finds_service_control_url() {
        let xml = "<root><serviceList>\
<service><serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>\
<controlURL>/AVTransport/ctl</controlURL></service>\
<service><serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>\
<controlURL>/RC/ctl</controlURL></service>\
</serviceList></root>";
        let base = url::Url::parse("http://10.0.0.5:8200/desc.xml").unwrap();
        assert_eq!(
            find_service_control(xml, "AVTransport", &base).as_deref(),
            Some("http://10.0.0.5:8200/AVTransport/ctl")
        );
        assert_eq!(
            find_service_control(xml, "RenderingControl", &base).as_deref(),
            Some("http://10.0.0.5:8200/RC/ctl")
        );
    }

    #[test]
    fn parses_hms() {
        assert_eq!(parse_hms("0:01:30"), Some(90.0));
        assert_eq!(parse_hms("1:00:00"), Some(3600.0));
        assert_eq!(parse_hms("NOT_IMPLEMENTED"), None);
        assert_eq!(parse_hms(""), None);
    }
}
