//! Small XML + DIDL-Lite helpers. Same hand-rolled approach as
//! `fin_player::upnp` — the documents are tiny and predictable.

pub fn encode_entities(s: &str) -> String {
    // `&` first so we don't double-encode our own replacements.
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Extract the text content of the first `<tag>…</tag>` pair, matching on
/// the *local* name only (namespace prefixes vary between control points).
/// Returns entity-decoded text.
pub fn tag_text(xml: &str, local_name: &str) -> Option<String> {
    let lower = xml.to_ascii_lowercase();
    let needle = local_name.to_ascii_lowercase();
    let mut cursor = 0;
    while cursor < lower.len() {
        let lt = lower[cursor..].find('<')?;
        let abs = cursor + lt + 1;
        let rest = &lower[abs..];
        if rest.starts_with('/') || rest.starts_with('!') || rest.starts_with('?') {
            cursor = abs;
            continue;
        }
        let name_end = rest
            .find(|c: char| c == ' ' || c == '>' || c == '/' || c == '\t' || c == '\n')
            .unwrap_or(rest.len());
        let tag_name = &rest[..name_end];
        let local = tag_name.rsplit(':').next().unwrap_or(tag_name);
        if local == needle {
            let gt = rest.find('>')?;
            if rest[..gt].ends_with('/') {
                // Self-closing tag — empty content.
                return Some(String::new());
            }
            let content_start = abs + gt + 1;
            let close_needle = format!("</{tag_name}>");
            let close_rel = lower[content_start..].find(&close_needle)?;
            return Some(decode_entities(
                xml[content_start..content_start + close_rel].trim(),
            ));
        }
        cursor = abs + name_end;
    }
    None
}

/// First occurrence of `name="value"` anywhere in the document — good
/// enough for the two attributes we care about (`protocolInfo`, `duration`)
/// since a cast payload holds exactly one `<res>`.
pub fn attr_anywhere(xml: &str, name: &str) -> Option<String> {
    let lower = xml.to_ascii_lowercase();
    let needle = format!("{}=\"", name.to_ascii_lowercase());
    let start = lower.find(&needle)? + needle.len();
    let end = start + lower[start..].find('"')?;
    Some(decode_entities(&xml[start..end]))
}

pub fn fmt_hms(secs: f64) -> String {
    let s = if secs.is_finite() && secs > 0.0 {
        secs as u64
    } else {
        0
    };
    format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// Parse `H:MM:SS[.fff]` (also tolerates `MM:SS`).
pub fn parse_hms(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() || s == "NOT_IMPLEMENTED" {
        return None;
    }
    let parts: Vec<f64> = s
        .split(':')
        .map(|p| p.parse::<f64>().ok())
        .collect::<Option<_>>()?;
    match parts[..] {
        [h, m, sec] => Some(h * 3600.0 + m * 60.0 + sec),
        [m, sec] => Some(m * 60.0 + sec),
        [sec] => Some(sec),
        _ => None,
    }
}

/// What we could pull out of the control point's DIDL-Lite metadata.
#[derive(Debug, Default)]
pub struct CastMeta {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub upnp_class: Option<String>,
    pub album_art: Option<String>,
    pub duration_secs: Option<u64>,
    /// MIME from `<res protocolInfo="http-get:*:MIME:*">`.
    pub mime: Option<String>,
}

pub fn parse_didl(didl: &str) -> CastMeta {
    let mime = attr_anywhere(didl, "protocolInfo")
        .and_then(|p| p.split(':').nth(2).map(|m| m.trim().to_string()))
        .filter(|m| !m.is_empty() && m != "*");
    CastMeta {
        title: tag_text(didl, "title").filter(|t| !t.is_empty()),
        artist: tag_text(didl, "artist")
            .or_else(|| tag_text(didl, "creator"))
            .filter(|t| !t.is_empty()),
        upnp_class: tag_text(didl, "class").filter(|t| !t.is_empty()),
        album_art: tag_text(didl, "albumArtURI").filter(|t| !t.is_empty()),
        duration_secs: attr_anywhere(didl, "duration")
            .and_then(|d| parse_hms(&d))
            .map(|d| d as u64),
        mime,
    }
}

/// Audio vs video decides the whole downstream path (symphonia vs mpv), so
/// check every signal in confidence order: DIDL `upnp:class`, then the
/// protocolInfo MIME, then the URL extension. Unknown defaults to audio —
/// symphonia fails fast and loud, mpv would pop a window.
pub fn is_video(meta: &CastMeta, uri: &str) -> bool {
    if let Some(class) = &meta.upnp_class {
        if class.contains("videoItem") {
            return true;
        }
        if class.contains("audioItem") || class.contains("musicTrack") {
            return false;
        }
    }
    if let Some(mime) = &meta.mime {
        if mime.starts_with("video/") {
            return true;
        }
        if mime.starts_with("audio/") {
            return false;
        }
    }
    matches!(
        extension(uri).as_deref(),
        Some(
            "mp4"
                | "m4v"
                | "mkv"
                | "webm"
                | "avi"
                | "mov"
                | "ts"
                | "mpg"
                | "mpeg"
                | "wmv"
                | "m3u8"
                | "flv"
        )
    )
}

pub fn content_type(meta: &CastMeta, uri: &str, video: bool) -> String {
    if let Some(mime) = &meta.mime {
        if mime.contains('/') {
            return mime.clone();
        }
    }
    let ext = extension(uri).unwrap_or_default();
    let guessed = match ext.as_str() {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "m4a" | "m4b" => "audio/mp4",
        "aac" => "audio/aac",
        "wav" => "audio/wav",
        "aif" | "aiff" => "audio/aiff",
        "wma" => "audio/x-ms-wma",
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "ts" => "video/mp2t",
        "mpg" | "mpeg" => "video/mpeg",
        "wmv" => "video/x-ms-wmv",
        "m3u8" => "application/x-mpegURL",
        _ => {
            if video {
                "video/mp4"
            } else {
                "audio/mpeg"
            }
        }
    };
    guessed.to_string()
}

/// Lowercased extension of the URL path, query string stripped.
fn extension(uri: &str) -> Option<String> {
    let path = uri.split(['?', '#']).next().unwrap_or(uri);
    let seg = path.rsplit('/').next()?;
    let (_, ext) = seg.rsplit_once('.')?;
    if ext.is_empty() || ext.len() > 5 {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

/// Fallback display title: the last URL path segment, percent-decoded.
pub fn title_from_uri(uri: &str) -> String {
    let no_query = uri.split(['?', '#']).next().unwrap_or(uri);
    // Cut `scheme://host` first so a bare-root URL doesn't yield the
    // hostname as a "title".
    let path = no_query
        .split_once("://")
        .and_then(|(_, rest)| rest.find('/').map(|i| &rest[i + 1..]))
        .unwrap_or(no_query);
    let seg = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let decoded = percent_encoding::percent_decode_str(seg)
        .decode_utf8()
        .map(|c| c.to_string())
        .unwrap_or_else(|_| seg.to_string());
    if decoded.is_empty() {
        "UPnP stream".to_string()
    } else {
        decoded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIDL: &str = r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><item id="1" parentID="0" restricted="1"><dc:title>So What</dc:title><upnp:artist>Miles Davis</upnp:artist><upnp:class>object.item.audioItem.musicTrack</upnp:class><res duration="0:09:22.000" protocolInfo="http-get:*:audio/flac:*">http://srv/track.flac</res></item></DIDL-Lite>"#;

    #[test]
    fn didl_roundtrip() {
        let m = parse_didl(DIDL);
        assert_eq!(m.title.as_deref(), Some("So What"));
        assert_eq!(m.artist.as_deref(), Some("Miles Davis"));
        assert_eq!(m.mime.as_deref(), Some("audio/flac"));
        assert_eq!(m.duration_secs, Some(562));
        assert!(!is_video(&m, "http://srv/track.flac"));
        assert_eq!(
            content_type(&m, "http://srv/track.flac", false),
            "audio/flac"
        );
    }

    #[test]
    fn video_class_wins_over_extension() {
        let didl = DIDL.replace("object.item.audioItem.musicTrack", "object.item.videoItem");
        let m = parse_didl(&didl);
        assert!(is_video(&m, "http://srv/track.flac"));
    }

    #[test]
    fn bare_uri_classification() {
        let m = CastMeta::default();
        assert!(is_video(&m, "http://srv/movie.mkv?token=a.b"));
        assert!(!is_video(&m, "http://srv/song.mp3"));
        assert_eq!(
            content_type(&m, "http://srv/movie.mkv", true),
            "video/x-matroska"
        );
    }

    #[test]
    fn hms_helpers() {
        assert_eq!(fmt_hms(562.9), "0:09:22");
        assert_eq!(parse_hms("1:02:03"), Some(3723.0));
        assert_eq!(parse_hms("NOT_IMPLEMENTED"), None);
    }

    #[test]
    fn title_fallback_decodes_percent_escapes() {
        assert_eq!(title_from_uri("http://s/a/My%20Song.mp3"), "My Song.mp3");
        assert_eq!(title_from_uri("http://s/"), "UPnP stream");
    }
}
