//! GENA eventing — SUBSCRIBE / UNSUBSCRIBE plus `LastChange` NOTIFYs.
//!
//! Control points subscribe to AVTransport / RenderingControl events to keep
//! their UI in sync without polling. We poll the renderer once a second and
//! push a full `LastChange` snapshot whenever anything in it moved — simpler
//! than per-variable diffing and every control point accepts it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use crate::http::{Request, Response};
use crate::soap::Service;
use crate::xml::encode_entities;
use crate::{desc, Inner};

const TIMEOUT_SECS: u64 = 1800;

pub(crate) struct Subscription {
    pub sid: String,
    pub service: Service,
    pub callbacks: Vec<String>,
    pub expires: Instant,
    pub seq: u32,
}

pub(crate) fn handle(inner: &Arc<Inner>, svc: Service, req: &Request) -> Response {
    match req.method.as_str() {
        "SUBSCRIBE" => subscribe(inner, svc, req),
        "UNSUBSCRIBE" => unsubscribe(inner, req),
        _ => Response::empty(405, "Method Not Allowed"),
    }
}

fn subscribe(inner: &Arc<Inner>, svc: Service, req: &Request) -> Response {
    // Renewal — the control point sends its SID back, no CALLBACK.
    if let Some(sid) = req.header("sid") {
        let mut subs = inner.subs.lock();
        match subs.iter_mut().find(|s| s.sid == sid) {
            Some(sub) => {
                sub.expires = Instant::now() + Duration::from_secs(TIMEOUT_SECS);
                return sub_ok(&sub.sid);
            }
            None => return Response::empty(412, "Precondition Failed"),
        }
    }

    let Some(callback) = req.header("callback") else {
        return Response::empty(412, "Precondition Failed");
    };
    let callbacks: Vec<String> = callback
        .split('>')
        .filter_map(|part| {
            let url = part
                .trim()
                .trim_start_matches(',')
                .trim()
                .strip_prefix('<')?;
            url.starts_with("http").then(|| url.to_string())
        })
        .collect();
    if callbacks.is_empty() {
        return Response::empty(412, "Precondition Failed");
    }

    let sid = format!("uuid:{}", uuid::Uuid::new_v4());
    inner.subs.lock().push(Subscription {
        sid: sid.clone(),
        service: svc,
        callbacks: callbacks.clone(),
        expires: Instant::now() + Duration::from_secs(TIMEOUT_SECS),
        seq: 1, // 0 goes out with the initial state NOTIFY below
    });
    debug!(%sid, ?svc, "GENA subscribe");

    // The spec requires an initial NOTIFY carrying the full current state,
    // SEQ 0, sent right after the subscribe response.
    let inner2 = inner.clone();
    let sid2 = sid.clone();
    tokio::spawn(async move {
        let body = match svc {
            Service::AvTransport => av_propertyset(&inner2),
            Service::RenderingControl => rc_propertyset(&inner2),
            Service::ConnectionManager => cm_propertyset(),
        };
        send_notify(&inner2, &callbacks, &sid2, 0, &body).await;
    });

    sub_ok(&sid)
}

fn sub_ok(sid: &str) -> Response {
    let mut r = Response::empty(200, "OK");
    r.headers.push(("SID".into(), sid.to_string()));
    r.headers
        .push(("TIMEOUT".into(), format!("Second-{TIMEOUT_SECS}")));
    r
}

fn unsubscribe(inner: &Arc<Inner>, req: &Request) -> Response {
    let Some(sid) = req.header("sid") else {
        return Response::empty(412, "Precondition Failed");
    };
    let mut subs = inner.subs.lock();
    let before = subs.len();
    subs.retain(|s| s.sid != sid);
    if subs.len() < before {
        Response::empty(200, "OK")
    } else {
        Response::empty(412, "Precondition Failed")
    }
}

/// What the AVTransport LastChange is built from — one comparable snapshot
/// per poll tick.
#[derive(PartialEq, Clone, Default)]
struct AvSnapshot {
    transport: &'static str,
    uri: String,
    duration: String,
    n_tracks: usize,
    track: usize,
}

#[derive(PartialEq, Clone, Default)]
struct RcSnapshot {
    volume: i32,
    muted: bool,
}

pub(crate) async fn notify_loop(inner: Arc<Inner>) {
    let mut last_av = AvSnapshot::default();
    let mut last_rc = RcSnapshot::default();
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        interval.tick().await;
        // Drop expired subscriptions first so we don't notify ghosts.
        inner.subs.lock().retain(|s| s.expires > Instant::now());

        let av = av_snapshot(&inner);
        if av != last_av {
            last_av = av;
            let body = av_propertyset(&inner);
            notify_service(&inner, Service::AvTransport, &body).await;
        }
        let rc = rc_snapshot(&inner);
        if rc != last_rc {
            last_rc = rc;
            let body = rc_propertyset(&inner);
            notify_service(&inner, Service::RenderingControl, &body).await;
        }
    }
}

