//! UPnP description documents: the root device description plus the three
//! service SCPDs. Static except for friendlyName/UDN interpolation.

use crate::xml::encode_entities;

pub fn device_description(friendly_name: &str, uuid: &str) -> String {
    let name = encode_entities(friendly_name);
    let version = env!("CARGO_PKG_VERSION");
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
<specVersion><major>1</major><minor>0</minor></specVersion>
<device>
<deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
<friendlyName>{name}</friendlyName>
<manufacturer>tsirysndr</manufacturer>
<manufacturerURL>https://github.com/tsirysndr/fin</manufacturerURL>
<modelDescription>fin terminal media player — rockbox-playback audio, mpv video</modelDescription>
<modelName>fin</modelName>
<modelNumber>{version}</modelNumber>
<modelURL>https://github.com/tsirysndr/fin</modelURL>
<UDN>uuid:{uuid}</UDN>
<dlna:X_DLNADOC xmlns:dlna="urn:schemas-dlna-org:device-1-0">DMR-1.50</dlna:X_DLNADOC>
<serviceList>
<service>
<serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
<serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
<SCPDURL>/scpd/AVTransport.xml</SCPDURL>
<controlURL>/control/AVTransport</controlURL>
<eventSubURL>/event/AVTransport</eventSubURL>
</service>
<service>
<serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
<serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId>
<SCPDURL>/scpd/RenderingControl.xml</SCPDURL>
<controlURL>/control/RenderingControl</controlURL>
<eventSubURL>/event/RenderingControl</eventSubURL>
</service>
<service>
<serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
<serviceId>urn:upnp-org:serviceId:ConnectionManager</serviceId>
<SCPDURL>/scpd/ConnectionManager.xml</SCPDURL>
<controlURL>/control/ConnectionManager</controlURL>
<eventSubURL>/event/ConnectionManager</eventSubURL>
</service>
</serviceList>
</device>
</root>"#
    )
}

// SCPD helpers — the arg/var XML is highly repetitive, so build it from
// short tables instead of one wall of literal text.

fn action(name: &str, args: &[(&str, &str, &str)]) -> String {
    let list: String = args
        .iter()
        .map(|(n, dir, var)| {
            format!(
                "<argument><name>{n}</name><direction>{dir}</direction>\
                 <relatedStateVariable>{var}</relatedStateVariable></argument>"
            )
        })
        .collect();
    format!("<action><name>{name}</name><argumentList>{list}</argumentList></action>")
}

fn var(name: &str, ty: &str, send_events: bool, allowed: &[&str]) -> String {
    let values: String = if allowed.is_empty() {
        String::new()
    } else {
        let items: String = allowed
            .iter()
            .map(|v| format!("<allowedValue>{v}</allowedValue>"))
            .collect();
        format!("<allowedValueList>{items}</allowedValueList>")
    };
    format!(
        "<stateVariable sendEvents=\"{}\"><name>{name}</name><dataType>{ty}</dataType>{values}</stateVariable>",
        if send_events { "yes" } else { "no" }
    )
}

fn scpd(actions: String, vars: String) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
<specVersion><major>1</major><minor>0</minor></specVersion>
<actionList>{actions}</actionList>
<serviceStateTable>{vars}</serviceStateTable>
</scpd>"#
    )
}

