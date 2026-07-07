//! SOAP control endpoints — AVTransport, RenderingControl, ConnectionManager.
//!
//! This is where an external control point's actions turn into calls on the
//! shared `Renderer`: `SetAVTransportURI` + `Play` build a `QueueItem` whose
//! `is_video` flag makes `LocalRenderer` route audio to symphonia and video
//! to mpv; the transport/volume queries read the same live `PlaybackState`
//! the TUI shows.

use std::sync::Arc;

use tracing::{debug, warn};

use fin_player::{PlaybackStatus, QueueItem, UPNP_CAST_ID_PREFIX};

use crate::http::{Request, Response};
use crate::xml::{
    content_type, encode_entities, fmt_hms, is_video, parse_didl, parse_hms, tag_text,
    title_from_uri,
};
use crate::{desc, Inner};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Service {
    AvTransport,
    RenderingControl,
    ConnectionManager,
}

impl Service {
    pub fn service_type(&self) -> &'static str {
        match self {
            Self::AvTransport => "urn:schemas-upnp-org:service:AVTransport:1",
            Self::RenderingControl => "urn:schemas-upnp-org:service:RenderingControl:1",
            Self::ConnectionManager => "urn:schemas-upnp-org:service:ConnectionManager:1",
        }
    }

    pub fn from_control_path(path: &str) -> Option<Self> {
        match path {
            "/control/AVTransport" => Some(Self::AvTransport),
            "/control/RenderingControl" => Some(Self::RenderingControl),
            "/control/ConnectionManager" => Some(Self::ConnectionManager),
            _ => None,
        }
    }

    pub fn from_event_path(path: &str) -> Option<Self> {
        match path {
            "/event/AVTransport" => Some(Self::AvTransport),
            "/event/RenderingControl" => Some(Self::RenderingControl),
            "/event/ConnectionManager" => Some(Self::ConnectionManager),
            _ => None,
        }
    }
}

struct SoapError {
    code: u16,
    desc: &'static str,
}

impl SoapError {
    const INVALID_ACTION: Self = Self {
        code: 401,
        desc: "Invalid Action",
    };
    const INVALID_ARGS: Self = Self {
        code: 402,
        desc: "Invalid Args",
    };
    const ACTION_FAILED: Self = Self {
        code: 501,
        desc: "Action Failed",
    };
    const SEEK_MODE_NOT_SUPPORTED: Self = Self {
        code: 710,
        desc: "Seek mode not supported",
    };
}

pub(crate) async fn handle(inner: &Arc<Inner>, svc: Service, req: &Request) -> Response {
    // SOAPACTION: "urn:schemas-upnp-org:service:AVTransport:1#Play"
    let action = req
        .header("soapaction")
        .and_then(|v| v.trim_matches('"').rsplit('#').next().map(str::to_string))
        .unwrap_or_default();
    let body = String::from_utf8_lossy(&req.body).to_string();
    match dispatch(inner, svc, &action, &body).await {
        Ok(out_args) => {
            let envelope = format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body><u:{action}Response xmlns:u="{}">{out_args}</u:{action}Response></s:Body>
</s:Envelope>"#,
                svc.service_type()
            );
            Response::xml(200, "OK", envelope)
        }
        Err(e) => {
            debug!(?action, code = e.code, "SOAP fault");
            let envelope = format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring>
<detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>{}</errorCode><errorDescription>{}</errorDescription></UPnPError></detail>
</s:Fault></s:Body>
</s:Envelope>"#,
                e.code, e.desc
            );
            Response::xml(500, "Internal Server Error", envelope)
        }
    }
}

async fn dispatch(
    inner: &Arc<Inner>,
    svc: Service,
    action: &str,
    body: &str,
) -> Result<String, SoapError> {
    match svc {
        Service::AvTransport => av_transport(inner, action, body).await,
        Service::RenderingControl => rendering_control(inner, action, body).await,
        Service::ConnectionManager => connection_manager(action),
    }
}

