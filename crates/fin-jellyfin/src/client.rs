use anyhow::{Context, Result};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde::Serialize;
use serde_json::json;
use tracing::debug;
use uuid::Uuid;

use crate::models::{AuthResult, BaseItem, SearchHint, SearchResult, UserViewsResult};

/// Pick the URL extension for a direct stream.
///
/// Preference: whatever Jellyfin reported in `Container`. When that's absent
/// (e.g. items came from `/Search/Hints`, which doesn't include container
/// info), fall back to a receiver-friendly default. We deliberately do NOT
/// hardcode `mp3` / `mp4` — Jellyfin honors any container extension it
/// supports (`flac`, `ogg`, `opus`, `webm`, `mkv`, …).
fn source_container(item: &BaseItem, is_audio: bool) -> String {
    if let Some(c) = item.container.as_deref() {
        // Jellyfin sometimes returns a comma-separated list ("mp3,mpeg"),
        // pick the first entry.
        if let Some(first) = c.split(',').next() {
            let first = first.trim();
            if !first.is_empty() {
                return first.to_ascii_lowercase();
            }
        }
    }
    if is_audio {
        "mp3".into()
    } else {
        "mp4".into()
    }
}

/// Normalize every JSON object key to PascalCase in place.
///
/// Different Jellyfin versions ship different casings — 10.8 sends PascalCase
/// (`Items`, `Id`), 10.10+ ships camelCase in the OpenAPI schema (`items`,
/// `id`), and some proxies mangle things further. Our models are all
/// `rename_all = "PascalCase"`, so we upcase the first letter of every key
/// before decoding.
fn normalize_keys(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let entries: Vec<_> = std::mem::take(map).into_iter().collect();
            for (k, mut v) in entries {
                normalize_keys(&mut v);
                let mut chars = k.chars();
                let key = match chars.next() {
                    Some(c) => c.to_uppercase().chain(chars).collect::<String>(),
                    None => k,
                };
                map.insert(key, v);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                normalize_keys(v);
            }
        }
        _ => {}
    }
}

fn parse_json_lenient(body: &str) -> Option<serde_json::Value> {
    let mut v = serde_json::from_str::<serde_json::Value>(body).ok()?;
    normalize_keys(&mut v);
    Some(v)
}

/// Deserialize `SearchHints` (Jellyfin's canonical shape) OR a bare array —
/// after key normalization above, both PascalCase and camelCase responses
/// arrive here with the same shape.
fn parse_search_hint_body(body: &str) -> Vec<SearchHint> {
    let Some(v) = parse_json_lenient(body) else {
        return Vec::new();
    };
    let hints = v.get("SearchHints").or_else(|| v.get("Hints")).or({
        if v.is_array() {
            Some(&v)
        } else {
            None
        }
    });
    let Some(hints) = hints else {
        return Vec::new();
    };
    if hints.is_null() {
        return Vec::new();
    }
    serde_json::from_value::<Vec<SearchHint>>(hints.clone()).unwrap_or_default()
}

/// Pull `TotalRecordCount` out of a `/Users/{id}/Items` response (or 0 if
/// the field isn't there). Used by paginated `items()` to know when to stop.
fn parse_total_record_count(body: &str) -> i64 {
    let Some(v) = parse_json_lenient(body) else {
        return 0;
    };
    v.get("TotalRecordCount")
        .and_then(|n| n.as_i64())
        .unwrap_or(0)
}

/// Same forgiving parse for `/Users/{id}/Items` responses.
fn parse_items_body(body: &str) -> Vec<BaseItem> {
    let Some(v) = parse_json_lenient(body) else {
        return Vec::new();
    };
    let items = v.get("Items").or({
        if v.is_array() {
            Some(&v)
        } else {
            None
        }
    });
    let Some(items) = items else {
        return Vec::new();
    };
    if items.is_null() {
        return Vec::new();
    }
    serde_json::from_value::<Vec<BaseItem>>(items.clone()).unwrap_or_default()
}

const CLIENT_NAME: &str = "fin";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy)]
pub enum StreamFormat {
    /// Direct/original stream if possible.
    Direct,
    /// HLS transcoded (useful for Chromecast).
    Hls,
}

#[derive(Debug, Clone)]
pub struct JellyfinClient {
    base_url: String,
    device_id: String,
    device_name: String,
    access_token: Option<String>,
    user_id: Option<String>,
    http: reqwest::Client,
}