fn av_snapshot(inner: &Inner) -> AvSnapshot {
    let state = inner.renderer().state();
    AvSnapshot {
        transport: crate::soap::transport_state(state.status),
        uri: state
            .now_playing
            .as_ref()
            .map(|i| i.stream_url.clone())
            .unwrap_or_default(),
        duration: crate::xml::fmt_hms(state.duration_secs),
        n_tracks: state.queue.len(),
        track: state.current_index.map(|i| i + 1).unwrap_or(0),
    }
}

fn rc_snapshot(inner: &Inner) -> RcSnapshot {
    RcSnapshot {
        volume: (inner.renderer().state().volume.clamp(0.0, 1.0) * 100.0).round() as i32,
        muted: inner.session.lock().pre_mute_volume.is_some(),
    }
}

/// `<e:propertyset>` wrapper with the LastChange event doc entity-escaped
/// inside, as GENA requires.
fn propertyset(last_change_event: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0"><e:property><LastChange>{}</LastChange></e:property></e:propertyset>"#,
        encode_entities(last_change_event)
    )
}

fn av_propertyset(inner: &Inner) -> String {
    let snap = av_snapshot(inner);
    let (uri, meta) = {
        let s = inner.session.lock();
        (s.current_uri.clone(), s.current_meta.clone())
    };
    let track_uri = if snap.uri.is_empty() {
        uri
    } else {
        snap.uri.clone()
    };
    let event = format!(
        r#"<Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT/"><InstanceID val="0"><TransportState val="{}"/><TransportStatus val="OK"/><CurrentPlayMode val="NORMAL"/><NumberOfTracks val="{}"/><CurrentTrack val="{}"/><CurrentTrackDuration val="{}"/><CurrentTrackURI val="{}"/><CurrentTrackMetaData val="{}"/><AVTransportURI val="{}"/><AVTransportURIMetaData val="{}"/></InstanceID></Event>"#,
        snap.transport,
        snap.n_tracks,
        snap.track,
        snap.duration,
        encode_entities(&track_uri),
        encode_entities(&meta),
        encode_entities(&track_uri),
        encode_entities(&meta),
    );
    propertyset(&event)
}

fn rc_propertyset(inner: &Inner) -> String {
    let snap = rc_snapshot(inner);
    let event = format!(
        r#"<Event xmlns="urn:schemas-upnp-org:metadata-1-0/RCS/"><InstanceID val="0"><Volume channel="Master" val="{}"/><Mute channel="Master" val="{}"/></InstanceID></Event>"#,
        snap.volume,
        u8::from(snap.muted),
    );
    propertyset(&event)
}

/// ConnectionManager events its plain variables directly (no LastChange).
fn cm_propertyset() -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0"><e:property><SourceProtocolInfo></SourceProtocolInfo></e:property><e:property><SinkProtocolInfo>{}</SinkProtocolInfo></e:property><e:property><CurrentConnectionIDs>0</CurrentConnectionIDs></e:property></e:propertyset>"#,
        encode_entities(desc::SINK_PROTOCOL_INFO)
    )
}

async fn notify_service(inner: &Arc<Inner>, svc: Service, body: &str) {
    // Snapshot targets under the lock, send without it.
    let targets: Vec<(String, Vec<String>, u32)> = {
        let mut subs = inner.subs.lock();
        subs.iter_mut()
            .filter(|s| s.service == svc)
            .map(|s| {
                let seq = s.seq;
                s.seq = s.seq.wrapping_add(1);
                (s.sid.clone(), s.callbacks.clone(), seq)
            })
            .collect()
    };
    for (sid, callbacks, seq) in targets {
        if !send_notify(inner, &callbacks, &sid, seq, body).await {
            // Callback gone — the control point exited without unsubscribing.
            inner.subs.lock().retain(|s| s.sid != sid);
        }
    }
}

async fn send_notify(inner: &Inner, callbacks: &[String], sid: &str, seq: u32, body: &str) -> bool {
    let method = reqwest::Method::from_bytes(b"NOTIFY").expect("static method name");
    for cb in callbacks {
        let res = inner
            .client
            .request(method.clone(), cb)
            .header("CONTENT-TYPE", "text/xml; charset=\"utf-8\"")
            .header("NT", "upnp:event")
            .header("NTS", "upnp:propchange")
            .header("SID", sid)
            .header("SEQ", seq.to_string())
            .body(body.to_string())
            .send()
            .await;
        match res {
            Ok(_) => return true,
            Err(e) => warn!(?e, %cb, "GENA notify failed"),
        }
    }
    false
}