fn arg(body: &str, name: &str) -> Option<String> {
    tag_text(body, name)
}

/// Build the queue item a cast URI turns into. The `upnp-cast:` id prefix
/// is what the TUI keys its "cast-in" badge off, and what keeps scrobbling
/// from reporting foreign ids to the media server.
fn build_item(inner: &Inner, uri: &str, meta_didl: &str) -> QueueItem {
    let meta = parse_didl(meta_didl);
    let video = is_video(&meta, uri);
    let seq = {
        let mut session = inner.session.lock();
        session.item_seq += 1;
        session.item_seq
    };
    QueueItem {
        id: format!("{UPNP_CAST_ID_PREFIX}{seq}"),
        title: meta.title.clone().unwrap_or_else(|| title_from_uri(uri)),
        subtitle: meta.artist.clone().unwrap_or_default(),
        stream_url: uri.to_string(),
        image_url: meta.album_art.clone(),
        duration_secs: meta.duration_secs,
        is_video: video,
        content_type: content_type(&meta, uri, video),
    }
}

pub(crate) fn transport_state(status: PlaybackStatus) -> &'static str {
    match status {
        PlaybackStatus::Playing => "PLAYING",
        PlaybackStatus::Paused => "PAUSED_PLAYBACK",
        PlaybackStatus::Buffering => "TRANSITIONING",
        PlaybackStatus::Stopped | PlaybackStatus::Idle => "STOPPED",
    }
}

