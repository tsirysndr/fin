use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use parking_lot::Mutex;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc;
use tracing::warn;

use fin_config::{Config, RendererPref};
use fin_jellyfin::{BaseItem, ItemKind, JellyfinClient, StreamFormat};
use fin_player::{
    discover_chromecasts, discover_upnp_renderers, CastDevice, ChromecastRenderer, LocalRenderer,
    PlaybackState, QueueItem, Renderer, RendererKind, UpnpDevice, UpnpRenderer,
};

/// A single row in the Devices screen. Both Chromecast and UPnP renderers
/// share the same list so the user picks by name rather than by protocol.
#[derive(Debug, Clone)]
pub enum RemoteDevice {
    Cast(CastDevice),
    Upnp(UpnpDevice),
}

impl RemoteDevice {
    pub fn display_name(&self) -> String {
        match self {
            Self::Cast(d) => d.display_name(),
            Self::Upnp(d) => d.display_name(),
        }
    }

    pub fn kind_label(&self) -> &'static str {
        match self {
            Self::Cast(_) => "chromecast",
            Self::Upnp(_) => "upnp",
        }
    }

    pub fn subtitle(&self) -> String {
        match self {
            Self::Cast(d) => format!("{}   {}:{}", d.model, d.address, d.port),
            Self::Upnp(d) => {
                let model = if d.model.is_empty() {
                    "-".to_string()
                } else {
                    d.model.clone()
                };
                let manuf = if d.manufacturer.is_empty() {
                    String::new()
                } else {
                    format!("{}   ", d.manufacturer)
                };
                format!("{manuf}{model}   {}", d.address)
            }
        }
    }
}

use crate::event::{spawn_event_loop, AppEvent};
use crate::screens::{item_row_line, RowLayout, Screen};
use crate::theme::{accent_style, base_style, muted_style, title_style, Palette};
use crate::widgets::{neon_block, NeonTabs, PlayerBar};

/// Everything the render loop needs.
pub struct App {
    pub config: Arc<Mutex<Config>>,
    pub jellyfin: Arc<Mutex<Arc<JellyfinClient>>>,
    pub renderer: Arc<Mutex<Arc<dyn Renderer>>>,
    pub renderer_kind: Arc<Mutex<RendererKind>>,
    pub renderer_label: Arc<Mutex<String>>,
    screen: Screen,
    // shared display state
    music_items: Arc<Mutex<Vec<BaseItem>>>,
    video_items: Arc<Mutex<Vec<BaseItem>>>,
    playlists: Arc<Mutex<Vec<BaseItem>>>,
    playlist_items: Arc<Mutex<Vec<BaseItem>>>,
    open_playlist: Arc<Mutex<Option<BaseItem>>>,
    album_tracks: Arc<Mutex<Vec<BaseItem>>>,
    open_album: Arc<Mutex<Option<BaseItem>>>,
    series_children: Arc<Mutex<Vec<BaseItem>>>,
    open_series: Arc<Mutex<Option<BaseItem>>>,
    search_results: Arc<Mutex<Vec<BaseItem>>>,
    devices: Arc<Mutex<Vec<RemoteDevice>>>,
    search_generation: Arc<AtomicU64>,
    // ephemeral
    search_query: String,
    search_input_focused: bool,
    list_state: ListState,
    status_message: Arc<Mutex<Option<(String, Instant)>>>,
    playback_state: Arc<Mutex<PlaybackState>>,
    should_quit: bool,
    logo_pulse: u8,
}

