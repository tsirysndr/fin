//! Backend-agnostic media client abstraction.
//!
//! `MediaClient` is the trait the TUI and CLI talk to; both
//! [`fin_jellyfin::JellyfinClient`] and [`fin_subsonic::SubsonicClient`]
//! implement it. `probe_server(url)` pings both flavours in parallel so
//! `fin login <url>` can figure out which one is at the other end without
//! the user having to say.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;

pub use fin_config::ServerKind;
pub use fin_jellyfin::{AuthResult, BaseItem, ItemKind, StreamFormat};
pub use fin_jellyfin::JellyfinClient;
pub use fin_subsonic::SubsonicClient;

/// Common browse / search / stream surface. Both backends model the same
/// concepts a little differently on the wire; the trait paves over the
/// differences so `fin-tui` doesn't have to branch on server kind.
#[async_trait]
pub trait MediaClient: Send + Sync {
    fn kind(&self) -> ServerKind;
    fn base_url(&self) -> &str;
    fn device_id(&self) -> &str;
    fn access_token(&self) -> Option<&str>;
    fn user_id(&self) -> Option<&str>;

    async fn items(
        &self,
        parent: Option<&str>,
        kinds: &[&str],
        recursive: bool,
        sort_by: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<BaseItem>>;

    async fn search(&self, query: &str, kinds: &[&str], limit: u32) -> Result<Vec<BaseItem>>;
    async fn playlists(&self) -> Result<Vec<BaseItem>>;
    async fn playlist_items(&self, id: &str) -> Result<Vec<BaseItem>>;

    fn stream_url(&self, item: &BaseItem, format: StreamFormat) -> Result<String>;
    fn image_url(&self, item_id: &str, tag: &str, width: u32) -> String;

    /// Tell the server a track just started playing (Jellyfin's
    /// `PlaybackStarted`, Subsonic's `scrobble?submission=false`).
    async fn report_started(&self, _item: &BaseItem, _session_id: &str) -> Result<()> {
        Ok(())
    }

    /// Send a progress tick — Jellyfin only. Subsonic doesn't have a
    /// per-progress endpoint; its scrobble stream fires once at completion.
    async fn report_progress(
        &self,
        _item: &BaseItem,
        _position_secs: u64,
        _is_paused: bool,
        _session_id: &str,
    ) -> Result<()> {
        Ok(())
    }

    /// Tell the server a track finished / stopped (Jellyfin's
    /// `PlaybackStopped`, Subsonic's `scrobble?submission=true` — which is
    /// what most Last.fm scrobblers listen to).
    async fn report_stopped(
        &self,
        _item: &BaseItem,
        _position_secs: u64,
        _session_id: &str,
    ) -> Result<()> {
        Ok(())
    }
}

// ---- adapters -------------------------------------------------------------

#[async_trait]
impl MediaClient for JellyfinClient {
    fn kind(&self) -> ServerKind {
        ServerKind::Jellyfin
    }
    fn base_url(&self) -> &str {
        JellyfinClient::base_url(self)
    }
    fn device_id(&self) -> &str {
        JellyfinClient::device_id(self)
    }
    fn access_token(&self) -> Option<&str> {
        JellyfinClient::access_token(self)
    }
    fn user_id(&self) -> Option<&str> {
        JellyfinClient::user_id(self)
    }

