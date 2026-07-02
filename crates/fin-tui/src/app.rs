use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

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
    discover_chromecasts, CastDevice, ChromecastRenderer, MpvRenderer, PlaybackState, QueueItem,
    Renderer, RendererKind,
};

use crate::event::{spawn_event_loop, AppEvent};
use crate::screens::{item_row_line, Screen};
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
    home_recent: Arc<Mutex<Vec<BaseItem>>>,
    home_resume: Arc<Mutex<Vec<BaseItem>>>,
    music_items: Arc<Mutex<Vec<BaseItem>>>,
    video_items: Arc<Mutex<Vec<BaseItem>>>,
    playlists: Arc<Mutex<Vec<BaseItem>>>,
    playlist_items: Arc<Mutex<Vec<BaseItem>>>,
    open_playlist: Arc<Mutex<Option<BaseItem>>>,
    search_results: Arc<Mutex<Vec<BaseItem>>>,
    devices: Arc<Mutex<Vec<CastDevice>>>,
    search_generation: Arc<AtomicU64>,
    // ephemeral
    search_query: String,
    search_input_focused: bool,
    list_state: ListState,
    status_message: Arc<Mutex<Option<String>>>,
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
        };
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            config: Arc::new(Mutex::new(config)),
            jellyfin: Arc::new(Mutex::new(Arc::new(jellyfin))),
            renderer: Arc::new(Mutex::new(renderer)),
            renderer_kind: Arc::new(Mutex::new(kind)),
            renderer_label: Arc::new(Mutex::new(label)),
            screen: Screen::Home,
            home_recent: Arc::new(Mutex::new(vec![])),
            home_resume: Arc::new(Mutex::new(vec![])),
            music_items: Arc::new(Mutex::new(vec![])),
            video_items: Arc::new(Mutex::new(vec![])),
            playlists: Arc::new(Mutex::new(vec![])),
            playlist_items: Arc::new(Mutex::new(vec![])),
            open_playlist: Arc::new(Mutex::new(None)),
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
            Screen::Home => {
                let mut v = self.home_resume.lock().clone();
                v.extend(self.home_recent.lock().clone());
                v
            }
            Screen::Search => self.search_results.lock().clone(),
            Screen::Music => self.music_items.lock().clone(),
            Screen::Videos => self.video_items.lock().clone(),
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

    fn set_status(&self, msg: impl Into<String>) {
        *self.status_message.lock() = Some(msg.into());
    }

    async fn load_screen(&self) {
        match self.screen {
            Screen::Home => {
                let jf = self.jf();
                let resume = self.home_resume.clone();
                let recent = self.home_recent.clone();
                let status = self.status_message.clone();
                tokio::spawn(async move {
                    match jf.resume(12).await {
                        Ok(v) => *resume.lock() = v,
                        Err(e) => *status.lock() = Some(format!("resume: {}", e)),
                    }
                    match jf.latest(None, 24).await {
                        Ok(v) => *recent.lock() = v,
                        Err(e) => *status.lock() = Some(format!("latest: {}", e)),
                    }
                });
            }
            Screen::Music => {
                let jf = self.jf();
                let out = self.music_items.clone();
                let status = self.status_message.clone();
                tokio::spawn(async move {
                    match jf
                        .items(None, &["MusicAlbum"], true, Some("SortName"), Some(200))
                        .await
                    {
                        Ok(v) => *out.lock() = v,
                        Err(e) => *status.lock() = Some(format!("music: {}", e)),
                    }
                });
            }
            Screen::Videos => {
                let jf = self.jf();
                let out = self.video_items.clone();
                let status = self.status_message.clone();
                tokio::spawn(async move {
                    match jf
                        .items(
                            None,
                            &["Movie", "Series"],
                            true,
                            Some("SortName"),
                            Some(200),
                        )
                        .await
                    {
                        Ok(v) => *out.lock() = v,
                        Err(e) => *status.lock() = Some(format!("videos: {}", e)),
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
                        Err(e) => *status.lock() = Some(format!("playlists: {}", e)),
                    }
                });
            }
            Screen::Devices => {
                let out = self.devices.clone();
                let status = self.status_message.clone();
                tokio::spawn(async move {
                    *status.lock() = Some("Scanning for Chromecasts…".into());
                    match discover_chromecasts(Duration::from_secs(4)).await {
                        Ok(v) => {
                            *status.lock() = Some(format!("Found {} device(s).", v.len()));
                            *out.lock() = v;
                        }
                        Err(e) => *status.lock() = Some(format!("scan: {}", e)),
                    }
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
            return;
        }
        let jf = self.jf();
        let out = self.search_results.clone();
        let status = self.status_message.clone();
        let generation = self.search_generation.clone();
        tokio::spawn(async move {
            // small debounce lets fast typists coalesce keystrokes
            tokio::time::sleep(Duration::from_millis(90)).await;
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
                        *status.lock() = Some(format!("{} match(es) for “{}”", v.len(), query));
                        *out.lock() = v;
                    }
                }
                Err(e) => {
                    if generation.load(Ordering::SeqCst) == gen {
                        *status.lock() = Some(format!("search: {}", e));
                    }
                }
            }
        });
    }

    fn base_item_to_queue(&self, item: &BaseItem, format: StreamFormat) -> Result<QueueItem> {
        let url = self.jf().stream_url(item, format)?;
        let is_video = item.kind().is_video();
        let content_type = if is_video {
            "video/mp4".to_string()
        } else {
            "audio/mpeg".to_string()
        };
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
                        Some("SortName"),
                        Some(500),
                    )
                    .await
            }
            ItemKind::MusicArtist => {
                self.jf()
                    .items(Some(&item.id), &["Audio"], true, Some("Album"), Some(500))
                    .await
            }
            ItemKind::Playlist => self.jf().playlist_items(&item.id).await,
            ItemKind::Series => {
                self.jf()
                    .items(
                        Some(&item.id),
                        &["Episode"],
                        true,
                        Some("SortName"),
                        Some(500),
                    )
                    .await
            }
            _ => Ok(vec![item]),
        }
    }

    async fn play_selected(&mut self, mode: PlayMode) {
        let Some(item) = self.selected_item() else {
            self.set_status("Nothing selected.");
            return;
        };
        // If we're in the Playlists screen with no playlist open, opening the
        // playlist should load its contents instead of playing.
        if self.screen == Screen::Playlists && self.open_playlist.lock().is_none() {
            self.open_playlist_selected(item).await;
            return;
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
        let format = match *self.renderer_kind.lock() {
            RendererKind::Mpv => StreamFormat::Direct,
            RendererKind::Chromecast => StreamFormat::Hls,
        };
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
            PlayMode::PlayNow => renderer.play(queue_items, 0).await,
            PlayMode::Enqueue => renderer.enqueue(queue_items).await,
            PlayMode::PlayNext => renderer.play_next(queue_items).await,
        };
        match result {
            Ok(()) => self.set_status(match mode {
                PlayMode::PlayNow => format!("▶ Playing “{}” ({} item(s))", title, count),
                PlayMode::Enqueue => format!("+ Queued {} item(s)", count),
                PlayMode::PlayNext => format!("↥ Playing next: {} item(s)", count),
            }),
            Err(e) => self.set_status(format!("renderer: {}", e)),
        }
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
                    *status.lock() = Some(format!("Loaded {} items", v.len()));
                    *out.lock() = v;
                }
                Err(e) => *status.lock() = Some(format!("playlist: {}", e)),
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
        self.set_status(format!("Connecting to {}…", sel.display_name()));
        let renderer = ChromecastRenderer::connect(sel.clone()).await?;
        let arc: Arc<dyn Renderer> = Arc::new(renderer);
        *self.renderer.lock() = arc;
        *self.renderer_kind.lock() = RendererKind::Chromecast;
        *self.renderer_label.lock() = sel.display_name();
        // persist preference
        {
            let mut cfg = self.config.lock();
            cfg.renderer = RendererPref::Chromecast;
            cfg.last_chromecast = Some(sel.display_name());
            let _ = cfg.save();
        }
        self.set_status(format!("Streaming to {}.", sel.display_name()));
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
        self.home_recent.lock().clear();
        self.home_resume.lock().clear();
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
        let renderer = MpvRenderer::new(None);
        let arc: Arc<dyn Renderer> = Arc::new(renderer);
        *self.renderer.lock() = arc;
        *self.renderer_kind.lock() = RendererKind::Mpv;
        *self.renderer_label.lock() = "this machine".into();
        {
            let mut cfg = self.config.lock();
            cfg.renderer = RendererPref::Mpv;
            let _ = cfg.save();
        }
        self.set_status("Streaming to local mpv.");
    }
}