pub fn av_transport_scpd() -> String {
    let id = ("InstanceID", "in", "A_ARG_TYPE_InstanceID");
    let actions = [
        action(
            "SetAVTransportURI",
            &[
                id,
                ("CurrentURI", "in", "AVTransportURI"),
                ("CurrentURIMetaData", "in", "AVTransportURIMetaData"),
            ],
        ),
        action(
            "SetNextAVTransportURI",
            &[
                id,
                ("NextURI", "in", "NextAVTransportURI"),
                ("NextURIMetaData", "in", "NextAVTransportURIMetaData"),
            ],
        ),
        action(
            "GetMediaInfo",
            &[
                id,
                ("NrTracks", "out", "NumberOfTracks"),
                ("MediaDuration", "out", "CurrentMediaDuration"),
                ("CurrentURI", "out", "AVTransportURI"),
                ("CurrentURIMetaData", "out", "AVTransportURIMetaData"),
                ("NextURI", "out", "NextAVTransportURI"),
                ("NextURIMetaData", "out", "NextAVTransportURIMetaData"),
                ("PlayMedium", "out", "PlaybackStorageMedium"),
                ("RecordMedium", "out", "RecordStorageMedium"),
                ("WriteStatus", "out", "RecordMediumWriteStatus"),
            ],
        ),
        action(
            "GetTransportInfo",
            &[
                id,
                ("CurrentTransportState", "out", "TransportState"),
                ("CurrentTransportStatus", "out", "TransportStatus"),
                ("CurrentSpeed", "out", "TransportPlaySpeed"),
            ],
        ),
        action(
            "GetPositionInfo",
            &[
                id,
                ("Track", "out", "CurrentTrack"),
                ("TrackDuration", "out", "CurrentTrackDuration"),
                ("TrackMetaData", "out", "CurrentTrackMetaData"),
                ("TrackURI", "out", "CurrentTrackURI"),
                ("RelTime", "out", "RelativeTimePosition"),
                ("AbsTime", "out", "AbsoluteTimePosition"),
                ("RelCount", "out", "RelativeCounterPosition"),
                ("AbsCount", "out", "AbsoluteCounterPosition"),
            ],
        ),
        action(
            "GetDeviceCapabilities",
            &[
                id,
                ("PlayMedia", "out", "PossiblePlaybackStorageMedia"),
                ("RecMedia", "out", "PossibleRecordStorageMedia"),
                ("RecQualityModes", "out", "PossibleRecordQualityModes"),
            ],
        ),
        action(
            "GetTransportSettings",
            &[
                id,
                ("PlayMode", "out", "CurrentPlayMode"),
                ("RecQualityMode", "out", "CurrentRecordQualityMode"),
            ],
        ),
        action("Stop", &[id]),
        action("Play", &[id, ("Speed", "in", "TransportPlaySpeed")]),
        action("Pause", &[id]),
        action(
            "Seek",
            &[
                id,
                ("Unit", "in", "A_ARG_TYPE_SeekMode"),
                ("Target", "in", "A_ARG_TYPE_SeekTarget"),
            ],
        ),
        action("Next", &[id]),
        action("Previous", &[id]),
    ]
    .concat();

    let vars = [
        var(
            "TransportState",
            "string",
            false,
            &[
                "STOPPED",
                "PAUSED_PLAYBACK",
                "PLAYING",
                "TRANSITIONING",
                "NO_MEDIA_PRESENT",
            ],
        ),
        var(
            "TransportStatus",
            "string",
            false,
            &["OK", "ERROR_OCCURRED"],
        ),
        var("PlaybackStorageMedium", "string", false, &[]),
        var("RecordStorageMedium", "string", false, &[]),
        var("PossiblePlaybackStorageMedia", "string", false, &[]),
        var("PossibleRecordStorageMedia", "string", false, &[]),
        var("CurrentPlayMode", "string", false, &["NORMAL"]),
        var("TransportPlaySpeed", "string", false, &["1"]),
        var("RecordMediumWriteStatus", "string", false, &[]),
        var("CurrentRecordQualityMode", "string", false, &[]),
        var("PossibleRecordQualityModes", "string", false, &[]),
        var("NumberOfTracks", "ui4", false, &[]),
        var("CurrentTrack", "ui4", false, &[]),
        var("CurrentTrackDuration", "string", false, &[]),
        var("CurrentMediaDuration", "string", false, &[]),
        var("CurrentTrackMetaData", "string", false, &[]),
        var("CurrentTrackURI", "string", false, &[]),
        var("AVTransportURI", "string", false, &[]),
        var("AVTransportURIMetaData", "string", false, &[]),
        var("NextAVTransportURI", "string", false, &[]),
        var("NextAVTransportURIMetaData", "string", false, &[]),
        var("RelativeTimePosition", "string", false, &[]),
        var("AbsoluteTimePosition", "string", false, &[]),
        var("RelativeCounterPosition", "i4", false, &[]),
        var("AbsoluteCounterPosition", "i4", false, &[]),
        var(
            "A_ARG_TYPE_SeekMode",
            "string",
            false,
            &["REL_TIME", "ABS_TIME"],
        ),
        var("A_ARG_TYPE_SeekTarget", "string", false, &[]),
        var("A_ARG_TYPE_InstanceID", "ui4", false, &[]),
        var("LastChange", "string", true, &[]),
    ]
    .concat();

    scpd(actions, vars)
}