    async fn items(
        &self,
        parent: Option<&str>,
        kinds: &[&str],
        recursive: bool,
        sort_by: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<BaseItem>> {
        JellyfinClient::items(self, parent, kinds, recursive, sort_by, limit).await
    }
    async fn search(&self, query: &str, kinds: &[&str], limit: u32) -> Result<Vec<BaseItem>> {
        JellyfinClient::search(self, query, kinds, limit).await
    }
    async fn playlists(&self) -> Result<Vec<BaseItem>> {
        JellyfinClient::playlists(self).await
    }
    async fn playlist_items(&self, id: &str) -> Result<Vec<BaseItem>> {
        JellyfinClient::playlist_items(self, id).await
    }

    fn stream_url(&self, item: &BaseItem, format: StreamFormat) -> Result<String> {
        JellyfinClient::stream_url(self, item, format)
    }
    fn image_url(&self, item_id: &str, tag: &str, width: u32) -> String {
        JellyfinClient::image_url(self, item_id, tag, width)
    }
    async fn report_started(&self, item: &BaseItem, session_id: &str) -> Result<()> {
        JellyfinClient::report_started(self, item, session_id).await
    }
    async fn report_progress(
        &self,
        item: &BaseItem,
        position_secs: u64,
        is_paused: bool,
        session_id: &str,
    ) -> Result<()> {
        // JellyfinClient expects ticks (100 ns units). Convert here.
        let ticks = (position_secs as i64) * 10_000_000;
        JellyfinClient::report_progress(self, item, session_id, ticks, is_paused).await
    }
    async fn report_stopped(
        &self,
        item: &BaseItem,
        position_secs: u64,
        session_id: &str,
    ) -> Result<()> {
        let ticks = (position_secs as i64) * 10_000_000;
        JellyfinClient::report_stopped(self, item, session_id, ticks).await
    }
}

#[async_trait]
impl MediaClient for SubsonicClient {
    fn kind(&self) -> ServerKind {
        ServerKind::Subsonic
    }
    fn base_url(&self) -> &str {
        SubsonicClient::base_url(self)
    }
    fn device_id(&self) -> &str {
        SubsonicClient::device_id(self)
    }
    fn access_token(&self) -> Option<&str> {
        // Subsonic auth is per-request; there's no long-lived token.
        None
    }
    fn user_id(&self) -> Option<&str> {
        None
    }

    async fn items(
        &self,
        parent: Option<&str>,
        kinds: &[&str],
        recursive: bool,
        sort_by: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<BaseItem>> {
        SubsonicClient::items(self, parent, kinds, recursive, sort_by, limit).await
    }
    async fn search(&self, query: &str, kinds: &[&str], limit: u32) -> Result<Vec<BaseItem>> {
        SubsonicClient::search(self, query, kinds, limit).await
    }
    async fn playlists(&self) -> Result<Vec<BaseItem>> {
        SubsonicClient::playlists(self).await
    }
    async fn playlist_items(&self, id: &str) -> Result<Vec<BaseItem>> {
        SubsonicClient::playlist_items(self, id).await
    }

    fn stream_url(&self, item: &BaseItem, format: StreamFormat) -> Result<String> {
        SubsonicClient::stream_url(self, item, format)
    }
    fn image_url(&self, item_id: &str, tag: &str, width: u32) -> String {
        SubsonicClient::image_url(self, item_id, tag, width)
    }
    async fn report_started(&self, item: &BaseItem, _session_id: &str) -> Result<()> {
        SubsonicClient::scrobble_now_playing(self, item).await
    }
    // report_progress: default no-op — Subsonic has no per-progress ping.
    async fn report_stopped(
        &self,
        item: &BaseItem,
        position_secs: u64,
        _session_id: &str,
    ) -> Result<()> {
        // Best-effort listen-start timestamp: now − however long we've
        // been playing this track. Navidrome's ListenBrainz forwarder
        // uses this as `listened_at`.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let time_ms = now_ms.saturating_sub(position_secs.saturating_mul(1000));
        SubsonicClient::scrobble_submission(self, item, Some(time_ms)).await
    }
}

// ---- server detection -----------------------------------------------------

/// Probe `url` to figure out whether the server is Jellyfin or Subsonic.
///
/// Fires both probes concurrently — the first to answer wins, so a healthy
/// server responds in one round-trip. If neither replies, returns an error
/// with both underlying failures so the user can tell what went wrong.
pub async fn probe_server(url: &str) -> Result<ServerKind> {
    let base = url.trim_end_matches('/');
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(8))
        .build()
        .context("building probe HTTP client")?;

