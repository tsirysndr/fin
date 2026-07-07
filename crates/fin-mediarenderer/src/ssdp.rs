//! SSDP presence — periodic `ssdp:alive` NOTIFYs, unicast M-SEARCH answers,
//! and a parting `ssdp:byebye`. This is the discovery half of being a
//! MediaRenderer; the LOCATION header points control points at our
//! description XML.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tracing::{debug, warn};

use crate::Inner;

const SSDP_ADDR: &str = "239.255.255.250:1900";
const SSDP_MULTICAST: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const MAX_AGE_SECS: u64 = 1800;
/// Re-advertise well inside the max-age window so caches never lapse.
const ALIVE_INTERVAL: Duration = Duration::from_secs(300);

const DEVICE_TYPE: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
const SERVICE_TYPES: [&str; 3] = [
    "urn:schemas-upnp-org:service:AVTransport:1",
    "urn:schemas-upnp-org:service:RenderingControl:1",
    "urn:schemas-upnp-org:service:ConnectionManager:1",
];

/// Bind 0.0.0.0:1900 with address reuse and join the SSDP multicast group.
/// Reuse matters — Chromecast/mDNS stacks and other UPnP apps commonly
/// share the port.
pub(crate) fn socket() -> Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .context("create SSDP socket")?;
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    sock.set_reuse_port(true)?;
    let bind_addr: SocketAddr = "0.0.0.0:1900".parse().expect("static addr");
    sock.bind(&bind_addr.into()).context("bind SSDP :1900")?;
    sock.join_multicast_v4(&SSDP_MULTICAST, &Ipv4Addr::UNSPECIFIED)
        .context("join SSDP multicast group")?;
    sock.set_multicast_ttl_v4(2)?;
    sock.set_nonblocking(true)?;
    UdpSocket::from_std(sock.into()).context("wrap SSDP socket for tokio")
}

/// The (NT, USN) pairs a root MediaRenderer must announce and answer for.
fn targets(udn: &str) -> Vec<(String, String)> {
    let mut list = vec![
        (
            "upnp:rootdevice".to_string(),
            format!("{udn}::upnp:rootdevice"),
        ),
        (udn.to_string(), udn.to_string()),
        (DEVICE_TYPE.to_string(), format!("{udn}::{DEVICE_TYPE}")),
    ];
    for svc in SERVICE_TYPES {
        list.push((svc.to_string(), format!("{udn}::{svc}")));
    }
    list
}

pub(crate) async fn run(sock: UdpSocket, inner: Arc<Inner>) {
    // Double initial burst — first packets routinely get dropped while
    // switches update their multicast tables.
    send_alive(&sock, &inner).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    send_alive(&sock, &inner).await;

    let mut interval = tokio::time::interval(ALIVE_INTERVAL);
    interval.tick().await; // consume the immediate first tick

    let mut buf = vec![0u8; 4096];
    loop {
        tokio::select! {
            _ = interval.tick() => send_alive(&sock, &inner).await,
            recv = sock.recv_from(&mut buf) => {
                if let Ok((n, src)) = recv {
                    let text = String::from_utf8_lossy(&buf[..n]).to_string();
                    handle_msearch(&sock, &inner, &text, src).await;
                }
            }
        }
    }
}

async fn send_alive(sock: &UdpSocket, inner: &Inner) {
    let udn = inner.udn();
    for (nt, usn) in targets(&udn) {
        let msg = format!(
            "NOTIFY * HTTP/1.1\r\n\
             HOST: {SSDP_ADDR}\r\n\
             CACHE-CONTROL: max-age={MAX_AGE_SECS}\r\n\
             LOCATION: {}\r\n\
             NT: {nt}\r\n\
             NTS: ssdp:alive\r\n\
             SERVER: {}\r\n\
             USN: {usn}\r\n\
             BOOTID.UPNP.ORG: 1\r\n\
             CONFIGID.UPNP.ORG: 1\r\n\
             \r\n",
            inner.location,
            Inner::server_header(),
        );
        if let Err(e) = sock.send_to(msg.as_bytes(), SSDP_ADDR).await {
            debug!(?e, "SSDP alive send failed");
        }
    }
}

async fn handle_msearch(sock: &UdpSocket, inner: &Inner, text: &str, src: SocketAddr) {
    if !text.starts_with("M-SEARCH") {
        return;
    }
    let Some(st) = header(text, "ST") else { return };
    if !header(text, "MAN").is_some_and(|m| m.contains("ssdp:discover")) {
        return;
    }

    let udn = inner.udn();
    let matched: Vec<(String, String)> = targets(&udn)
        .into_iter()
        .filter(|(nt, _)| st == "ssdp:all" || st == *nt)
        .collect();
    if matched.is_empty() {
        return;
    }
    debug!(%st, %src, "answering M-SEARCH");

    // A short fixed delay spreads replies without holding the receive loop
    // long enough to matter (the spec asks for 0..MX seconds of jitter).
    tokio::time::sleep(Duration::from_millis(40)).await;
    let date = chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT");
    for (nt, usn) in matched {
        let st_out = if st == "ssdp:all" { &nt } else { &st };
        let msg = format!(
            "HTTP/1.1 200 OK\r\n\
             CACHE-CONTROL: max-age={MAX_AGE_SECS}\r\n\
             DATE: {date}\r\n\
             EXT: \r\n\
             LOCATION: {}\r\n\
             SERVER: {}\r\n\
             ST: {st_out}\r\n\
             USN: {usn}\r\n\
             BOOTID.UPNP.ORG: 1\r\n\
             CONFIGID.UPNP.ORG: 1\r\n\
             \r\n",
            inner.location,
            Inner::server_header(),
        );
        if let Err(e) = sock.send_to(msg.as_bytes(), src).await {
            debug!(?e, %src, "M-SEARCH reply failed");
        }
    }
}

/// Multicast `ssdp:byebye` for every advertised target so control points
/// drop the device immediately instead of waiting out max-age.
pub(crate) async fn byebye(inner: &Inner) {
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0").await else {
        warn!("SSDP byebye skipped — no socket");
        return;
    };
    let udn = inner.udn();
    for (nt, usn) in targets(&udn) {
        let msg = format!(
            "NOTIFY * HTTP/1.1\r\n\
             HOST: {SSDP_ADDR}\r\n\
             NT: {nt}\r\n\
             NTS: ssdp:byebye\r\n\
             USN: {usn}\r\n\
             BOOTID.UPNP.ORG: 1\r\n\
             CONFIGID.UPNP.ORG: 1\r\n\
             \r\n"
        );
        let _ = sock.send_to(msg.as_bytes(), SSDP_ADDR).await;
    }
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