pub fn rendering_control_scpd() -> String {
    let id = ("InstanceID", "in", "A_ARG_TYPE_InstanceID");
    let ch = ("Channel", "in", "A_ARG_TYPE_Channel");
    let actions = [
        action("GetVolume", &[id, ch, ("CurrentVolume", "out", "Volume")]),
        action("SetVolume", &[id, ch, ("DesiredVolume", "in", "Volume")]),
        action("GetMute", &[id, ch, ("CurrentMute", "out", "Mute")]),
        action("SetMute", &[id, ch, ("DesiredMute", "in", "Mute")]),
    ]
    .concat();
    let vars = [
        var("Volume", "ui2", false, &[]),
        var("Mute", "boolean", false, &[]),
        var("A_ARG_TYPE_Channel", "string", false, &["Master"]),
        var("A_ARG_TYPE_InstanceID", "ui4", false, &[]),
        var("LastChange", "string", true, &[]),
    ]
    .concat();
    scpd(actions, vars)
}

pub fn connection_manager_scpd() -> String {
    let actions = [
        action(
            "GetProtocolInfo",
            &[
                ("Source", "out", "SourceProtocolInfo"),
                ("Sink", "out", "SinkProtocolInfo"),
            ],
        ),
        action(
            "GetCurrentConnectionIDs",
            &[("ConnectionIDs", "out", "CurrentConnectionIDs")],
        ),
        action(
            "GetCurrentConnectionInfo",
            &[
                ("ConnectionID", "in", "A_ARG_TYPE_ConnectionID"),
                ("RcsID", "out", "A_ARG_TYPE_RcsID"),
                ("AVTransportID", "out", "A_ARG_TYPE_AVTransportID"),
                ("ProtocolInfo", "out", "A_ARG_TYPE_ProtocolInfo"),
                (
                    "PeerConnectionManager",
                    "out",
                    "A_ARG_TYPE_ConnectionManager",
                ),
                ("PeerConnectionID", "out", "A_ARG_TYPE_ConnectionID"),
                ("Direction", "out", "A_ARG_TYPE_Direction"),
                ("Status", "out", "A_ARG_TYPE_ConnectionStatus"),
            ],
        ),
    ]
    .concat();
    let vars = [
        var("SourceProtocolInfo", "string", true, &[]),
        var("SinkProtocolInfo", "string", true, &[]),
        var("CurrentConnectionIDs", "string", true, &[]),
        var("A_ARG_TYPE_ConnectionID", "i4", false, &[]),
        var("A_ARG_TYPE_RcsID", "i4", false, &[]),
        var("A_ARG_TYPE_AVTransportID", "i4", false, &[]),
        var("A_ARG_TYPE_ProtocolInfo", "string", false, &[]),
        var("A_ARG_TYPE_ConnectionManager", "string", false, &[]),
        var(
            "A_ARG_TYPE_Direction",
            "string",
            false,
            &["Input", "Output"],
        ),
        var("A_ARG_TYPE_ConnectionStatus", "string", false, &["OK"]),
    ]
    .concat();
    scpd(actions, vars)
}

/// Everything rockbox-playback (audio) or mpv (video) can realistically take.
/// Control points filter their "cast to" pickers on this list.
pub const SINK_PROTOCOL_INFO: &str = "http-get:*:audio/mpeg:*,\
http-get:*:audio/mp3:*,\
http-get:*:audio/flac:*,\
http-get:*:audio/x-flac:*,\
http-get:*:audio/ogg:*,\
http-get:*:application/ogg:*,\
http-get:*:audio/opus:*,\
http-get:*:audio/vorbis:*,\
http-get:*:audio/mp4:*,\
http-get:*:audio/m4a:*,\
http-get:*:audio/aac:*,\
http-get:*:audio/x-m4a:*,\
http-get:*:audio/wav:*,\
http-get:*:audio/x-wav:*,\
http-get:*:audio/aiff:*,\
http-get:*:audio/L16:*,\
http-get:*:video/mp4:*,\
http-get:*:video/x-matroska:*,\
http-get:*:video/webm:*,\
http-get:*:video/mpeg:*,\
http-get:*:video/avi:*,\
http-get:*:video/x-msvideo:*,\
http-get:*:video/quicktime:*,\
http-get:*:video/mp2t:*,\
http-get:*:video/x-ms-wmv:*,\
http-get:*:application/x-mpegURL:*";