    let jf = probe_jellyfin(&http, base);
    let ss = probe_subsonic(&http, base);
    // Race the two — first Ok wins.
    tokio::select! {
        biased;
        r = jf => {
            if let Ok(k) = r {
                return Ok(k);
            }
        }
        r = ss => {
            if let Ok(k) = r {
                return Ok(k);
            }
        }
    }
    // If the winner failed, run the other to completion so we can report
    // its result too.
    let (jf, ss) = tokio::join!(
        probe_jellyfin(&http, base),
        probe_subsonic(&http, base),
    );
    if jf.is_ok() {
        return Ok(ServerKind::Jellyfin);
    }
    if ss.is_ok() {
        return Ok(ServerKind::Subsonic);
    }
    Err(anyhow!(
        "could not detect server type at {}:\n  jellyfin probe: {}\n  subsonic probe: {}",
        base,
        jf.err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "n/a".into()),
        ss.err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "n/a".into())
    ))
}

async fn probe_jellyfin(http: &reqwest::Client, base: &str) -> Result<ServerKind> {
    let url = format!("{}/System/Info/Public", base);
    let resp = http.get(&url).send().await?.error_for_status()?;
    let text = resp.text().await?;
    // Jellyfin returns JSON with `ProductName: "Jellyfin Server"`.
    if text.contains("Jellyfin") {
        Ok(ServerKind::Jellyfin)
    } else {
        Err(anyhow!("/System/Info/Public didn't look like Jellyfin"))
    }
}

async fn probe_subsonic(http: &reqwest::Client, base: &str) -> Result<ServerKind> {
    // `ping.view` doesn't require auth — servers return status "failed" for
    // unauthenticated calls but still 200 OK with valid JSON, so a bare
    // response body inspection is enough. We also accept the older `ping`
    // form (some Airsonic derivatives drop the `.view` suffix).
    let url = format!("{}/rest/ping.view?c=fin&v=1.16.1&f=json", base);
    let resp = http.get(&url).send().await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.is_success() && text.contains("subsonic-response") {
        Ok(ServerKind::Subsonic)
    } else {
        Err(anyhow!(
            "/rest/ping.view returned {}: {}",
            status,
            text.chars().take(80).collect::<String>()
        ))
    }
}

// ---- login helpers --------------------------------------------------------

/// One-shot auth wrapper: probe the server, log in with the right client,
/// and return everything wired up as `Arc<dyn MediaClient>`.
pub async fn login_any(
    url: &str,
    username: &str,
    password: &str,
) -> Result<LoggedIn> {
    let kind = probe_server(url).await?;
    match kind {
        ServerKind::Jellyfin => {
            let mut client = JellyfinClient::new(url)?;
            let auth = client.login(username, password).await?;
            let device_id = client.device_id().to_string();
            let arc: Arc<dyn MediaClient> = Arc::new(client);
            Ok(LoggedIn {
                kind,
                auth,
                device_id,
                client: arc,
            })
        }
        ServerKind::Subsonic => {
            let mut client = SubsonicClient::new(url)?;
            let auth = client.login(username, password).await?;
            let device_id = client.device_id().to_string();
            let arc: Arc<dyn MediaClient> = Arc::new(client);
            Ok(LoggedIn {
                kind,
                auth,
                device_id,
                client: arc,
            })
        }
    }
}

pub struct LoggedIn {
    pub kind: ServerKind,
    pub auth: AuthResult,
    pub device_id: String,
    pub client: Arc<dyn MediaClient>,
}

/// Build a stored-credentials client without re-logging in (i.e. after
/// loading `ServerConfig` from disk).
pub fn client_from_stored(
    kind: ServerKind,
    url: &str,
    device_id: &str,
    user_id: &str,
    user_name: &str,
    access_token: &str,
) -> Result<Arc<dyn MediaClient>> {
    match kind {
        ServerKind::Jellyfin => {
            let c = JellyfinClient::with_credentials(url, device_id, user_id, access_token)?;
            Ok(Arc::new(c))
        }
        ServerKind::Subsonic => {
            // For Subsonic we stashed the password in `access_token` at
            // login time and the username in `user_name`.
            let c = SubsonicClient::with_credentials(url, device_id, user_name, access_token)?;
            Ok(Arc::new(c))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_kind_labels_are_stable() {
        assert_eq!(ServerKind::Jellyfin.label(), "jellyfin");
        assert_eq!(ServerKind::Subsonic.label(), "subsonic");
        // Default is Jellyfin so old configs — which lack the field —
        // load with the correct backend.
        assert_eq!(ServerKind::default(), ServerKind::Jellyfin);
    }
}