#[derive(Debug, Clone, Copy)]
enum PlayMode {
    PlayNow,
    Enqueue,
    PlayNext,
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
            app.screen = Screen::Home;
            app.list_state.select(Some(0));
            app.load_screen().await;
        }
        (KeyCode::Char('2'), _) => {
            app.screen = Screen::Search;
            app.list_state.select(Some(0));
            app.search_input_focused = true;
        }
        (KeyCode::Char('3'), _) => {
            app.screen = Screen::Music;
            app.list_state.select(Some(0));
            app.load_screen().await;
        }
        (KeyCode::Char('4'), _) => {
            app.screen = Screen::Videos;
            app.list_state.select(Some(0));
            app.load_screen().await;
        }
        (KeyCode::Char('5'), _) => {
            app.screen = Screen::Playlists;
            *app.open_playlist.lock() = None;
            app.list_state.select(Some(0));
            app.load_screen().await;
        }
        (KeyCode::Char('6'), _) => {
            app.screen = Screen::Queue;
            app.list_state.select(Some(0));
        }
        (KeyCode::Char('7'), _) => {
            app.screen = Screen::Devices;
            app.list_state.select(Some(0));
            app.load_screen().await;
        }
        (KeyCode::Char('8'), _) => {
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
            if app.screen == Screen::Playlists && app.open_playlist.lock().is_some() {
                *app.open_playlist.lock() = None;
                app.playlist_items.lock().clear();
                app.list_state.select(Some(0));
            }
        }
        (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
            let len = app.current_list().len();
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
            let len = app.current_list().len();
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
            _ => app.play_selected(PlayMode::PlayNow).await,
        },
        (KeyCode::Char('a'), _) => app.play_selected(PlayMode::Enqueue).await,
        (KeyCode::Char('n'), _) => app.play_selected(PlayMode::PlayNext).await,
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
            let title = " ▤ Playlists ";
            draw_list(f, area, app, title);
        }
        Screen::Playlists => {
            let name = app
                .open_playlist
                .lock()
                .as_ref()
                .map(|p| p.name.clone())
                .unwrap_or_default();
            let title = format!(" ▤ {} — {} tracks ", name, app.playlist_items.lock().len());
            draw_list_with_title(f, area, app, &title);
        }
        Screen::Home => draw_list(
            f,
            area,
            app,
            " ◉ Home — Continue Watching / Recently Added ",
        ),
        Screen::Music => draw_list(f, area, app, " ♪ Music — Albums "),
        Screen::Videos => draw_list(f, area, app, " ▶ Videos — Movies & Series "),
        Screen::Queue => draw_list(f, area, app, " ≡ Queue "),
    }
}