impl App {
    pub fn new(config: Config, jellyfin: JellyfinClient, renderer: Arc<dyn Renderer>) -> Self {
        let kind = renderer.kind();
        let label = match kind {
            RendererKind::Mpv => "this machine".into(),
            RendererKind::Chromecast => "Chromecast".into(),
            RendererKind::Upnp => "UPnP".into(),
        };
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            config: Arc::new(Mutex::new(config)),
            jellyfin: Arc::new(Mutex::new(Arc::new(jellyfin))),
            renderer: Arc::new(Mutex::new(renderer)),
            renderer_kind: Arc::new(Mutex::new(kind)),
            renderer_label: Arc::new(Mutex::new(label)),
            screen: Screen::Music,
            music_items: Arc::new(Mutex::new(vec![])),
            video_items: Arc::new(Mutex::new(vec![])),
            playlists: Arc::new(Mutex::new(vec![])),
            playlist_items: Arc::new(Mutex::new(vec![])),
            open_playlist: Arc::new(Mutex::new(None)),
            album_tracks: Arc::new(Mutex::new(vec![])),
            open_album: Arc::new(Mutex::new(None)),
            series_children: Arc::new(Mutex::new(vec![])),
            open_series: Arc::new(Mutex::new(None)),
            search_results: Arc::new(Mutex::new(vec![])),
            devices: Arc::new(Mutex::new(vec![])),
            search_generation: Arc::new(AtomicU64::new(0)),
            search_query: String::new(),
            search_input_focused: true,
            list_state,
            status_message: Arc::new(Mutex::new(None)),
            playback_state: Arc::new(Mutex::new(PlaybackState::default())),
            should_quit: false,
            logo_pulse: 0,
        }
    }

    /// Handy accessor — the current Jellyfin client. Swapped out atomically
    /// when the user switches servers.
    fn jf(&self) -> Arc<JellyfinClient> {
        self.jellyfin.lock().clone()
    }

    fn current_list(&self) -> Vec<BaseItem> {
        match self.screen {
            Screen::Search => self.search_results.lock().clone(),
            Screen::Music => {
                if self.open_album.lock().is_some() {
                    self.album_tracks.lock().clone()
                } else {
                    self.music_items.lock().clone()
                }
            }
            Screen::Videos => {
                if self.open_series.lock().is_some() {
                    self.series_children.lock().clone()
                } else {
                    self.video_items.lock().clone()
                }
            }
            Screen::Playlists => {
                if self.open_playlist.lock().is_some() {
                    self.playlist_items.lock().clone()
                } else {
                    self.playlists.lock().clone()
                }
            }
            Screen::Queue => {
                let items = self.playback_state.lock().queue.clone();
                items
                    .into_iter()
                    .map(|q| BaseItem {
                        id: q.id,
                        name: q.title,
                        type_: if q.is_video {
                            "Video".into()
                        } else {
                            "Audio".into()
                        },
                        album: None,
                        album_id: None,
                        album_artist: None,
                        artists: if q.subtitle.is_empty() {
                            None
                        } else {
                            Some(vec![q.subtitle])
                        },
                        series_name: None,
                        production_year: None,
                        run_time_ticks: q.duration_secs.map(|s| (s * 10_000_000) as i64),
                        media_type: None,
                        container: None,
                        index_number: None,
                        parent_index_number: None,
                        image_tags: None,
                        is_folder: None,
                        overview: None,
                    })
                    .collect()
            }
            Screen::Devices | Screen::Settings => vec![],
        }
    }

    fn selected_item(&self) -> Option<BaseItem> {
        let list = self.current_list();
        self.list_state
            .selected()
            .and_then(|i| list.get(i).cloned())
    }

    /// Length of the currently-navigable list for the active screen.
    /// Devices and Settings render their own row types, not `BaseItem`s, so
    /// `current_list()` is empty for them — nav has to size against the real
    /// backing collection instead.
    fn list_len(&self) -> usize {
        match self.screen {
            Screen::Devices => self.devices.lock().len(),
            Screen::Settings => self.config.lock().servers.len(),
            _ => self.current_list().len(),
        }
    }

    fn set_status(&self, msg: impl Into<String>) {
        *self.status_message.lock() = Some((msg.into(), Instant::now()));
    }

    async fn load_screen(&self) {
        match self.screen {
            Screen::Music => {
                let jf = self.jf();
                let out = self.music_items.clone();
                let status = self.status_message.clone();
                tokio::spawn(async move {
                    // No limit — Jellyfin's `Limit=<omitted>` returns all
                    // matching items. Users with 50k+ tracks get everything.
                    match jf
                        .items(None, &["MusicAlbum"], true, Some("SortName"), None)
                        .await
                    {
                        Ok(v) => {
                            *status.lock() =
                                Some((format!("♪ {} album(s)", v.len()), Instant::now()));
                            *out.lock() = v;
                        }
                        Err(e) => {
                            *status.lock() = Some((format!("music: {}", e), Instant::now()))
                        }
                    }
                });
            }
            Screen::Videos => {
                let jf = self.jf();
                let out = self.video_items.clone();
                let status = self.status_message.clone();
                tokio::spawn(async move {
                    match jf
                        .items(None, &["Movie", "Series"], true, Some("SortName"), None)
                        .await
                    {
                        Ok(v) => {
                            *status.lock() =
                                Some((format!("▶ {} item(s)", v.len()), Instant::now()));
                            *out.lock() = v;
                        }
                        Err(e) => {
                            *status.lock() = Some((format!("videos: {}", e), Instant::now()))
                        }
                    }
                });
            }
            Screen::Playlists => {
                let jf = self.jf();
                let out = self.playlists.clone();
                let status = self.status_message.clone();
                tokio::spawn(async move {
                    match jf.playlists().await {
                        Ok(v) => *out.lock() = v,
                        Err(e) => {
                            *status.lock() = Some((format!("playlists: {}", e), Instant::now()))
                        }
                    }
                });
            }
            Screen::Devices => {
                let out = self.devices.clone();
                let status = self.status_message.clone();
                tokio::spawn(async move {
                    *status.lock() = Some((
                        "Scanning for Chromecasts & UPnP renderers…".into(),
                        Instant::now(),
                    ));
                    let (casts, upnps) = tokio::join!(
                        discover_chromecasts(Duration::from_secs(4)),
                        discover_upnp_renderers(Duration::from_secs(4)),
                    );
                    let mut merged: Vec<RemoteDevice> = Vec::new();
                    match casts {
                        Ok(v) => merged.extend(v.into_iter().map(RemoteDevice::Cast)),
                        Err(e) => tracing::warn!(?e, "chromecast scan failed"),
                    }
                    match upnps {
                        Ok(v) => merged.extend(v.into_iter().map(RemoteDevice::Upnp)),
                        Err(e) => tracing::warn!(?e, "upnp scan failed"),
                    }
                    merged.sort_by(|a, b| a.display_name().cmp(&b.display_name()));
                    *status.lock() =
                        Some((format!("Found {} device(s).", merged.len()), Instant::now()));
                    *out.lock() = merged;
                });
            }
            _ => {}
        }
    }

    /// fzf-style instant search — fires on every keystroke.
    /// Uses a generation counter so a slower response for an older query
    /// can never overwrite results from a newer one.
    fn run_search(&self) {
        let query = self.search_query.trim().to_string();
        let gen = self.search_generation.fetch_add(1, Ordering::SeqCst) + 1;
        if query.is_empty() {
            self.search_results.lock().clear();
            *self.status_message.lock() = None;
            return;
        }
        // Immediate feedback while the request is in flight.
        *self.status_message.lock() =
            Some((format!("⌕ searching for “{}”…", query), Instant::now()));
        let jf = self.jf();
        let out = self.search_results.clone();
        let status = self.status_message.clone();
        let generation = self.search_generation.clone();
        tokio::spawn(async move {
            // Very small coalescing window — enough to avoid firing three
            // requests when a fast typist hits three keys in <30ms, but not
            // so long that the UI feels dead.
            tokio::time::sleep(Duration::from_millis(40)).await;
            if generation.load(Ordering::SeqCst) != gen {
                return;
            }
            match jf
                .search(
                    &query,
                    &[
                        "Audio",
                        "MusicAlbum",
                        "MusicArtist",
                        "Movie",
                        "Series",
                        "Episode",
                    ],
                    50,
                )
                .await
            {
                Ok(v) => {
                    if generation.load(Ordering::SeqCst) == gen {
                        *status.lock() = Some((
                            format!("⌕ {} match(es) for “{}”", v.len(), query),
                            Instant::now(),
                        ));
                        *out.lock() = v;
                    }
                }
                Err(e) => {
                    if generation.load(Ordering::SeqCst) == gen {
                        *status.lock() =
                            Some((format!("⌕ search failed: {}", e), Instant::now()));
                        tracing::warn!(query=%query, error=?e, "jellyfin search failed");
                    }
                }
            }
        });
    }

    fn base_item_to_queue(&self, item: &BaseItem, format: StreamFormat) -> Result<QueueItem> {
        let url = self.jf().stream_url(item, format)?;
        let is_video = item.kind().is_video();
        // Content type derives from the URL that stream_url() actually built,
        // so it always matches what the receiver will be handed.
        let content_type = JellyfinClient::content_type_for_url(&url).to_string();
        let image_url = item
            .image_tags
            .as_ref()
            .and_then(|v| v.get("Primary"))
            .and_then(|v| v.as_str())
            .map(|tag| self.jf().image_url(&item.id, tag, 480));
        Ok(QueueItem {
            id: item.id.clone(),
            title: item.name.clone(),
            subtitle: item.subtitle(),
            stream_url: url,
            image_url,
            duration_secs: item.duration_secs(),
            is_video,
            content_type,
        })
    }

    async fn expand_playable(&self, item: BaseItem) -> Result<Vec<BaseItem>> {
        match item.kind() {
            ItemKind::MusicAlbum => {
                self.jf()
                    .items(
                        Some(&item.id),
                        &["Audio"],
                        false,
                        Some("ParentIndexNumber,IndexNumber,SortName"),
                        None,
                    )
                    .await
            }
            ItemKind::MusicArtist => {
                self.jf()
                    .items(Some(&item.id), &["Audio"], true, Some("Album"), None)
                    .await
            }
            ItemKind::Playlist => self.jf().playlist_items(&item.id).await,
            ItemKind::Series => {
                self.jf()
                    .items(
                        Some(&item.id),
                        &["Episode"],
                        true,
                        Some("ParentIndexNumber,IndexNumber,SortName"),
                        None,
                    )
                    .await
            }
            _ => Ok(vec![item]),
        }
    }

    /// Delete the highlighted entry from the queue. If it's the item being
    /// played, playback jumps to the next entry (or stops if the queue is
    /// now empty); otherwise the currently-playing track keeps going and
    /// only the surrounding queue shrinks.
    async fn remove_selected_from_queue(&self) {
        let Some(idx) = self.list_state.selected() else {
            return;
        };
        let items = self.playback_state.lock().queue.clone();
        if items.is_empty() || idx >= items.len() {
            self.set_status("Nothing to remove.");
            return;
        }
        let title = items[idx].title.clone();
        let renderer = self.renderer.lock().clone();
        match renderer.remove_from_queue(idx).await {
            Ok(()) => self.set_status(format!("− Removed “{}”", title)),
            Err(e) => self.set_status(format!("remove: {}", e)),
        }
    }

    /// Enter on the Queue screen must NOT replace the queue with just the
    /// selected item. Instead, restart playback from the current queue's
    /// selected index — same items, new playhead.
    async fn jump_to_queue_index(&self) {
        let Some(idx) = self.list_state.selected() else {
            return;
        };
        let items = self.playback_state.lock().queue.clone();
        if items.is_empty() || idx >= items.len() {
            self.set_status("Nothing to jump to.");
            return;
        }
        let title = items[idx].title.clone();
        let renderer = self.renderer.lock().clone();
        match renderer.play(items, idx).await {
            Ok(()) => self.set_status(format!("▶ Jumped to “{}”", title)),
            Err(e) => self.set_status(format!("jump: {}", e)),
        }
    }

    async fn play_selected(&mut self, mode: PlayMode) {
        let Some(item) = self.selected_item() else {
            self.set_status("Nothing selected.");
            return;
        };

        // Drill-in behaviour: Enter (PlayNow) on a *container* opens it and
        // shows its children. `a` (Enqueue), `n` (PlayNext), and `x`
        // (PlayContainer) always dispatch straight to the renderer.
        if mode == PlayMode::PlayNow {
            match (self.screen, item.kind()) {
                (Screen::Playlists, _) if self.open_playlist.lock().is_none() => {
                    self.open_playlist_selected(item).await;
                    return;
                }
                (Screen::Music, ItemKind::MusicAlbum) if self.open_album.lock().is_none() => {
                    self.open_album_selected(item).await;
                    return;
                }
                (Screen::Videos, ItemKind::Series) if self.open_series.lock().is_none() => {
                    self.open_series_selected(item).await;
                    return;
                }
                _ => {}
            }
        }

        let items = match self.expand_playable(item).await {
            Ok(v) => v,
            Err(e) => {
                self.set_status(format!("expand: {}", e));
                return;
            }
        };
        if items.is_empty() {
            self.set_status("Nothing playable here.");
            return;
        }
        // Both renderers get a Direct URL — Jellyfin serves the source
        // container as-is so no unnecessary transcoding happens. Chromecast
        // handles MP3/AAC/FLAC/Opus audio and H.264 MP4 video natively;
        // for anything else you can force HLS transcoding with `--hls`
        // from the CLI (or by switching the renderer's format).
        let format = StreamFormat::Direct;
        let mut queue_items = Vec::with_capacity(items.len());
        for it in &items {
            match self.base_item_to_queue(it, format) {
                Ok(q) => queue_items.push(q),
                Err(e) => warn!(?e, "skip item"),
            }
        }
        if queue_items.is_empty() {
            self.set_status("No playable stream URLs.");
            return;
        }
        let renderer = self.renderer.lock().clone();
        let title = queue_items
            .first()
            .map(|q| q.title.clone())
            .unwrap_or_default();
        let count = queue_items.len();
        let result = match mode {
            PlayMode::PlayNow | PlayMode::PlayContainer => renderer.play(queue_items, 0).await,
            PlayMode::Enqueue => renderer.enqueue(queue_items).await,
            PlayMode::PlayNext => renderer.play_next(queue_items).await,
        };
        match result {
            Ok(()) => self.set_status(match mode {
                PlayMode::PlayNow => format!("▶ Playing “{}” ({} item(s))", title, count),
                PlayMode::PlayContainer => {
                    format!("▶▶ Playing full container ({} item(s))", count)
                }
                PlayMode::Enqueue => format!("+ Queued {} item(s)", count),
                PlayMode::PlayNext => format!("↥ Playing next: {} item(s)", count),
            }),
            Err(e) => self.set_status(format!("renderer: {}", e)),
        }
    }

    async fn open_album_selected(&mut self, item: BaseItem) {
        let jf = self.jf();
        let out = self.album_tracks.clone();
        let open = self.open_album.clone();
        let status = self.status_message.clone();
        let id = item.id.clone();
        let name = item.name.clone();
        *open.lock() = Some(item);
        tokio::spawn(async move {
            match jf
                .items(
                    Some(&id),
                    &["Audio"],
                    false,
                    Some("ParentIndexNumber,IndexNumber,SortName"),
                    None,
                )
                .await
            {
                Ok(v) => {
                    *status.lock() = Some((
                        format!("◈ {} — {} track(s)", name, v.len()),
                        Instant::now(),
                    ));
                    *out.lock() = v;
                }
                Err(e) => *status.lock() = Some((format!("album: {}", e), Instant::now())),
            }
        });
        self.list_state.select(Some(0));
    }

    async fn open_series_selected(&mut self, item: BaseItem) {
        let jf = self.jf();
        let out = self.series_children.clone();
        let open = self.open_series.clone();
        let status = self.status_message.clone();
        let id = item.id.clone();
        let name = item.name.clone();
        *open.lock() = Some(item);
        tokio::spawn(async move {
            match jf
                .items(
                    Some(&id),
                    &["Episode"],
                    true,
                    Some("ParentIndexNumber,IndexNumber,SortName"),
                    None,
                )
                .await
            {
                Ok(v) => {
                    *status.lock() = Some((
                        format!("▶ {} — {} episode(s)", name, v.len()),
                        Instant::now(),
                    ));
                    *out.lock() = v;
                }
                Err(e) => *status.lock() = Some((format!("series: {}", e), Instant::now())),
            }
        });
        self.list_state.select(Some(0));
    }

    async fn open_playlist_selected(&mut self, item: BaseItem) {
        let jf = self.jf();
        let out = self.playlist_items.clone();
        let open = self.open_playlist.clone();
        let status = self.status_message.clone();
        let id = item.id.clone();
        *open.lock() = Some(item);
        tokio::spawn(async move {
            match jf.playlist_items(&id).await {
                Ok(v) => {
                    *status.lock() =
                        Some((format!("Loaded {} items", v.len()), Instant::now()));
                    *out.lock() = v;
                }
                Err(e) => *status.lock() = Some((format!("playlist: {}", e), Instant::now())),
            }
        });
        self.list_state.select(Some(0));
    }

    async fn use_selected_device(&self) -> Result<()> {
        let devices = self.devices.lock().clone();
        let Some(sel) = self
            .list_state
            .selected()
            .and_then(|i| devices.get(i).cloned())
        else {
            return Err(anyhow!("no device selected"));
        };
        let name = sel.display_name();
        self.set_status(format!("Connecting to {}…", name));
        match sel {
            RemoteDevice::Cast(dev) => {
                let renderer = ChromecastRenderer::connect(dev).await?;
                let arc: Arc<dyn Renderer> = Arc::new(renderer);
                *self.renderer.lock() = arc;
                *self.renderer_kind.lock() = RendererKind::Chromecast;
                *self.renderer_label.lock() = name.clone();
                let mut cfg = self.config.lock();
                cfg.renderer = RendererPref::Chromecast;
                cfg.last_chromecast = Some(name.clone());
                let _ = cfg.save();
            }
            RemoteDevice::Upnp(dev) => {
                let renderer = UpnpRenderer::connect(dev).await?;
                let arc: Arc<dyn Renderer> = Arc::new(renderer);
                *self.renderer.lock() = arc;
                *self.renderer_kind.lock() = RendererKind::Upnp;
                *self.renderer_label.lock() = name.clone();
                let mut cfg = self.config.lock();
                cfg.renderer = RendererPref::Upnp;
                cfg.last_upnp = Some(name.clone());
                let _ = cfg.save();
            }
        }
        self.set_status(format!("Streaming to {}.", name));
        Ok(())
    }

    /// Switch to a saved server by name — updates config, rebuilds the
    /// Jellyfin client, and reloads the current screen.
    fn switch_server(&self, name: &str) -> Result<()> {
        let server = {
            let mut cfg = self.config.lock();
            cfg.switch_to(name)?;
            cfg.save()?;
            cfg.current().cloned()
        };
        let Some(server) = server else {
            return Err(anyhow!("no active server after switch"));
        };
        let client = JellyfinClient::with_credentials(
            &server.url,
            &server.device_id,
            &server.user_id,
            &server.access_token,
        )?;
        *self.jellyfin.lock() = Arc::new(client);
        // Clear cached content — belongs to the *previous* server.
        self.music_items.lock().clear();
        self.video_items.lock().clear();
        self.playlists.lock().clear();
        self.playlist_items.lock().clear();
        *self.open_playlist.lock() = None;
        self.search_results.lock().clear();
        self.set_status(format!(
            "◉ Switched to `{}` ({}, as {})",
            server.name, server.url, server.user_name
        ));
        Ok(())
    }

    fn cycle_server(&self) {
        let new_name = {
            let mut cfg = self.config.lock();
            cfg.cycle_next().map(|s| s.name.clone())
        };
        let Some(name) = new_name else {
            self.set_status("No other server to switch to.");
            return;
        };
        if let Err(e) = self.switch_server(&name) {
            self.set_status(format!("switch: {}", e));
        }
    }

    async fn switch_to_mpv(&self) {
        // Local playback: audio → symphonia, video → mpv. Persistence is
        // wired in so switching to local while there's a saved queue picks
        // it up on next restart (the current session keeps whatever queue
        // was already active).
        let queue_path = fin_config::queue_path().ok();
        let renderer = LocalRenderer::with_persist(queue_path);
        let arc: Arc<dyn Renderer> = Arc::new(renderer);
        *self.renderer.lock() = arc;
        *self.renderer_kind.lock() = RendererKind::Mpv;
        *self.renderer_label.lock() = "this machine".into();
        {
            let mut cfg = self.config.lock();
            cfg.renderer = RendererPref::Mpv;
            let _ = cfg.save();
        }
        self.set_status("Streaming locally (symphonia audio, mpv video).");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlayMode {
    /// Default Enter behaviour — drills into containers, plays leaves.
    PlayNow,
    /// `a` — append to the current queue.
    Enqueue,
    /// `n` — play next (insert after the current item).
    PlayNext,
    /// `x` — play the whole container (or leaf) NOW, replacing the queue.
    /// Skips drill-in so an album or playlist starts playing straight away.
    PlayContainer,
}

pub async fn run_tui(app: App) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = app;
    let mut events = spawn_event_loop(Duration::from_millis(200));
    app.load_screen().await;

    let result = event_loop(&mut app, &mut terminal, &mut events).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

async fn event_loop(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    events: &mut mpsc::UnboundedReceiver<AppEvent>,
) -> Result<()> {
    while !app.should_quit {
        // Refresh live playback state each tick.
        *app.playback_state.lock() = app.renderer.lock().state();

        terminal.draw(|f| draw(f, app))?;

        if let Some(ev) = events.recv().await {
            match ev {
                AppEvent::Tick => {
                    app.logo_pulse = app.logo_pulse.wrapping_add(1);
                }
                AppEvent::Resize(_, _) => {}
                AppEvent::Key(k) => handle_key(app, k).await?,
            }
        } else {
            break;
        }
    }
    Ok(())
}

async fn handle_key(app: &mut App, key: KeyEvent) -> Result<()> {
    // fzf-style search: the input eats printable keys and updates results on
    // every keystroke. Arrow keys still navigate the results list; Enter plays
    // the highlighted item.
    if app.screen == Screen::Search && app.search_input_focused {
        match key.code {
            KeyCode::Esc => {
                app.search_input_focused = false;
            }
            KeyCode::Enter => {
                app.play_selected(PlayMode::PlayNow).await;
            }
            KeyCode::Down => {
                let len = app.current_list().len();
                let i = app.list_state.selected().unwrap_or(0);
                if len > 0 {
                    app.list_state.select(Some((i + 1).min(len - 1)));
                }
            }
            KeyCode::Up => {
                let i = app.list_state.selected().unwrap_or(0);
                app.list_state.select(Some(i.saturating_sub(1)));
            }
            KeyCode::Backspace => {
                app.search_query.pop();
                app.list_state.select(Some(0));
                app.run_search();
            }
            KeyCode::Tab => {
                app.search_input_focused = false;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.should_quit = true;
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.search_query.clear();
                app.run_search();
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.play_selected(PlayMode::Enqueue).await;
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.play_selected(PlayMode::PlayNext).await;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.search_query.push(c);
                app.list_state.select(Some(0));
                app.run_search();
            }
            _ => {}
        }
        return Ok(());
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        (KeyCode::Tab, _) | (KeyCode::Right, KeyModifiers::CONTROL) => {
            app.screen = app.screen.next();
            app.list_state.select(Some(0));
            app.load_screen().await;
        }
        (KeyCode::BackTab, _) | (KeyCode::Left, KeyModifiers::CONTROL) => {
            app.screen = app.screen.prev();
            app.list_state.select(Some(0));
            app.load_screen().await;
        }
        (KeyCode::Char('1'), _) => {
            app.screen = Screen::Music;
            *app.open_album.lock() = None;
            app.list_state.select(Some(0));
            app.load_screen().await;
        }
        (KeyCode::Char('2'), _) => {
            app.screen = Screen::Videos;
            *app.open_series.lock() = None;
            app.list_state.select(Some(0));
            app.load_screen().await;
        }
        (KeyCode::Char('3'), _) => {
            app.screen = Screen::Playlists;
            *app.open_playlist.lock() = None;
            app.list_state.select(Some(0));
            app.load_screen().await;
        }
        (KeyCode::Char('4'), _) => {
            app.screen = Screen::Queue;
            app.list_state.select(Some(0));
        }
        (KeyCode::Char('5'), _) => {
            app.screen = Screen::Search;
            app.list_state.select(Some(0));
            app.search_input_focused = true;
        }
        (KeyCode::Char('6'), _) => {
            app.screen = Screen::Devices;
            app.list_state.select(Some(0));
            app.load_screen().await;
        }
        (KeyCode::Char('7'), _) => {
            app.screen = Screen::Settings;
            app.list_state.select(Some(0));
        }
        (KeyCode::Char('/'), _) => {
            app.screen = Screen::Search;
            app.search_input_focused = true;
        }
        (KeyCode::Char('r'), _) => {
            app.load_screen().await;
            app.set_status("Refreshed.");
        }
        (KeyCode::Esc, _) => {
            // Esc pops the current drill-in (album / series / playlist).
            match app.screen {
                Screen::Music if app.open_album.lock().is_some() => {
                    *app.open_album.lock() = None;
                    app.album_tracks.lock().clear();
                    app.list_state.select(Some(0));
                }
                Screen::Videos if app.open_series.lock().is_some() => {
                    *app.open_series.lock() = None;
                    app.series_children.lock().clear();
                    app.list_state.select(Some(0));
                }
                Screen::Playlists if app.open_playlist.lock().is_some() => {
                    *app.open_playlist.lock() = None;
                    app.playlist_items.lock().clear();
                    app.list_state.select(Some(0));
                }
                _ => {}
            }
        }
        (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
            let len = app.list_len();
            let i = app.list_state.selected().unwrap_or(0);
            if len > 0 {
                app.list_state.select(Some((i + 1).min(len - 1)));
            }
        }
        (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
            let i = app.list_state.selected().unwrap_or(0);
            app.list_state.select(Some(i.saturating_sub(1)));
        }
        (KeyCode::PageDown, _) => {
            let len = app.list_len();
            let i = app.list_state.selected().unwrap_or(0);
            if len > 0 {
                app.list_state.select(Some((i + 10).min(len - 1)));
            }
        }
        (KeyCode::PageUp, _) => {
            let i = app.list_state.selected().unwrap_or(0);
            app.list_state.select(Some(i.saturating_sub(10)));
        }
        (KeyCode::Enter, _) => match app.screen {
            Screen::Devices => {
                if let Err(e) = app.use_selected_device().await {
                    app.set_status(format!("device: {}", e));
                }
            }
            Screen::Settings => {
                let name = {
                    let cfg = app.config.lock();
                    app.list_state
                        .selected()
                        .and_then(|i| cfg.servers.get(i).map(|s| s.name.clone()))
                };
                if let Some(name) = name {
                    if let Err(e) = app.switch_server(&name) {
                        app.set_status(format!("switch: {}", e));
                    } else {
                        app.load_screen().await;
                    }
                }
            }
            // The Queue screen shows the *current* queue — Enter jumps the
            // playhead inside it instead of collapsing the queue down to a
            // single item.
            Screen::Queue => app.jump_to_queue_index().await,
            _ => app.play_selected(PlayMode::PlayNow).await,
        },
        (KeyCode::Char('a'), _) => app.play_selected(PlayMode::Enqueue).await,
        (KeyCode::Char('n'), _) => app.play_selected(PlayMode::PlayNext).await,
        // `x` — play the highlighted container *right now*, without drilling in.
        (KeyCode::Char('x'), _) => app.play_selected(PlayMode::PlayContainer).await,
        (KeyCode::Char(' '), _) | (KeyCode::Char('p'), _) => {
            let renderer = app.renderer.lock().clone();
            let state = renderer.state();
            let _ = match state.status {
                fin_player::PlaybackStatus::Playing => renderer.pause().await,
                _ => renderer.resume().await,
            };
        }
        (KeyCode::Char('s'), _) => {
            let renderer = app.renderer.lock().clone();
            let _ = renderer.stop().await;
        }
        (KeyCode::Char('>'), _) | (KeyCode::Char('l'), _) => {
            let renderer = app.renderer.lock().clone();
            let _ = renderer.next().await;
        }
        (KeyCode::Char('<'), _) | (KeyCode::Char('h'), _) => {
            let renderer = app.renderer.lock().clone();
            let _ = renderer.previous().await;
        }
        (KeyCode::Char('+'), _) | (KeyCode::Char('='), _) => {
            let renderer = app.renderer.lock().clone();
            let v = renderer.state().volume + 0.05;
            let _ = renderer.set_volume(v.min(1.5)).await;
        }
        (KeyCode::Char('-'), _) | (KeyCode::Char('_'), _) => {
            let renderer = app.renderer.lock().clone();
            let v = renderer.state().volume - 0.05;
            let _ = renderer.set_volume(v.max(0.0)).await;
        }
        (KeyCode::Char('m'), _) => {
            app.switch_to_mpv().await;
        }
        (KeyCode::Char('t'), _) => {
            app.cycle_server();
            app.load_screen().await;
        }
        (KeyCode::Char('z'), _) => {
            let renderer = app.renderer.lock().clone();
            let current = renderer.state().shuffle;
            let _ = renderer.set_shuffle(!current).await;
            app.set_status(if current {
                "Shuffle off"
            } else {
                "Shuffle on"
            });
        }
        // Shift+R cycles repeat (`r` alone refreshes the screen; `l` is next).
        (KeyCode::Char('R'), _) => {
            let renderer = app.renderer.lock().clone();
            let next = renderer.state().repeat.next();
            let _ = renderer.set_repeat(next).await;
            app.set_status(format!("Repeat: {}", next.label()));
        }
        // Queue-screen-only: `d` removes the highlighted entry, `Shift+C`
        // clears the whole queue. Elsewhere these keys are inert.
        (KeyCode::Char('d'), _) if app.screen == Screen::Queue => {
            app.remove_selected_from_queue().await;
        }
        (KeyCode::Char('C'), _) if app.screen == Screen::Queue => {
            let renderer = app.renderer.lock().clone();
            let _ = renderer.stop().await;
            app.set_status("Queue cleared.");
        }
        // ReplayGain: `g` cycles off → track → album → off.
        (KeyCode::Char('g'), _) => {
            let mut new_settings = {
                let cfg = app.config.lock();
                cfg.replaygain
            };
            new_settings.mode = new_settings.mode.next();
            let renderer = app.renderer.lock().clone();
            if let Err(e) = renderer.set_replaygain(new_settings).await {
                app.set_status(format!("replaygain: {}", e));
            } else {
                let mut cfg = app.config.lock();
                cfg.replaygain = new_settings;
                let _ = cfg.save();
                drop(cfg);
                app.set_status(format!("ReplayGain: {}", new_settings.mode.label()));
            }
        }
        _ => {}
    }
    Ok(())
}

fn draw(f: &mut Frame<'_>, app: &mut App) {
    let size = f.area();
    f.render_widget(Paragraph::new("").style(base_style()), size);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Length(3), // tabs
            Constraint::Min(4),    // main
            Constraint::Length(6), // player bar
            Constraint::Length(1), // status/help
        ])
        .split(size);

    draw_header(f, chunks[0], app);
    draw_tabs(f, chunks[1], app);
    draw_body(f, chunks[2], app);
    draw_player_bar(f, chunks[3], app);
    draw_status_bar(f, chunks[4], app);
}