async fn av_transport(inner: &Arc<Inner>, action: &str, body: &str) -> Result<String, SoapError> {
    let renderer = inner.renderer();
    match action {
        "SetAVTransportURI" => {
            let uri = arg(body, "CurrentURI").ok_or(SoapError::INVALID_ARGS)?;
            let meta = arg(body, "CurrentURIMetaData").unwrap_or_default();
            let item = build_item(inner, &uri, &meta);
            debug!(
                title = %item.title,
                video = item.is_video,
                "UPnP cast-in: SetAVTransportURI"
            );
            let playing = matches!(
                renderer.state().status,
                PlaybackStatus::Playing | PlaybackStatus::Buffering
            );
            {
                let mut s = inner.session.lock();
                s.current_uri = uri;
                s.current_meta = meta;
                s.next_uri.clear();
                s.next_meta.clear();
                if playing {
                    s.current = Some(item.clone());
                    s.pending = None;
                } else {
                    s.pending = Some(item.clone());
                }
            }
            // Mid-playback URI swap means "switch tracks now" — most control
            // points otherwise send Stop → SetAVTransportURI → Play.
            if playing {
                renderer
                    .play(vec![item], 0)
                    .await
                    .map_err(|e| action_failed(e, "play"))?;
            }
            Ok(String::new())
        }
        "SetNextAVTransportURI" => {
            let uri = arg(body, "NextURI").ok_or(SoapError::INVALID_ARGS)?;
            let meta = arg(body, "NextURIMetaData").unwrap_or_default();
            let item = build_item(inner, &uri, &meta);
            {
                let mut s = inner.session.lock();
                s.next_uri = uri;
                s.next_meta = meta;
            }
            // Enqueue locally — the renderer's own auto-advance gives the
            // control point its gapless transition.
            renderer
                .enqueue(vec![item])
                .await
                .map_err(|e| action_failed(e, "enqueue"))?;
            Ok(String::new())
        }
        "Play" => {
            let (pending, current) = {
                let mut s = inner.session.lock();
                let p = s.pending.take();
                if let Some(item) = &p {
                    s.current = Some(item.clone());
                }
                (p, s.current.clone())
            };
            if let Some(item) = pending {
                renderer
                    .play(vec![item], 0)
                    .await
                    .map_err(|e| action_failed(e, "play"))?;
            } else {
                match renderer.state().status {
                    PlaybackStatus::Paused => renderer
                        .resume()
                        .await
                        .map_err(|e| action_failed(e, "resume"))?,
                    PlaybackStatus::Stopped | PlaybackStatus::Idle => {
                        if let Some(item) = current {
                            renderer
                                .play(vec![item], 0)
                                .await
                                .map_err(|e| action_failed(e, "play"))?;
                        }
                    }
                    _ => {}
                }
            }
            Ok(String::new())
        }
        "Pause" => {
            renderer
                .pause()
                .await
                .map_err(|e| action_failed(e, "pause"))?;
            Ok(String::new())
        }
        "Stop" => {
            renderer
                .stop()
                .await
                .map_err(|e| action_failed(e, "stop"))?;
            Ok(String::new())
        }
        "Seek" => {
            let unit = arg(body, "Unit").unwrap_or_default();
            if unit != "REL_TIME" && unit != "ABS_TIME" {
                return Err(SoapError::SEEK_MODE_NOT_SUPPORTED);
            }
            let target = arg(body, "Target")
                .as_deref()
                .and_then(parse_hms)
                .ok_or(SoapError::INVALID_ARGS)?;
            renderer
                .seek(target)
                .await
                .map_err(|e| action_failed(e, "seek"))?;
            Ok(String::new())
        }
        "Next" => {
            renderer
                .next()
                .await
                .map_err(|e| action_failed(e, "next"))?;
            Ok(String::new())
        }
        "Previous" => {
            renderer
                .previous()
                .await
                .map_err(|e| action_failed(e, "previous"))?;
            Ok(String::new())
        }
        "GetTransportInfo" => {
            let state = renderer.state();
            Ok(format!(
                "<CurrentTransportState>{}</CurrentTransportState>\
                 <CurrentTransportStatus>OK</CurrentTransportStatus>\
                 <CurrentSpeed>1</CurrentSpeed>",
                transport_state(state.status)
            ))
        }
        "GetPositionInfo" => {
            let state = renderer.state();
            let (uri, meta) = {
                let s = inner.session.lock();
                (s.current_uri.clone(), s.current_meta.clone())
            };
            // Prefer live renderer info (covers TUI-initiated playback the
            // control point is merely observing), fall back to the session.
            let (track_uri, duration) = match &state.now_playing {
                Some(item) => (
                    item.stream_url.clone(),
                    if state.duration_secs > 0.0 {
                        state.duration_secs
                    } else {
                        item.duration_secs.unwrap_or(0) as f64
                    },
                ),
                None => (uri, 0.0),
            };
            let track = state
                .current_index
                .map(|i| i + 1)
                .unwrap_or(usize::from(state.now_playing.is_some()));
            Ok(format!(
                "<Track>{track}</Track>\
                 <TrackDuration>{}</TrackDuration>\
                 <TrackMetaData>{}</TrackMetaData>\
                 <TrackURI>{}</TrackURI>\
                 <RelTime>{}</RelTime>\
                 <AbsTime>NOT_IMPLEMENTED</AbsTime>\
                 <RelCount>2147483647</RelCount>\
                 <AbsCount>2147483647</AbsCount>",
                fmt_hms(duration),
                encode_entities(&meta),
                encode_entities(&track_uri),
                fmt_hms(state.position_secs),
            ))
        }
        "GetMediaInfo" => {
            let state = renderer.state();
            let s = inner.session.lock();
            Ok(format!(
                "<NrTracks>{}</NrTracks>\
                 <MediaDuration>{}</MediaDuration>\
                 <CurrentURI>{}</CurrentURI>\
                 <CurrentURIMetaData>{}</CurrentURIMetaData>\
                 <NextURI>{}</NextURI>\
                 <NextURIMetaData>{}</NextURIMetaData>\
                 <PlayMedium>NETWORK</PlayMedium>\
                 <RecordMedium>NOT_IMPLEMENTED</RecordMedium>\
                 <WriteStatus>NOT_IMPLEMENTED</WriteStatus>",
                state
                    .queue
                    .len()
                    .max(usize::from(!s.current_uri.is_empty())),
                fmt_hms(state.duration_secs),
                encode_entities(&s.current_uri),
                encode_entities(&s.current_meta),
                encode_entities(&s.next_uri),
                encode_entities(&s.next_meta),
            ))
        }
        "GetDeviceCapabilities" => Ok("<PlayMedia>NETWORK</PlayMedia>\
             <RecMedia>NOT_IMPLEMENTED</RecMedia>\
             <RecQualityModes>NOT_IMPLEMENTED</RecQualityModes>"
            .to_string()),
        "GetTransportSettings" => Ok("<PlayMode>NORMAL</PlayMode>\
             <RecQualityMode>NOT_IMPLEMENTED</RecQualityMode>"
            .to_string()),
        _ => Err(SoapError::INVALID_ACTION),
    }
}