impl JellyfinClient {
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .user_agent(format!("{}/{}", CLIENT_NAME, CLIENT_VERSION))
            .connect_timeout(std::time::Duration::from_secs(5))
            // A single request can be a full library listing on a big
            // server — give it plenty of headroom. Pagination keeps any
            // individual page well under this.
            .timeout(std::time::Duration::from_secs(120))
            .build()?;
        let device_name = whoami::fallible::hostname().unwrap_or_else(|_| "fin-cli".to_string());
        Ok(Self {
            base_url,
            device_id: Uuid::new_v4().to_string(),
            device_name,
            access_token: None,
            user_id: None,
            http,
        })
    }

    pub fn with_credentials(
        base_url: impl Into<String>,
        device_id: impl Into<String>,
        user_id: impl Into<String>,
        access_token: impl Into<String>,
    ) -> Result<Self> {
        let mut c = Self::new(base_url)?;
        c.device_id = device_id.into();
        c.user_id = Some(user_id.into());
        c.access_token = Some(access_token.into());
        Ok(c)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn access_token(&self) -> Option<&str> {
        self.access_token.as_deref()
    }

    pub fn user_id(&self) -> Option<&str> {
        self.user_id.as_deref()
    }

    fn auth_header(&self) -> String {
        let token = self
            .access_token
            .as_deref()
            .map(|t| format!(", Token=\"{}\"", t))
            .unwrap_or_default();
        format!(
            "MediaBrowser Client=\"{}\", Device=\"{}\", DeviceId=\"{}\", Version=\"{}\"{}",
            CLIENT_NAME, self.device_name, self.device_id, CLIENT_VERSION, token
        )
    }

    fn headers(&self) -> Result<HeaderMap> {
        let mut h = HeaderMap::new();
        h.insert(ACCEPT, HeaderValue::from_static("application/json"));
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        h.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&self.auth_header()).context("invalid auth header")?,
        );
        Ok(h)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Authenticate by username/password and store the access token on the client.
    pub async fn login(&mut self, username: &str, password: &str) -> Result<AuthResult> {
        let url = self.url("/Users/AuthenticateByName");
        let body = json!({
            "Username": username,
            "Pw": password,
        });
        debug!(?url, "authenticating");
        let resp = self
            .http
            .post(&url)
            .headers(self.headers()?)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("login failed ({}): {}", status, text);
        }
        let auth: AuthResult = resp.json().await.context("parsing auth response")?;
        self.access_token = Some(auth.access_token.clone());
        self.user_id = Some(auth.user.id.clone());
        Ok(auth)
    }

    /// Fetch the current user's library views (Movies, TV Shows, Music, …).
    pub async fn views(&self) -> Result<Vec<crate::models::UserView>> {
        let user_id = self.user_id.as_ref().context("not authenticated")?;
        let url = self.url(&format!("/Users/{}/Views", user_id));
        let resp = self.http.get(&url).headers(self.headers()?).send().await?;
        let res: UserViewsResult = resp.error_for_status()?.json().await?;
        Ok(res.items)
    }

    /// Fetch children of a parent (library, folder, album, playlist, …).
    ///
    /// When `limit` is `None` the endpoint is paginated internally with
    /// `StartIndex` + `Limit=500`, driven by the server-reported
    /// `TotalRecordCount`. This is the difference between "we asked without
    /// a limit and Jellyfin capped us at its silent default" and "we got
    /// everything". Passing `Some(l)` requests exactly one page of that size.
    pub async fn items(
        &self,
        parent_id: Option<&str>,
        include_types: &[&str],
        recursive: bool,
        sort_by: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<BaseItem>> {
        self.items_filtered(parent_id, include_types, recursive, sort_by, limit, None)
            .await
    }

    /// Same as `items()` with an optional `Filters` value (e.g.
    /// `"IsFavorite"`) threaded through to the server.
    async fn items_filtered(
        &self,
        parent_id: Option<&str>,
        include_types: &[&str],
        recursive: bool,
        sort_by: Option<&str>,
        limit: Option<u32>,
        filters: Option<&str>,
    ) -> Result<Vec<BaseItem>> {
        // Single-page mode — caller wants exactly N results.
        if let Some(l) = limit {
            let (items, _) = self
                .items_page(parent_id, include_types, recursive, sort_by, l, 0, filters)
                .await?;
            return Ok(items);
        }

        // Paginated mode — page through until we've collected every item.
        const PAGE: u32 = 500;
        let mut all: Vec<BaseItem> = Vec::new();
        let mut start: u32 = 0;
        loop {
            let (items, total) = self
                .items_page(
                    parent_id,
                    include_types,
                    recursive,
                    sort_by,
                    PAGE,
                    start,
                    filters,
                )
                .await?;
            let got = items.len() as u32;
            all.extend(items);
            // Stop when the server ran out of matches …
            if got == 0 || got < PAGE {
                break;
            }
            // … or when we've collected everything the server said existed.
            if total > 0 && (all.len() as i64) >= total {
                break;
            }
            start += got;
            // Belt-and-braces guard against a runaway server response.
            if all.len() > 1_000_000 {
                break;
            }
        }
        Ok(all)
    }

    /// One page of `/Users/{id}/Items`. Returns `(items, total_record_count)`
    /// so the caller can drive pagination.
    async fn items_page(
        &self,
        parent_id: Option<&str>,
        include_types: &[&str],
        recursive: bool,
        sort_by: Option<&str>,
        limit: u32,
        start_index: u32,
        filters: Option<&str>,
    ) -> Result<(Vec<BaseItem>, i64)> {
        let user_id = self.user_id.as_ref().context("not authenticated")?;
        let mut q: Vec<(String, String)> = vec![
            (
                "Fields".into(),
                "PrimaryImageAspectRatio,ProductionYear,Overview,Container,MediaSources".into(),
            ),
            ("Recursive".into(), recursive.to_string()),
            ("Limit".into(), limit.to_string()),
            ("StartIndex".into(), start_index.to_string()),
        ];
        if let Some(p) = parent_id {
            q.push(("ParentId".into(), p.into()));
        }
        if !include_types.is_empty() {
            q.push(("IncludeItemTypes".into(), include_types.join(",")));
        }
        if let Some(s) = sort_by {
            q.push(("SortBy".into(), s.into()));
            q.push(("SortOrder".into(), "Ascending".into()));
        }
        if let Some(f) = filters {
            q.push(("Filters".into(), f.into()));
        }
        let url = self.url(&format!("/Users/{}/Items", user_id));
        debug!(?url, start_index, limit, "items page");
        let body = self
            .http
            .get(&url)
            .headers(self.headers()?)
            .query(&q)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let items = parse_items_body(&body);
        let total = parse_total_record_count(&body);
        Ok((items, total))
    }

    /// Recent / resume items on the home screen.
    pub async fn resume(&self, limit: u32) -> Result<Vec<BaseItem>> {
        let user_id = self.user_id.as_ref().context("not authenticated")?;
        let url = self.url(&format!("/Users/{}/Items/Resume", user_id));
        let resp = self
            .http
            .get(&url)
            .headers(self.headers()?)
            .query(&[("Limit", limit.to_string())])
            .send()
            .await?;
        let res: SearchResult = resp.error_for_status()?.json().await?;
        Ok(res.items)
    }

    pub async fn latest(&self, parent_id: Option<&str>, limit: u32) -> Result<Vec<BaseItem>> {
        let user_id = self.user_id.as_ref().context("not authenticated")?;
        let mut q: Vec<(String, String)> = vec![
            ("Limit".into(), limit.to_string()),
            ("Fields".into(), "ProductionYear,Container".into()),
        ];
        if let Some(p) = parent_id {
            q.push(("ParentId".into(), p.into()));
        }
        let url = self.url(&format!("/Users/{}/Items/Latest", user_id));
        let resp = self
            .http
            .get(&url)
            .headers(self.headers()?)
            .query(&q)
            .send()
            .await?;
        let items: Vec<BaseItem> = resp.error_for_status()?.json().await?;
        Ok(items)
    }

    /// Search hints — the fast fuzzy search endpoint Jellyfin's web client uses.
    ///
    /// Jellyfin's `/Search/Hints` endpoint returns a `{ SearchHints, TotalRecordCount }`
    /// payload. If it comes back empty we fall back to `/Users/{id}/Items` with
    /// `SearchTerm`, which is the endpoint the Jellyfin web app actually uses
    /// for the media results grid — it's slower but has universal coverage.
    pub async fn search(
        &self,
        term: &str,
        include_types: &[&str],
        limit: u32,
    ) -> Result<Vec<BaseItem>> {
        let user_id = self.user_id.as_ref().context("not authenticated")?;

        // Jellyfin's Kestrel binder is case-insensitive, but its OpenAPI
        // documents camelCase — use that so proxies that pass query strings
        // through untouched still route correctly.
        let mut q: Vec<(String, String)> = vec![
            ("searchTerm".into(), term.into()),
            ("userId".into(), user_id.clone()),
            ("limit".into(), limit.to_string()),
            ("includePeople".into(), "false".into()),
            ("includeGenres".into(), "false".into()),
            ("includeStudios".into(), "false".into()),
            ("includeArtists".into(), "true".into()),
            ("includeMedia".into(), "true".into()),
        ];
        if !include_types.is_empty() {
            q.push(("includeItemTypes".into(), include_types.join(",")));
        }
        let url = self.url("/Search/Hints");
        debug!(?url, term=%term, "search hints");
        let resp = self
            .http
            .get(&url)
            .headers(self.headers()?)
            .query(&q)
            .send()
            .await?
            .error_for_status()?;

        // Read as text first so we can be forgiving about the response shape
        // — Jellyfin has shipped PascalCase, camelCase, and (in some proxies)
        // even snake_case variants across versions.
        let body = resp.text().await?;
        let hits = parse_search_hint_body(&body);
        if !hits.is_empty() {
            return Ok(hits.into_iter().map(|h| h.into_base_item()).collect());
        }
        debug!(
            body_head = body.chars().take(240).collect::<String>().as_str(),
            "search/hints returned no parsable hits — falling back to /Users/.../Items"
        );

        // Fallback: the same query against the Items endpoint.
        let mut q2: Vec<(String, String)> = vec![
            ("searchTerm".into(), term.into()),
            ("recursive".into(), "true".into()),
            ("limit".into(), limit.to_string()),
            (
                "fields".into(),
                "PrimaryImageAspectRatio,ProductionYear,Overview,Container,MediaSources".into(),
            ),
        ];
        if !include_types.is_empty() {
            q2.push(("includeItemTypes".into(), include_types.join(",")));
        }
        let url2 = self.url(&format!("/Users/{}/Items", user_id));
        let body2 = self
            .http
            .get(&url2)
            .headers(self.headers()?)
            .query(&q2)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let items = parse_items_body(&body2);
        Ok(items)
    }

    /// All playlists visible to the current user.
    pub async fn playlists(&self) -> Result<Vec<BaseItem>> {
        self.items(None, &["Playlist"], true, Some("SortName"), None)
            .await
    }

    /// Every item the user has marked as a favorite — tracks, albums,
    /// artists, movies, series, episodes. Paginated like `items()`.
    pub async fn favorites(&self) -> Result<Vec<BaseItem>> {
        self.items_filtered(
            None,
            &[
                "Audio",
                "MusicAlbum",
                "MusicArtist",
                "Movie",
                "Series",
                "Episode",
            ],
            true,
            Some("SortName"),
            None,
            Some("IsFavorite"),
        )
        .await
    }

    /// Mark (`POST`) or unmark (`DELETE`) an item as a favorite.
    pub async fn set_favorite(&self, item_id: &str, favorite: bool) -> Result<()> {
        let user_id = self.user_id.as_ref().context("not authenticated")?;
        let url = self.url(&format!("/Users/{}/FavoriteItems/{}", user_id, item_id));
        let req = if favorite {
            self.http.post(&url)
        } else {
            self.http.delete(&url)
        };
        req.headers(self.headers()?)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// All items in a playlist, paginated by `StartIndex` + `Limit=500`.
    pub async fn playlist_items(&self, playlist_id: &str) -> Result<Vec<BaseItem>> {
        let user_id = self.user_id.as_ref().context("not authenticated")?;
        let url = self.url(&format!("/Playlists/{}/Items", playlist_id));
        const PAGE: u32 = 500;
        let mut all: Vec<BaseItem> = Vec::new();
        let mut start: u32 = 0;
        loop {
            let body = self
                .http
                .get(&url)
                .headers(self.headers()?)
                .query(&[
                    ("UserId", user_id.as_str()),
                    ("Limit", &PAGE.to_string()),
                    ("StartIndex", &start.to_string()),
                ])
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?;
            let page = parse_items_body(&body);
            let got = page.len() as u32;
            let total = parse_total_record_count(&body);
            all.extend(page);
            if got == 0 || got < PAGE {
                break;
            }
            if total > 0 && (all.len() as i64) >= total {
                break;
            }
            start += got;
            if all.len() > 1_000_000 {
                break;
            }
        }
        Ok(all)
    }

    pub async fn create_playlist(
        &self,
        name: &str,
        media_type: &str,
        item_ids: &[String],
    ) -> Result<String> {
        let user_id = self.user_id.as_ref().context("not authenticated")?;
        let url = self.url("/Playlists");
        let body = json!({
            "Name": name,
            "Ids": item_ids,
            "UserId": user_id,
            "MediaType": media_type,
        });
        let resp = self
            .http
            .post(&url)
            .headers(self.headers()?)
            .json(&body)
            .send()
            .await?;
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct R {
            id: String,
        }
        let r: R = resp.error_for_status()?.json().await?;
        Ok(r.id)
    }

    pub async fn add_to_playlist(&self, playlist_id: &str, item_ids: &[String]) -> Result<()> {
        let user_id = self.user_id.as_ref().context("not authenticated")?;
        let url = self.url(&format!("/Playlists/{}/Items", playlist_id));
        self.http
            .post(&url)
            .headers(self.headers()?)
            .query(&[("Ids", item_ids.join(",")), ("UserId", user_id.to_string())])
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn remove_from_playlist(
        &self,
        playlist_id: &str,
        entry_ids: &[String],
    ) -> Result<()> {
        let url = self.url(&format!("/Playlists/{}/Items", playlist_id));
        self.http
            .delete(&url)
            .headers(self.headers()?)
            .query(&[("EntryIds", entry_ids.join(","))])
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Build a stream URL for `item`.
    ///
    /// - `Direct` — uses `item.container` (`.mp3` / `.flac` / `.mp4` / `.mkv`
    ///   / …) so the URL matches the source and no unnecessary transcoding
    ///   is triggered. Falls back to a sensible generic when Jellyfin
    ///   didn't send a container.
    /// - `Hls` — `main.m3u8` served by Jellyfin's HLS transcoder.
    pub fn stream_url(&self, item: &BaseItem, format: StreamFormat) -> Result<String> {
        let token = self.access_token.as_deref().context("no access token")?;
        let kind = item.kind();
        let is_audio = kind.is_audio() || item.media_type.as_deref() == Some("Audio");

        let path = match format {
            StreamFormat::Hls if is_audio => format!("/Audio/{}/main.m3u8", item.id),
            StreamFormat::Hls => format!("/Videos/{}/main.m3u8", item.id),
            StreamFormat::Direct => {
                let container = source_container(item, is_audio);
                if is_audio {
                    format!("/Audio/{}/stream.{}", item.id, container)
                } else {
                    format!("/Videos/{}/stream.{}", item.id, container)
                }
            }
        };

        let mut params: Vec<(&str, String)> = vec![
            ("api_key", token.to_string()),
            ("DeviceId", self.device_id.clone()),
            ("PlaySessionId", Uuid::new_v4().to_string()),
        ];
        // `Static=true` skips server-side transcoding — safe because the
        // URL extension already matches the source container.
        if matches!(format, StreamFormat::Direct) {
            params.push(("Static", "true".into()));
            params.push(("MediaSourceId", item.id.clone()));
        }

        let qs = params
            .into_iter()
            .map(|(k, v)| {
                format!(
                    "{}={}",
                    k,
                    utf8_percent_encode(&v, NON_ALPHANUMERIC).to_string()
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        Ok(format!("{}{}?{}", self.base_url, path, qs))
    }

    /// Determine the MIME type from the URL a stream URL points at.
    /// Single source of truth: whatever `stream_url()` produced,
    /// `content_type_for_url()` labels it correctly for the receiver.
    pub fn content_type_for_url(url: &str) -> &'static str {
        let path = url.split('?').next().unwrap_or(url);
        let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        match ext.as_str() {
            "m3u8" => "application/vnd.apple.mpegurl",
            "mpd" => "application/dash+xml",
            "mp4" | "m4v" => "video/mp4",
            "mkv" => "video/x-matroska",
            "webm" => "video/webm",
            "mov" => "video/quicktime",
            "ts" => "video/mp2t",
            "avi" => "video/x-msvideo",
            "mp3" => "audio/mpeg",
            "m4a" | "aac" => "audio/aac",
            "ogg" | "oga" => "audio/ogg",
            "opus" => "audio/opus",
            "flac" => "audio/flac",
            "wav" => "audio/wav",
            "wma" => "audio/x-ms-wma",
            _ => {
                if url.contains("/Videos/") {
                    "video/mp4"
                } else {
                    "audio/mpeg"
                }
            }
        }
    }

    /// Primary image URL for an item, suitable for tile previews.
    pub fn image_url(&self, item_id: &str, tag: &str, width: u32) -> String {
        format!(
            "{}/Items/{}/Images/Primary?tag={}&fillWidth={}&format=jpg",
            self.base_url, item_id, tag, width
        )
    }

    pub async fn report_started(&self, item: &BaseItem, session_id: &str) -> Result<()> {
        self.report(
            "/Sessions/Playing",
            json!({
                "ItemId": item.id,
                "PlaySessionId": session_id,
                "CanSeek": true,
            }),
        )
        .await
    }

    pub async fn report_progress(
        &self,
        item: &BaseItem,
        session_id: &str,
        position_ticks: i64,
        paused: bool,
    ) -> Result<()> {
        self.report(
            "/Sessions/Playing/Progress",
            json!({
                "ItemId": item.id,
                "PlaySessionId": session_id,
                "PositionTicks": position_ticks,
                "IsPaused": paused,
                "CanSeek": true,
                "EventName": "TimeUpdate",
            }),
        )
        .await
    }

    pub async fn report_stopped(
        &self,
        item: &BaseItem,
        session_id: &str,
        position_ticks: i64,
    ) -> Result<()> {
        self.report(
            "/Sessions/Playing/Stopped",
            json!({
                "ItemId": item.id,
                "PlaySessionId": session_id,
                "PositionTicks": position_ticks,
            }),
        )
        .await
    }

    async fn report<T: Serialize>(&self, path: &str, body: T) -> Result<()> {
        let url = self.url(path);
        self.http
            .post(&url)
            .headers(self.headers()?)
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item_with_container(kind: &str, container: Option<&str>) -> BaseItem {
        BaseItem {
            id: "id-1".into(),
            name: "n".into(),
            type_: kind.into(),
            album: None,
            album_id: None,
            album_artist: None,
            artists: None,
            series_name: None,
            production_year: None,
            run_time_ticks: None,
            media_type: None,
            container: container.map(str::to_string),
            index_number: None,
            parent_index_number: None,
            image_tags: None,
            is_folder: None,
            overview: None,
        }
    }

    // ------------------------------------------------------------------
    // source_container
    // ------------------------------------------------------------------

    #[test]
    fn source_container_prefers_first_entry_of_comma_list() {
        let it = item_with_container("Audio", Some("mp3,mpeg"));
        assert_eq!(source_container(&it, true), "mp3");
    }

    #[test]
    fn source_container_lowercases_and_trims() {
        let it = item_with_container("Audio", Some("  FLAC  "));
        assert_eq!(source_container(&it, true), "flac");
    }

    #[test]
    fn source_container_falls_back_to_mp3_or_mp4() {
        let audio = item_with_container("Audio", None);
        assert_eq!(source_container(&audio, true), "mp3");
        let video = item_with_container("Movie", None);
        assert_eq!(source_container(&video, false), "mp4");
    }

    #[test]
    fn source_container_falls_back_on_empty_string() {
        let it = item_with_container("Audio", Some(""));
        assert_eq!(source_container(&it, true), "mp3");
    }

    // ------------------------------------------------------------------
    // content_type_for_url
    // ------------------------------------------------------------------

    #[test]
    fn content_type_maps_common_extensions() {
        assert_eq!(
            JellyfinClient::content_type_for_url("http://x/a/stream.mp3?foo=1"),
            "audio/mpeg"
        );
        assert_eq!(
            JellyfinClient::content_type_for_url("http://x/a/stream.flac"),
            "audio/flac"
        );
        assert_eq!(
            JellyfinClient::content_type_for_url("http://x/a/stream.m4a"),
            "audio/aac"
        );
        assert_eq!(
            JellyfinClient::content_type_for_url("http://x/Videos/1/stream.mkv"),
            "video/x-matroska"
        );
        assert_eq!(
            JellyfinClient::content_type_for_url("http://x/Videos/1/main.m3u8"),
            "application/vnd.apple.mpegurl"
        );
    }

    #[test]
    fn content_type_ignores_query_string() {
        assert_eq!(
            JellyfinClient::content_type_for_url("http://x/a/stream.opus?api_key=abc&Static=true"),
            "audio/opus"
        );
    }

    #[test]
    fn content_type_falls_back_by_url_shape() {
        // Unknown ext, but URL contains /Videos/ → video/mp4.
        assert_eq!(
            JellyfinClient::content_type_for_url("http://x/Videos/1/stream.unknown"),
            "video/mp4"
        );
        // Otherwise → audio/mpeg.
        assert_eq!(
            JellyfinClient::content_type_for_url("http://x/whatever"),
            "audio/mpeg"
        );
    }
}