fn draw_header(f: &mut Frame<'_>, area: Rect, app: &App) {
    let pulse = ((app.logo_pulse as f32 * 0.15).sin() * 0.5 + 0.5) * 60.0 + 195.0;
    let r = 0u8;
    let g = pulse as u8;
    let b = (pulse * 0.85) as u8;
    let subtitle_col = ratatui::style::Color::Rgb(r, g, b);
    let (server, user, server_name, servers_total) = {
        let cfg = app.config.lock();
        let cur = cfg.current();
        (
            cur.map(|s| s.url.clone())
                .unwrap_or_else(|| "not logged in".into()),
            cur.map(|s| s.user_name.clone())
                .unwrap_or_else(|| "guest".into()),
            cur.map(|s| s.name.clone()).unwrap_or_default(),
            cfg.servers.len(),
        )
    };
    let servers_badge = if servers_total > 1 {
        format!("  ⇄ {} servers", servers_total)
    } else {
        String::new()
    };
    let line = Line::from(vec![
        Span::styled(
            "  ⚡ fin ",
            Style::default()
                .fg(Palette::PRIMARY)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "— a neon jellyfin client",
            Style::default().fg(subtitle_col),
        ),
        Span::styled(
            format!("   {} ", user),
            Style::default()
                .fg(Palette::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("@ {} ", server), muted_style()),
        Span::styled(
            if server_name.is_empty() {
                String::new()
            } else {
                format!("[{}]", server_name)
            },
            Style::default().fg(Palette::HIGHLIGHT),
        ),
        Span::styled(servers_badge, Style::default().fg(Palette::SKY)),
    ]);
    let block = neon_block("", false);
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(Paragraph::new(line), inner);
}

fn draw_tabs(f: &mut Frame<'_>, area: Rect, app: &App) {
    let block = neon_block("", false);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let labels: Vec<(&str, &str)> = Screen::ALL.iter().map(|s| (s.icon(), s.label())).collect();
    let selected = Screen::ALL
        .iter()
        .position(|s| *s == app.screen)
        .unwrap_or(0);
    f.render_widget(NeonTabs::new(&labels, selected), inner);
}

fn draw_body(f: &mut Frame<'_>, area: Rect, app: &mut App) {
    match app.screen {
        Screen::Search => draw_search(f, area, app),
        Screen::Devices => draw_devices(f, area, app),
        Screen::Settings => draw_settings(f, area, app),
        Screen::Playlists if app.open_playlist.lock().is_none() => {
            draw_list(f, area, app, " ▤ Playlists ");
        }
        Screen::Playlists => {
            let name = app
                .open_playlist
                .lock()
                .as_ref()
                .map(|p| p.name.clone())
                .unwrap_or_default();
            let title = format!(
                " ▤ {} — {} tracks   (Esc to go back) ",
                name,
                app.playlist_items.lock().len()
            );
            draw_list_with_title(f, area, app, &title);
        }
        Screen::Music if app.open_album.lock().is_none() => {
            draw_list(f, area, app, " ♪ Music — Albums ")
        }
        Screen::Music => {
            let name = app
                .open_album
                .lock()
                .as_ref()
                .map(|a| a.name.clone())
                .unwrap_or_default();
            let sub = app
                .open_album
                .lock()
                .as_ref()
                .map(|a| a.subtitle())
                .unwrap_or_default();
            let title = format!(
                " ◈ {}   {}   — {} track(s)   (Esc to go back) ",
                name,
                sub,
                app.album_tracks.lock().len()
            );
            draw_list_with_title(f, area, app, &title);
        }
        Screen::Videos if app.open_series.lock().is_none() => {
            draw_list(f, area, app, " ▶ Videos — Movies & Series ")
        }
        Screen::Videos => {
            let name = app
                .open_series
                .lock()
                .as_ref()
                .map(|s| s.name.clone())
                .unwrap_or_default();
            let title = format!(
                " ▶ {} — {} episode(s)   (Esc to go back) ",
                name,
                app.series_children.lock().len()
            );
            draw_list_with_title(f, area, app, &title);
        }
        Screen::Queue => {
            let state = app.playback_state.lock().clone();
            let total = state.queue.len();
            let title = if total == 0 {
                " ≡ Queue ".to_string()
            } else {
                let pos = state.current_index.map(|i| i + 1).unwrap_or(0);
                format!(" ≡ Queue  ({}/{}) ", pos, total)
            };
            draw_list(f, area, app, &title);
        }
    }
}

fn draw_list(f: &mut Frame<'_>, area: Rect, app: &mut App, title: &str) {
    draw_list_with_title(f, area, app, title);
}

fn draw_list_with_title(f: &mut Frame<'_>, area: Rect, app: &mut App, title: &str) {
    let items_data = app.current_list();
    // Compute the column layout ONCE, using the width available inside the
    // block minus the 3-column highlight symbol (" ▍ "). Every row uses the
    // same widths so titles / subtitles / times line up as columns.
    let block = neon_block(title, true);
    let inner = block.inner(area);
    let row_width = inner.width.saturating_sub(3); // highlight_symbol reserves 3 cols
    let layout = RowLayout::compute(row_width);

    if items_data.is_empty() {
        f.render_widget(block, area);
        let msg = match app.screen {
            Screen::Queue => "Queue is empty — press Enter on an item to play, `a` to enqueue.",
            Screen::Music | Screen::Videos => "Loading… (press `r` to refresh)",
            Screen::Playlists => "No playlists yet.",
            Screen::Search if app.search_query.trim().is_empty() => {
                "type to search — results update as you type"
            }
            Screen::Search => "no matches",
            _ => "Nothing here.",
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(msg, muted_style())))
                .alignment(Alignment::Center),
            inner.inner(Margin::new(2, 1)),
        );
        return;
    }
    // The Queue screen paints its currently-playing row with a distinct
    // marker. Other screens leave `now_playing` false — they either aren't
    // showing the queue (Music/Videos/Playlists browse the library) or don't
    // have a stable notion of "currently playing" tied to the visible index.
    let playing_idx = if app.screen == Screen::Queue {
        app.playback_state.lock().current_index
    } else {
        None
    };
    let items: Vec<ListItem> = items_data
        .iter()
        .enumerate()
        .map(|(i, it)| {
            ListItem::new(item_row_line(
                it,
                Some(i) == app.list_state.selected(),
                Some(i) == playing_idx,
                layout,
            ))
        })
        .collect();
    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .bg(Palette::SURFACE)
                .fg(Palette::PRIMARY)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(" ▍ ");
    f.render_stateful_widget(list, area, &mut app.list_state);
}