async fn rendering_control(
    inner: &Arc<Inner>,
    action: &str,
    body: &str,
) -> Result<String, SoapError> {
    let renderer = inner.renderer();
    match action {
        "GetVolume" => {
            let vol = (renderer.state().volume.clamp(0.0, 1.0) * 100.0).round() as i32;
            Ok(format!("<CurrentVolume>{vol}</CurrentVolume>"))
        }
        "SetVolume" => {
            let vol: f32 = arg(body, "DesiredVolume")
                .and_then(|v| v.parse().ok())
                .ok_or(SoapError::INVALID_ARGS)?;
            inner.session.lock().pre_mute_volume = None;
            renderer
                .set_volume((vol / 100.0).clamp(0.0, 1.0))
                .await
                .map_err(|e| action_failed(e, "set_volume"))?;
            Ok(String::new())
        }
        "GetMute" => {
            let muted = inner.session.lock().pre_mute_volume.is_some();
            Ok(format!("<CurrentMute>{}</CurrentMute>", u8::from(muted)))
        }
        "SetMute" => {
            let want_mute = matches!(
                arg(body, "DesiredMute").as_deref(),
                Some("1") | Some("true") | Some("True") | Some("yes")
            );
            let (apply, target) = {
                let mut s = inner.session.lock();
                match (want_mute, s.pre_mute_volume) {
                    (true, None) => {
                        let cur = renderer.state().volume;
                        s.pre_mute_volume = Some(cur);
                        (true, 0.0)
                    }
                    (false, Some(saved)) => {
                        s.pre_mute_volume = None;
                        (true, saved)
                    }
                    _ => (false, 0.0),
                }
            };
            if apply {
                renderer
                    .set_volume(target)
                    .await
                    .map_err(|e| action_failed(e, "set_volume"))?;
            }
            Ok(String::new())
        }
        _ => Err(SoapError::INVALID_ACTION),
    }
}

fn connection_manager(action: &str) -> Result<String, SoapError> {
    match action {
        "GetProtocolInfo" => Ok(format!(
            "<Source></Source><Sink>{}</Sink>",
            desc::SINK_PROTOCOL_INFO
        )),
        "GetCurrentConnectionIDs" => Ok("<ConnectionIDs>0</ConnectionIDs>".to_string()),
        "GetCurrentConnectionInfo" => Ok("<RcsID>0</RcsID>\
             <AVTransportID>0</AVTransportID>\
             <ProtocolInfo></ProtocolInfo>\
             <PeerConnectionManager></PeerConnectionManager>\
             <PeerConnectionID>-1</PeerConnectionID>\
             <Direction>Input</Direction>\
             <Status>OK</Status>"
            .to_string()),
        _ => Err(SoapError::INVALID_ACTION),
    }
}

fn action_failed(e: anyhow::Error, what: &str) -> SoapError {
    warn!(?e, what, "renderer call failed for UPnP control point");
    SoapError::ACTION_FAILED
}
