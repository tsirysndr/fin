//! Minimal Subsonic REST API client.
//!
//! Speaks Subsonic REST v1.16.1 and hands results back as
//! `fin_jellyfin::BaseItem` / `AuthResult` so the TUI widgets don't have
//! to know which backend they came from. Compatible with Airsonic,
//! Navidrome, Gonic, Astiga, and other servers that speak the standard
//! Subsonic vocabulary.
//!
//! Auth uses the standard salt + MD5(password + salt) token scheme — no
//! plain password ever hits the wire once the client is logged in.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use rand::Rng;
use serde::Deserialize;
use uuid::Uuid;

use fin_jellyfin::{AuthResult, AuthUser, BaseItem, StreamFormat};

const API_VERSION: &str = "1.16.1";
const CLIENT_NAME: &str = "fin";

pub struct SubsonicClient {
    http: reqwest::Client,
    base_url: String,
    device_id: String,
    /// In-memory credential store. Never persisted outside of what the
    /// caller already writes into `ServerConfig.access_token`.
    username: Arc<Mutex<Option<String>>>,
    password: Arc<Mutex<Option<String>>>,
}

impl SubsonicClient {
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(format!("fin/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            device_id: Uuid::new_v4().to_string(),
            username: Arc::new(Mutex::new(None)),
            password: Arc::new(Mutex::new(None)),
        })
    }

    pub fn with_credentials(
        base_url: impl Into<String>,
        device_id: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<Self> {
        let mut c = Self::new(base_url)?;
        c.device_id = device_id.into();
        *c.username.lock() = Some(username.into());
        *c.password.lock() = Some(password.into());
        Ok(c)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }
    pub fn device_id(&self) -> &str {
        &self.device_id
    }
    pub fn username(&self) -> Option<String> {
        self.username.lock().clone()
    }
    pub fn password(&self) -> Option<String> {
        self.password.lock().clone()
    }

    /// Verify credentials by hitting `ping.view`. Populates the client's
    /// state and returns an `AuthResult` shaped like Jellyfin's so
    /// downstream code doesn't have to branch on server kind.
    pub async fn login(&mut self, username: &str, password: &str) -> Result<AuthResult> {
        *self.username.lock() = Some(username.to_string());
        *self.password.lock() = Some(password.to_string());
        self.ping().await?;
        Ok(AuthResult {
            access_token: password.to_string(),
            server_id: self.base_url.clone(),
            user: AuthUser {
                id: username.to_string(),
                name: username.to_string(),
            },
        })
    }

    /// GET /rest/ping.view. Cheap credential + reachability check.
    pub async fn ping(&self) -> Result<()> {
        let resp: PingResp = self.get_json("ping", &[]).await?;
        resp.check()
    }

    /// Browse. `parent` = album id returns its tracks; otherwise this
    /// returns the alphabetical album list. `kinds` selects which subsonic
    /// endpoint to hit (`Audio`/`MusicAlbum` → albums, `Playlist` →
    /// playlists). Video kinds are silently no-op'd — Subsonic has no
    /// video model.
    pub async fn items(
        &self,
        parent: Option<&str>,
        kinds: &[&str],
        _recursive: bool,
        _sort_by: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<BaseItem>> {
        if let Some(album_id) = parent {
            return self.album_tracks(album_id).await;
        }
        if kinds.iter().any(|k| *k == "Playlist") {
            return self.playlists().await;
        }
        if kinds.iter().any(|k| matches!(*k, "Audio" | "MusicAlbum")) {
            let size = limit.unwrap_or(500).min(500);
            return self.album_list(size).await;
        }
        Ok(Vec::new())
    }

    async fn album_list(&self, size: u32) -> Result<Vec<BaseItem>> {
        let resp: AlbumListResp = self
            .get_json(
                "getAlbumList2",
                &[
                    ("type", "alphabeticalByName".to_string()),
                    ("size", size.to_string()),
                ],
            )
            .await?;
        resp.check()?;
        Ok(resp
            .album_list2()
            .map(|a| a.album)
            .unwrap_or_default()
            .into_iter()
            .map(album_to_base_item)
            .collect())
    }

    async fn album_tracks(&self, album_id: &str) -> Result<Vec<BaseItem>> {
        let resp: AlbumResp = self
            .get_json("getAlbum", &[("id", album_id.to_string())])
            .await?;
        resp.check()?;
        let album = resp
            .album()
            .ok_or_else(|| anyhow!("album {} not found", album_id))?;
        Ok(album.song.into_iter().map(song_to_base_item).collect())
    }

    pub async fn search(&self, query: &str, _kinds: &[&str], limit: u32) -> Result<Vec<BaseItem>> {
        let per = limit.min(50);
        let resp: SearchResp = self
            .get_json(
                "search3",
                &[
                    ("query", query.to_string()),
                    ("songCount", per.to_string()),
                    ("albumCount", per.to_string()),
                    ("artistCount", per.to_string()),
                ],
            )
            .await?;
        resp.check()?;
        let sr = resp.search_result3().unwrap_or_default();
        let mut out = Vec::new();
        for a in sr.album.unwrap_or_default() {
            out.push(album_to_base_item(a));
        }
        for s in sr.song.unwrap_or_default() {
            out.push(song_to_base_item(s));
        }
        Ok(out)
    }

    pub async fn playlists(&self) -> Result<Vec<BaseItem>> {
        let resp: PlaylistsResp = self.get_json("getPlaylists", &[]).await?;
        resp.check()?;
        Ok(resp
            .playlists()
            .map(|p| p.playlist)
            .unwrap_or_default()
            .into_iter()
            .map(playlist_to_base_item)
            .collect())
    }

    pub async fn playlist_items(&self, id: &str) -> Result<Vec<BaseItem>> {
        let resp: PlaylistResp = self
            .get_json("getPlaylist", &[("id", id.to_string())])
            .await?;
        resp.check()?;
        let p = resp
            .playlist()
            .ok_or_else(|| anyhow!("playlist {} not found", id))?;
        Ok(p.entry
            .unwrap_or_default()
            .into_iter()
            .map(song_to_base_item)
            .collect())
    }

    /// Everything the user has starred — albums first, then songs, matching
    /// the order `getStarred2` reports them in. Starred *artists* are
    /// skipped: the browse layer has no artist drill-in for Subsonic, so
    /// surfacing them would produce rows Enter can't do anything with.
    pub async fn starred(&self) -> Result<Vec<BaseItem>> {
        let resp: StarredResp = self.get_json("getStarred2", &[]).await?;
        resp.check()?;
        let s = resp.starred2().unwrap_or_default();
        let mut out = Vec::new();
        for a in s.album.unwrap_or_default() {
            out.push(album_to_base_item(a));
        }
        for song in s.song.unwrap_or_default() {
            out.push(song_to_base_item(song));
        }
        Ok(out)
    }

    /// Star (`star`) or unstar (`unstar`) a song / album / artist by id.
    pub async fn set_star(&self, id: &str, star: bool) -> Result<()> {
        let endpoint = if star { "star" } else { "unstar" };
        let resp: PingResp = self.get_json(endpoint, &[("id", id.to_string())]).await?;
        resp.check()
    }

    /// Report that a track just started playing (`submission=false` — the
    /// Subsonic vocabulary calls this "now playing"). Fills in `time` with
    /// the current wall-clock so downstream agents like Navidrome's
    /// ListenBrainz forwarder have a consistent playback timestamp.
    pub async fn scrobble_now_playing(&self, item: &BaseItem) -> Result<()> {
        self.scrobble(&item.id, false, current_time_ms()).await
    }

    /// Report a completed listen. `submission=true` tells the server to
    /// record the track as played; the `time` param carries the moment
    /// the listen started (or, if unknown, the moment of submission —
    /// most Subsonic forwarders honor whichever we send).
    ///
    /// The `time_ms` argument lets the caller record the real listen-start
    /// (usually now − position_secs). Pass `None` to have `time` default
    /// to the moment of submission.
    pub async fn scrobble_submission(&self, item: &BaseItem, time_ms: Option<u64>) -> Result<()> {
        self.scrobble(&item.id, true, time_ms.unwrap_or_else(current_time_ms))
            .await
    }

    async fn scrobble(&self, id: &str, submission: bool, time_ms: u64) -> Result<()> {
        let resp: PingResp = self
            .get_json(
                "scrobble",
                &[
                    ("id", id.to_string()),
                    ("submission", submission.to_string()),
                    ("time", time_ms.to_string()),
                ],
            )
            .await?;
        resp.check()
    }

    /// Build a direct stream URL. Subsonic's `stream` endpoint serves
    /// the container the server picks — `format` is accepted for API
    /// symmetry with Jellyfin but ignored here.
    pub fn stream_url(&self, item: &BaseItem, _format: StreamFormat) -> Result<String> {
        let params = self.auth_params()?;
        let mut qs = params
            .into_iter()
            .map(|(k, v)| format!("{}={}", k, esc(&v)))
            .collect::<Vec<_>>()
            .join("&");
        qs.push_str(&format!("&id={}", esc(&item.id)));
        Ok(format!("{}/rest/stream.view?{}", self.base_url, qs))
    }

    pub fn image_url(&self, item_id: &str, _tag: &str, size: u32) -> String {
        let auth = self.auth_params().unwrap_or_default();
        let mut qs = auth
            .into_iter()
            .map(|(k, v)| format!("{}={}", k, esc(&v)))
            .collect::<Vec<_>>()
            .join("&");
        qs.push_str(&format!("&id={}&size={}", esc(item_id), size));
        format!("{}/rest/getCoverArt.view?{}", self.base_url, qs)
    }

    // ------------------------------------------------------------------

    fn auth_params(&self) -> Result<Vec<(&'static str, String)>> {
        let user = self
            .username
            .lock()
            .clone()
            .ok_or_else(|| anyhow!("subsonic client not logged in"))?;
        let password = self
            .password
            .lock()
            .clone()
            .ok_or_else(|| anyhow!("subsonic client has no password"))?;
        let salt = random_salt();
        let token = md5_hex(&format!("{password}{salt}"));
        Ok(vec![
            ("u", user),
            ("t", token),
            ("s", salt),
            ("v", API_VERSION.to_string()),
            ("c", CLIENT_NAME.to_string()),
            ("f", "json".to_string()),
        ])
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        endpoint: &str,
        extra_params: &[(&str, String)],
    ) -> Result<T> {
        let mut params = self.auth_params()?;
        for (k, v) in extra_params {
            params.push((*k, v.clone()));
        }
        let qs = params
            .into_iter()
            .map(|(k, v)| format!("{}={}", k, esc(&v)))
            .collect::<Vec<_>>()
            .join("&");
        let url = format!("{}/rest/{}.view?{}", self.base_url, endpoint, qs);
        let text = self
            .http
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        serde_json::from_str(&text).with_context(|| format!("parsing {} response", endpoint))
    }
}

fn esc(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

fn random_salt() -> String {
    let mut rng = rand::rng();
    (0..8)
        .map(|_| {
            let n: u8 = rng.random_range(0..36);
            if n < 10 {
                (b'0' + n) as char
            } else {
                (b'a' + (n - 10)) as char
            }
        })
        .collect()
}

fn md5_hex(input: &str) -> String {
    let digest = md5::compute(input.as_bytes());
    format!("{:x}", digest)
}

/// Current wall-clock in milliseconds since the Unix epoch. Used for the
/// `time` param on scrobble calls — Navidrome and its ListenBrainz
/// forwarder use this as the listen timestamp.
fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---- API response shapes --------------------------------------------------
//
// Every Subsonic endpoint wraps its payload in
// `{"subsonic-response": { "status": "ok"|"failed", "error": {...}, <payload> }}`.
// We flatten that: each response struct carries the outer status/error and
// its own payload fields at the top level.

fn default_status() -> String {
    "failed".into()
}

#[derive(Deserialize, Default)]
struct SubsonicError {
    #[serde(default)]
    code: Option<i32>,
    #[serde(default)]
    message: Option<String>,
}

fn check_status(status: &str, error: Option<&SubsonicError>) -> Result<()> {
    if status == "ok" {
        return Ok(());
    }
    let (code, msg) = error
        .map(|e| {
            (
                e.code.unwrap_or(-1),
                e.message.as_deref().unwrap_or("unknown"),
            )
        })
        .unwrap_or((-1, "unknown"));
    Err(anyhow!("subsonic error {}: {}", code, msg))
}

// Every response is `{"subsonic-response": {"status": ..., "error"?: ...,
// <payload fields>}}`. Rather than macro-generate identically-named inner
// types, spell each shape out — the extra lines pay off in readable
// compiler errors.

#[derive(Deserialize)]
struct PingResp {
    #[serde(rename = "subsonic-response")]
    r: PingInner,
}
#[derive(Deserialize)]
struct PingInner {
    #[serde(default = "default_status")]
    status: String,
    #[serde(default)]
    error: Option<SubsonicError>,
}
impl PingResp {
    fn check(&self) -> Result<()> {
        check_status(&self.r.status, self.r.error.as_ref())
    }
}

#[derive(Deserialize)]
struct AlbumListResp {
    #[serde(rename = "subsonic-response")]
    r: AlbumListInner,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AlbumListInner {
    #[serde(default = "default_status")]
    status: String,
    #[serde(default)]
    error: Option<SubsonicError>,
    #[serde(default)]
    album_list2: Option<AlbumList2>,
}
#[derive(Deserialize, Default)]
struct AlbumList2 {
    #[serde(default)]
    album: Vec<Album>,
}
impl AlbumListResp {
    fn check(&self) -> Result<()> {
        check_status(&self.r.status, self.r.error.as_ref())
    }
    fn album_list2(self) -> Option<AlbumList2> {
        self.r.album_list2
    }
}

#[derive(Deserialize)]
struct AlbumResp {
    #[serde(rename = "subsonic-response")]
    r: AlbumInner,
}
#[derive(Deserialize)]
struct AlbumInner {
    #[serde(default = "default_status")]
    status: String,
    #[serde(default)]
    error: Option<SubsonicError>,
    #[serde(default)]
    album: Option<AlbumWithSongs>,
}
#[derive(Deserialize, Default)]
struct AlbumWithSongs {
    #[serde(default)]
    song: Vec<Song>,
}
impl AlbumResp {
    fn check(&self) -> Result<()> {
        check_status(&self.r.status, self.r.error.as_ref())
    }
    fn album(self) -> Option<AlbumWithSongs> {
        self.r.album
    }
}

#[derive(Deserialize)]
struct SearchResp {
    #[serde(rename = "subsonic-response")]
    r: SearchInner,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchInner {
    #[serde(default = "default_status")]
    status: String,
    #[serde(default)]
    error: Option<SubsonicError>,
    #[serde(default)]
    search_result3: Option<SearchResult3>,
}
#[derive(Deserialize, Default)]
struct SearchResult3 {
    #[serde(default)]
    album: Option<Vec<Album>>,
    #[serde(default)]
    song: Option<Vec<Song>>,
}
impl SearchResp {
    fn check(&self) -> Result<()> {
        check_status(&self.r.status, self.r.error.as_ref())
    }
    fn search_result3(self) -> Option<SearchResult3> {
        self.r.search_result3
    }
}

#[derive(Deserialize)]
struct PlaylistsResp {
    #[serde(rename = "subsonic-response")]
    r: PlaylistsInner,
}
#[derive(Deserialize)]
struct PlaylistsInner {
    #[serde(default = "default_status")]
    status: String,
    #[serde(default)]
    error: Option<SubsonicError>,
    #[serde(default)]
    playlists: Option<Playlists>,
}
#[derive(Deserialize, Default)]
struct Playlists {
    #[serde(default)]
    playlist: Vec<Playlist>,
}
impl PlaylistsResp {
    fn check(&self) -> Result<()> {
        check_status(&self.r.status, self.r.error.as_ref())
    }
    fn playlists(self) -> Option<Playlists> {
        self.r.playlists
    }
}

#[derive(Deserialize)]
struct StarredResp {
    #[serde(rename = "subsonic-response")]
    r: StarredInner,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StarredInner {
    #[serde(default = "default_status")]
    status: String,
    #[serde(default)]
    error: Option<SubsonicError>,
    #[serde(default)]
    starred2: Option<Starred2>,
}
#[derive(Deserialize, Default)]
struct Starred2 {
    #[serde(default)]
    album: Option<Vec<Album>>,
    #[serde(default)]
    song: Option<Vec<Song>>,
}
impl StarredResp {
    fn check(&self) -> Result<()> {
        check_status(&self.r.status, self.r.error.as_ref())
    }
    fn starred2(self) -> Option<Starred2> {
        self.r.starred2
    }
}

#[derive(Deserialize)]
struct PlaylistResp {
    #[serde(rename = "subsonic-response")]
    r: PlaylistInner,
}
#[derive(Deserialize)]
struct PlaylistInner {
    #[serde(default = "default_status")]
    status: String,
    #[serde(default)]
    error: Option<SubsonicError>,
    #[serde(default)]
    playlist: Option<PlaylistWithEntries>,
}
#[derive(Deserialize, Default)]
struct PlaylistWithEntries {
    #[serde(default)]
    entry: Option<Vec<Song>>,
}
impl PlaylistResp {
    fn check(&self) -> Result<()> {
        check_status(&self.r.status, self.r.error.as_ref())
    }
    fn playlist(self) -> Option<PlaylistWithEntries> {
        self.r.playlist
    }
}

#[derive(Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct Album {
    id: String,
    name: String,
    #[serde(default)]
    artist: Option<String>,
    #[serde(default)]
    year: Option<i32>,
    #[serde(default)]
    duration: Option<u64>,
    #[serde(default)]
    song_count: Option<u32>,
    #[serde(default)]
    cover_art: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Song {
    id: String,
    title: String,
    #[serde(default)]
    album: Option<String>,
    #[serde(default)]
    album_id: Option<String>,
    #[serde(default)]
    artist: Option<String>,
    #[serde(default)]
    track: Option<i32>,
    #[serde(default)]
    disc_number: Option<i32>,
    #[serde(default)]
    year: Option<i32>,
    #[serde(default)]
    duration: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Playlist {
    id: String,
    name: String,
    #[serde(default)]
    song_count: Option<u32>,
    #[serde(default)]
    duration: Option<u64>,
}

fn album_to_base_item(a: Album) -> BaseItem {
    BaseItem {
        id: a.id,
        name: a.name,
        type_: "MusicAlbum".into(),
        album: None,
        album_id: None,
        album_artist: a.artist,
        artists: None,
        series_name: None,
        production_year: a.year,
        run_time_ticks: a.duration.map(|s| (s as i64) * 10_000_000),
        media_type: None,
        container: None,
        index_number: a.song_count.map(|n| n as i32),
        parent_index_number: None,
        image_tags: a.cover_art.map(|c| serde_json::json!({ "Primary": c })),
        is_folder: Some(true),
        overview: None,
    }
}

fn song_to_base_item(s: Song) -> BaseItem {
    BaseItem {
        id: s.id,
        name: s.title,
        type_: "Audio".into(),
        album: s.album,
        album_id: s.album_id,
        album_artist: None,
        artists: s.artist.map(|a| vec![a]),
        series_name: None,
        production_year: s.year,
        run_time_ticks: s.duration.map(|d| (d as i64) * 10_000_000),
        media_type: Some("Audio".into()),
        container: None,
        index_number: s.track,
        parent_index_number: s.disc_number,
        image_tags: None,
        is_folder: Some(false),
        overview: None,
    }
}

fn playlist_to_base_item(p: Playlist) -> BaseItem {
    BaseItem {
        id: p.id,
        name: p.name,
        type_: "Playlist".into(),
        album: None,
        album_id: None,
        album_artist: None,
        artists: None,
        series_name: None,
        production_year: None,
        run_time_ticks: p.duration.map(|d| (d as i64) * 10_000_000),
        media_type: None,
        container: None,
        index_number: p.song_count.map(|n| n as i32),
        parent_index_number: None,
        image_tags: None,
        is_folder: Some(true),
        overview: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_hex_matches_reference_vectors() {
        // Canonical MD5 test vectors from RFC 1321.
        assert_eq!(md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn random_salt_is_eight_alnum_chars() {
        let s = random_salt();
        assert_eq!(s.len(), 8);
        for c in s.chars() {
            assert!(c.is_ascii_lowercase() || c.is_ascii_digit(), "bad char {c}");
        }
    }

    #[test]
    fn album_maps_to_music_album_base_item() {
        let a = Album {
            id: "a1".into(),
            name: "Kind of Blue".into(),
            artist: Some("Miles Davis".into()),
            year: Some(1959),
            duration: Some(2712),
            song_count: Some(5),
            cover_art: Some("cov-a1".into()),
        };
        let bi = album_to_base_item(a);
        assert_eq!(bi.id, "a1");
        assert_eq!(bi.type_, "MusicAlbum");
        assert_eq!(bi.production_year, Some(1959));
        assert_eq!(bi.album_artist.as_deref(), Some("Miles Davis"));
        // 2712 seconds → run_time_ticks in Jellyfin's 100-ns unit.
        assert_eq!(bi.run_time_ticks, Some(27_120_000_000));
    }

    #[test]
    fn song_maps_disc_and_track_number() {
        let s = Song {
            id: "s1".into(),
            title: "So What".into(),
            album: Some("Kind of Blue".into()),
            album_id: Some("a1".into()),
            artist: Some("Miles Davis".into()),
            track: Some(1),
            disc_number: Some(2),
            year: Some(1959),
            duration: Some(556),
        };
        let bi = song_to_base_item(s);
        assert_eq!(bi.type_, "Audio");
        assert_eq!(bi.index_number, Some(1));
        assert_eq!(bi.parent_index_number, Some(2));
        assert_eq!(bi.album.as_deref(), Some("Kind of Blue"));
    }

    #[test]
    fn deserialize_ok_ping_response() {
        let raw = r#"{"subsonic-response": {"status": "ok", "version": "1.16.1"}}"#;
        let r: PingResp = serde_json::from_str(raw).unwrap();
        r.check().unwrap();
    }

    #[test]
    fn deserialize_starred2_maps_albums_and_songs() {
        // Real-world shape: `getStarred2` wraps its payload in
        // `subsonic-response`, with `status` appearing *after* the payload
        // (key order is server-defined). Starred artists are present but
        // deliberately dropped by `starred()`.
        let raw = r#"{"subsonic-response":{"starred2":{"album":[],"artist":[{"id":"ar-1","name":"070 Shake"}],"song":[{"id":"so-1","title":"Catch 1","album":"X","albumId":"al-1","artist":"42 Dugg","track":11,"discNumber":1,"duration":169,"year":2024},{"id":"so-2","title":"Megan","artist":"42 Dugg","duration":155}]},"status":"ok","type":"smolsonic","version":"1.16.1"}}"#;
        let r: StarredResp = serde_json::from_str(raw).expect("deserialize starred");
        r.check().expect("check ok");
        let s = r.starred2().expect("has starred2");
        assert_eq!(s.album.unwrap_or_default().len(), 0);
        assert_eq!(s.song.unwrap_or_default().len(), 2);
    }

    #[test]
    fn deserialize_error_response_surfaces_code_and_message() {
        let raw = r#"{"subsonic-response": {"status": "failed", "error": {"code": 40, "message": "Wrong username or password"}}}"#;
        let r: PingResp = serde_json::from_str(raw).unwrap();
        let err = r.check().unwrap_err().to_string();
        assert!(err.contains("40"));
        assert!(err.contains("Wrong username"));
    }
}