fn draw_search(f: &mut Frame<'_>, area: Rect, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3)])
        .split(area);
    let title = if app.search_input_focused {
        " ⌕ Search  (typing) "
    } else {
        " ⌕ Search  (press / or Tab to type) "
    };
    let block = neon_block(title, app.search_input_focused);
    let inner = block.inner(chunks[0]);
    f.render_widget(block, chunks[0]);

    let prompt_style = Style::default()
        .fg(Palette::PRIMARY)
        .add_modifier(Modifier::BOLD);
    let cursor_visible = app.search_input_focused && app.logo_pulse % 2 == 0;
    let mut spans: Vec<Span> = vec![
        Span::styled("  ", Style::default()),
        Span::styled("⌕ ", prompt_style),
    ];
    if app.search_query.is_empty() {
        spans.push(Span::styled(
            "type to search music, movies, series…",
            muted_style(),
        ));
        if app.search_input_focused {
            spans.push(Span::styled(
                if cursor_visible { "  ▏" } else { "   " },
                Style::default().fg(Palette::PRIMARY),
            ));
        }
    } else {
        spans.push(Span::styled(
            app.search_query.clone(),
            Style::default()
                .fg(Palette::FG)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            if cursor_visible { "▏" } else { " " },
            Style::default()
                .fg(Palette::PRIMARY)
                .add_modifier(Modifier::BOLD),
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(Palette::SURFACE)),
        inner,
    );
    draw_list_with_title(f, chunks[1], app, " ⌕ Results ");
}

fn draw_devices(f: &mut Frame<'_>, area: Rect, app: &mut App) {
    let devices = app.devices.lock().clone();
    let items: Vec<ListItem> = devices
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let selected = Some(i) == app.list_state.selected();
            let icon_col = if selected {
                Palette::PRIMARY
            } else {
                Palette::ACCENT
            };
            let icon = match d {
                RemoteDevice::Cast(_) => " 󰓐 ",
                RemoteDevice::Upnp(_) => " ◈ ",
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    icon,
                    Style::default().fg(icon_col).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    d.display_name(),
                    Style::default()
                        .fg(Palette::FG)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("   [{}]  ", d.kind_label()),
                    Style::default().fg(Palette::HIGHLIGHT),
                ),
                Span::styled(d.subtitle(), Style::default().fg(Palette::SKY)),
            ]))
        })
        .collect();
    let title = " ◈ Renderers  (Chromecast + UPnP — Enter to select, r to rescan) ";
    if items.is_empty() {
        let block = neon_block(title, true);
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "No devices found yet — press `r` to rescan.",
                muted_style(),
            )))
            .alignment(Alignment::Center),
            inner.inner(Margin::new(2, 1)),
        );
        return;
    }
    let list = List::new(items)
        .block(neon_block(title, true))
        .highlight_style(
            Style::default()
                .bg(Palette::SURFACE)
                .fg(Palette::PRIMARY)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(" ▍ ");
    f.render_stateful_widget(list, area, &mut app.list_state);
}