fn draw_list(f: &mut Frame<'_>, area: Rect, app: &mut App, title: &str) {
    draw_list_with_title(f, area, app, title);
}

fn draw_list_with_title(f: &mut Frame<'_>, area: Rect, app: &mut App, title: &str) {
    let items_data = app.current_list();
    let items: Vec<ListItem> = items_data
        .iter()
        .enumerate()
        .map(|(i, it)| ListItem::new(item_row_line(it, Some(i) == app.list_state.selected())))
        .collect();
    if items.is_empty() {
        let block = neon_block(title, true);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let msg = match app.screen {
            Screen::Queue => "Queue is empty — press Enter on an item to play, `a` to enqueue.",
            Screen::Home => "Loading… (press `r` to refresh)",
            Screen::Playlists => "No playlists yet.",
            _ => "Nothing here.",
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(msg, muted_style())))
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

fn draw_search(f: &mut Frame<'_>, area: Rect, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3)])
        .split(area);
    let block = neon_block(" ⌕ Search ", app.search_input_focused);
    let inner = block.inner(chunks[0]);
    f.render_widget(block, chunks[0]);
    let indicator = if app.search_input_focused {
        Span::styled(
            "▍ ",
            Style::default()
                .fg(Palette::PRIMARY)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("  ", Style::default())
    };
    let line = Line::from(vec![
        indicator,
        Span::styled(
            if app.search_query.is_empty() {
                "type to search music, movies, series…".to_string()
            } else {
                app.search_query.clone()
            },
            if app.search_query.is_empty() {
                muted_style()
            } else {
                Style::default()
                    .fg(Palette::FG)
                    .add_modifier(Modifier::BOLD)
            },
        ),
        Span::styled(
            if app.search_input_focused { "▊" } else { "" },
            Style::default()
                .fg(Palette::PRIMARY)
                .add_modifier(Modifier::SLOW_BLINK),
        ),
    ]);
    f.render_widget(Paragraph::new(line), inner);
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
            ListItem::new(Line::from(vec![
                Span::styled(
                    " 󰓐 ",
                    Style::default().fg(icon_col).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    d.display_name(),
                    Style::default()
                        .fg(Palette::FG)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("   {}  ", d.model),
                    Style::default().fg(Palette::MUTED),
                ),
                Span::styled(
                    format!("{}:{}", d.address, d.port),
                    Style::default().fg(Palette::SKY),
                ),
            ]))
        })
        .collect();
    let title = " ◈ Chromecast Devices  (Enter to select, r to rescan) ";
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
    let path = fin_config::config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(6), Constraint::Min(4)])
        .split(area);

    // Top card — global settings.
    let lines = vec![
        Line::from(vec![
            Span::styled("  Renderer      ", title_style()),
            Span::styled(renderer_pref, accent_style()),
            Span::styled(
                "   (press m for mpv, 7 → Enter for a chromecast)",
                muted_style(),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Last Cast     ", title_style()),
            Span::styled(last_cast, Style::default().fg(Palette::HIGHLIGHT)),
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
    let msg = app.status_message.lock().clone();
    let help = " tab: screen  ↑↓/jk: nav  enter: play/switch  a: queue  n: next  space: pause  s: stop  </>: skip  +/-: vol  m: mpv  t: next server  /: search  q: quit ";
    let text = msg.unwrap_or_else(|| help.to_string());
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {} ", text),
            Style::default().fg(Palette::MUTED),
        )))
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
