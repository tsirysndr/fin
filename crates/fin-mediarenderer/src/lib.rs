//! fin as a UPnP AV **MediaRenderer device** — the receiving side.
//!
//! `fin_player::upnp` casts *to* other renderers; this crate is the mirror
//! image: it advertises this machine on the LAN (SSDP), serves the device /
//! service description XML plus SOAP control endpoints over a tiny embedded
//! HTTP server, and routes whatever a control point pushes at us into the
//! local playback stack — audio lands in the in-process symphonia player,
//! video is handed to mpv. The split happens for free by driving the shared
//! [`fin_player::LocalRenderer`] through the same `Renderer` trait the TUI
//! uses, so incoming casts show up in the Now Playing bar and respond to the
//! normal transport keys.
//!
//! Like the UPnP client code, all XML/SSDP parsing is hand-rolled: the
//! documents involved are small and well-shaped, and a full XML/HTTP-server
//! dependency would blow up the tree for no real gain.

mod desc;
mod gena;
mod http;
mod soap;
mod ssdp;
mod xml;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use fin_player::{QueueItem, Renderer};

/// Shared, swappable handle to whatever renderer the TUI is currently
/// driving. The TUI stores the same cell, so a `m` (switch to local) or a
/// Devices-screen connect is immediately reflected in where incoming casts
/// land.
pub type RendererCell = Arc<Mutex<Arc<dyn Renderer>>>;

#[derive(Debug, Clone)]
pub struct Options {
    /// Name shown in control-point device pickers.
    pub friendly_name: String,
    /// Bare UUID (no `uuid:` prefix). Should be stable across restarts so
    /// control points remember the device.
    pub uuid: String,
    /// TCP port for the description/control/event server. `0` = ephemeral.
    pub port: u16,
}

/// AVTransport session state pushed at us by the control point. The
/// renderer owns actual playback; this only remembers what the last
/// `SetAVTransportURI` said so `GetMediaInfo`/`GetPositionInfo` can echo it.
pub(crate) struct Session {
    /// Item staged by `SetAVTransportURI`, consumed by the next `Play`.
    pub pending: Option<QueueItem>,
    /// Last item we actually pushed into the renderer.
    pub current: Option<QueueItem>,
    pub current_uri: String,
    pub current_meta: String,
    pub next_uri: String,
    pub next_meta: String,
    /// Volume before mute — `Some` means we're muted.
    pub pre_mute_volume: Option<f32>,
    /// Monotonic counter feeding the `upnp-cast:<n>` queue-item ids.
    pub item_seq: u64,
}

pub(crate) struct Inner {
    pub opts: Options,
    pub renderer: RendererCell,
    pub http_addr: SocketAddr,
    pub location: String,
    pub session: Mutex<Session>,
    pub subs: Mutex<Vec<gena::Subscription>>,
    pub client: reqwest::Client,
}

impl Inner {
    pub fn renderer(&self) -> Arc<dyn Renderer> {
        self.renderer.lock().clone()
    }

    pub fn udn(&self) -> String {
        format!("uuid:{}", self.opts.uuid)
    }

    pub fn server_header() -> String {
        format!(
            "{} UPnP/1.0 fin/{}",
            std::env::consts::OS,
            env!("CARGO_PKG_VERSION")
        )
    }
}

pub struct MediaRendererServer {
    inner: Arc<Inner>,
    tasks: Vec<JoinHandle<()>>,
}

impl MediaRendererServer {
    /// Bind the HTTP endpoint, join the SSDP multicast group, and start
    /// advertising. Returns as soon as everything is listening; playback
    /// commands arrive on background tasks from then on.
    pub async fn start(opts: Options, renderer: RendererCell) -> Result<Self> {
        let host_ip = local_ip().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let listener = bind_http(opts.port).await?;
        let http_addr = SocketAddr::new(host_ip, listener.local_addr()?.port());
        let location = format!("http://{http_addr}/description.xml");

        let inner = Arc::new(Inner {
            opts,
            renderer,
            http_addr,
            location,
            session: Mutex::new(Session {
                pending: None,
                current: None,
                current_uri: String::new(),
                current_meta: String::new(),
                next_uri: String::new(),
                next_meta: String::new(),
                pre_mute_volume: None,
                item_seq: 0,
            }),
            subs: Mutex::new(Vec::new()),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(3))
                .build()
                .context("build GENA notify client")?,
        });

        let mut tasks = vec![
            tokio::spawn(http::serve(listener, inner.clone())),
            tokio::spawn(gena::notify_loop(inner.clone())),
        ];
        // SSDP needs port 1900 with address reuse — if another UPnP stack on
        // this machine grabbed it exclusively, the device stays reachable by
        // direct URL but won't be discoverable. Degrade with a warning.
        match ssdp::socket() {
            Ok(sock) => tasks.push(tokio::spawn(ssdp::run(sock, inner.clone()))),
            Err(e) => warn!(?e, "SSDP bind failed — MediaRenderer not discoverable"),
        }

        info!(
            name = %inner.opts.friendly_name,
            location = %inner.location,
            "UPnP MediaRenderer up"
        );
        Ok(Self { inner, tasks })
    }

    pub fn http_addr(&self) -> SocketAddr {
        self.inner.http_addr
    }

    pub fn friendly_name(&self) -> &str {
        &self.inner.opts.friendly_name
    }

    /// Multicast `ssdp:byebye` so control points drop us immediately, then
    /// stop all background tasks.
    pub async fn shutdown(self) {
        ssdp::byebye(&self.inner).await;
        for t in &self.tasks {
            t.abort();
        }
    }
}

async fn bind_http(port: u16) -> Result<TcpListener> {
    match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => Ok(l),
        Err(e) if port != 0 => {
            warn!(
                ?e,
                port, "MediaRenderer port busy — falling back to ephemeral"
            );
            TcpListener::bind(("0.0.0.0", 0))
                .await
                .context("bind MediaRenderer HTTP listener")
        }
        Err(e) => Err(e).context("bind MediaRenderer HTTP listener"),
    }
}

/// The IPv4 address of the default-route interface — what goes into the
/// SSDP `LOCATION` header. A connected UDP socket never sends a packet;
/// the OS just resolves which source address it *would* use.
fn local_ip() -> Option<IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    sock.local_addr().ok().map(|a| a.ip())
}