fn draw_settings(f: &mut Frame<'_>, area: Rect, app: &mut App) {
    let cfg_snapshot = app.config.lock().clone();
    let renderer_pref = cfg_snapshot.renderer.label();
    let last_cast = cfg_snapshot
        .last_chromecast
        .clone()
        .unwrap_or_else(|| "—".into());
    let last_upnp = cfg_snapshot.last_upnp.clone().unwrap_or_else(|| "—".into());
    let path = fin_config::config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(4)])
        .split(area);

    // Top card — global settings.
    let lines = vec![
        Line::from(vec![
            Span::styled("  Renderer      ", title_style()),
            Span::styled(renderer_pref, accent_style()),
            Span::styled(
                "   (press m for local, 6 → Enter for a chromecast / UPnP renderer)",
                muted_style(),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Last Cast     ", title_style()),
            Span::styled(last_cast, Style::default().fg(Palette::HIGHLIGHT)),
        ]),
        Line::from(vec![
            Span::styled("  Last UPnP     ", title_style()),
            Span::styled(last_upnp, Style::default().fg(Palette::HIGHLIGHT)),
        ]),
        Line::from(vec![
            Span::styled("  ReplayGain    ", title_style()),
            Span::styled(cfg_snapshot.replaygain.mode.label(), accent_style()),
            Span::styled(
                format!(
                    "   preamp {:+.1} dB   clip-guard {}   (press g to cycle)",
                    cfg_snapshot.replaygain.preamp_db,
                    if cfg_snapshot.replaygain.prevent_clip {
                        "on"
                    } else {
                        "off"
                    }
                ),
                muted_style(),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Config File   ", title_style()),
            Span::styled(path, Style::default().fg(Palette::SKY)),
        ]),
        Line::from(Span::styled(
            "  All settings are also CLI flags — `fin --help`.",
            muted_style(),
        )),
    ];
    let block = neon_block(" ⚙ Settings ", false);
    let inner = block.inner(rows[0]);
    f.render_widget(block, rows[0]);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        inner.inner(Margin::new(2, 0)),
    );

    // Bottom card — interactive server list. Enter switches, `t` cycles.
    let current = cfg_snapshot.current_server.clone().unwrap_or_default();
    let items: Vec<ListItem> = cfg_snapshot
        .servers
        .iter()
        .map(|s| {
            let is_current = s.name == current;
            let marker = if is_current { " ▍ " } else { "   " };
            let name_style = if is_current {
                Style::default()
                    .fg(Palette::PRIMARY)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Palette::FG)
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    marker,
                    Style::default()
                        .fg(Palette::PRIMARY)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("{:<18}", s.name), name_style),
                Span::styled(format!(" {}  ", s.url), Style::default().fg(Palette::SKY)),
                Span::styled(
                    format!("as {}", s.user_name),
                    Style::default().fg(Palette::MUTED),
                ),
            ]))
        })
        .collect();

    let title = format!(
        " ◉ Servers  ({} saved — Enter to switch, t to cycle) ",
        cfg_snapshot.servers.len()
    );

    if items.is_empty() {
        let block = neon_block(&title, true);
        let inner = block.inner(rows[1]);
        f.render_widget(block, rows[1]);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  No servers yet — run `fin login <url>` to add one.",
                muted_style(),
            ))),
            inner.inner(Margin::new(2, 1)),
        );
        return;
    }

    let list = List::new(items)
        .block(neon_block(&title, true))
        .highlight_style(
            Style::default()
                .bg(Palette::SURFACE)
                .fg(Palette::PRIMARY)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("");
    f.render_stateful_widget(list, rows[1], &mut app.list_state);
}

fn draw_player_bar(f: &mut Frame<'_>, area: Rect, app: &App) {
    let renderer_kind = *app.renderer_kind.lock();
    let label = app.renderer_label.lock().clone();
    let state = app.playback_state.lock().clone();
    f.render_widget(
        PlayerBar {
            state: &state,
            renderer: renderer_kind,
            renderer_label: &label,
        },
        area,
    );
}

fn draw_status_bar(f: &mut Frame<'_>, area: Rect, app: &App) {
    // Transient status messages hide the help text; expire them so the help
    // reappears once the user has had a moment to read the message.
    const STATUS_TTL: Duration = Duration::from_secs(4);
    let msg = {
        let mut guard = app.status_message.lock();
        match guard.as_ref() {
            Some((text, set_at)) if set_at.elapsed() < STATUS_TTL => Some(text.clone()),
            Some(_) => {
                *guard = None;
                None
            }
            None => None,
        }
    };
    let help = " tab: screen  ↑↓/jk: nav  enter: play/drill  x: play all  a: queue  n: next  z: shuffle  R: repeat  g: replaygain  space: pause  s: stop  d: rm/C: clear (queue)  </>: skip  +/-: vol  m: local  t: server  /: search  esc: back  q: quit ";
    // Errors/warnings pop in warn-red; other status messages use the primary
    // teal so they stand out from the muted help text.
    let (text, style) = match msg {
        Some(m) if m.contains("failed") || m.contains("error") => (
            m,
            Style::default()
                .fg(Palette::ERROR)
                .add_modifier(Modifier::BOLD),
        ),
        Some(m) => (
            m,
            Style::default()
                .fg(Palette::PRIMARY)
                .add_modifier(Modifier::BOLD),
        ),
        None => (help.to_string(), Style::default().fg(Palette::MUTED)),
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(format!(" {} ", text), style)))
            .style(Style::default().bg(Palette::SURFACE)),
        area,
    );
}

impl App {
    pub fn config_snapshot(&self) -> Config {
        self.config.lock().clone()
    }
    pub fn save_config(&self) -> Result<()> {
        self.config.lock().save().context("saving config")
    }
}
