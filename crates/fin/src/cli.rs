use clap::builder::styling::{AnsiColor, Color, RgbColor, Style, Styles};
use clap::{Args, Parser, Subcommand, ValueEnum};
use fin_config::RendererPref;

/// Neon-electric palette applied to clap's help & error output.
/// Mirrors the ratatui theme in `fin_tui::theme`.
pub fn neon_styles() -> Styles {
    let primary = Color::Rgb(RgbColor(0, 232, 198)); // electric teal — section titles
    let sky = Color::Rgb(RgbColor(0, 210, 255)); // sky blue — flag literals
    let accent = Color::Rgb(RgbColor(130, 100, 255)); // violet — "Usage:" line
    let highlight = Color::Rgb(RgbColor(100, 232, 130)); // mint — valid values
    let link = Color::Rgb(RgbColor(255, 160, 100)); // orange — <PLACEHOLDER> tokens
    let error = Color::Rgb(RgbColor(255, 100, 100)); // red — "error:" prefix

    Styles::styled()
        .header(Style::new().bold().underline().fg_color(Some(primary)))
        .usage(Style::new().bold().fg_color(Some(accent)))
        .literal(Style::new().bold().fg_color(Some(sky)))
        .placeholder(Style::new().fg_color(Some(link)))
        .valid(Style::new().bold().fg_color(Some(highlight)))
        .invalid(
            Style::new()
                .bold()
                .fg_color(Some(Color::Ansi(AnsiColor::Yellow))),
        )
        .error(Style::new().bold().fg_color(Some(error)))
}

/// A clap-friendly mirror of `fin_config::RendererPref`. Kept out of
/// `fin-config` so that library doesn't pull in clap.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RendererArg {
    Mpv,
    Chromecast,
    Upnp,
}

impl From<RendererArg> for RendererPref {
    fn from(a: RendererArg) -> Self {
        match a {
            RendererArg::Mpv => RendererPref::Mpv,
            RendererArg::Chromecast => RendererPref::Chromecast,
            RendererArg::Upnp => RendererPref::Upnp,
        }
    }
}

/// fin — a neon-electric TUI Jellyfin client for mpv & Chromecast.
///
/// Every setting is exposed as both a CLI flag and a TOML key:
///
///   fin --server https://media.example --renderer chromecast --chromecast "Living Room"
///
/// or persist the equivalent settings in the config file (see `fin config --path`).
#[derive(Debug, Parser, Clone)]
#[command(
    author,
    version,
    about,
    long_about,
    propagate_version = true,
    arg_required_else_help = false,
    styles = neon_styles(),
)]
pub struct Cli {
    /// Increase log verbosity (-v, -vv).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Jellyfin server URL. Overrides the TOML `server.url`.
    #[arg(long, global = true, env = "FIN_SERVER")]
    pub server: Option<String>,

    /// Use a saved server by its name (see `fin server`). Overrides `current_server`.
    #[arg(long = "server-name", global = true, env = "FIN_SERVER_NAME")]
    pub server_name: Option<String>,

    /// Access token (skip `login` if you already have one). Overrides `server.access_token`.
    #[arg(long, global = true, env = "FIN_TOKEN")]
    pub token: Option<String>,

    /// User id. Overrides `server.user_id`.
    #[arg(long, global = true, env = "FIN_USER_ID")]
    pub user_id: Option<String>,

    /// User name (display only). Overrides `server.user_name`.
    #[arg(long, global = true)]
    pub user_name: Option<String>,

    /// Client device id. Overrides `server.device_id`.
    #[arg(long, global = true, env = "FIN_DEVICE_ID")]
    pub device_id: Option<String>,

    /// Renderer to use. Advanced form; usually `--mpv` / `--chromecast NAME` is enough.
    #[arg(long, global = true, value_enum, env = "FIN_RENDERER")]
    pub renderer: Option<RendererArg>,

    /// Force the local mpv renderer.
    #[arg(long, global = true, conflicts_with_all = ["chromecast", "upnp", "renderer"])]
    pub mpv: bool,

    /// Cast to a Chromecast by display name. Implies `--renderer chromecast`.
    /// Pass an empty string to auto-pick the first device found.
    #[arg(long, global = true, env = "FIN_CHROMECAST", num_args = 0..=1, default_missing_value = "")]
    pub chromecast: Option<String>,

    /// Stream to a UPnP MediaRenderer by friendly name. Implies `--renderer upnp`.
    /// Pass an empty string to auto-pick the first device found.
    #[arg(long, global = true, env = "FIN_UPNP", num_args = 0..=1, default_missing_value = "", conflicts_with = "chromecast")]
    pub upnp: Option<String>,

    /// Don't advertise this machine as a UPnP MediaRenderer (cast target).
    /// Persistent form: `media_renderer.enabled = false` in the config.
    #[arg(
        long = "no-media-renderer",
        global = true,
        env = "FIN_NO_MEDIA_RENDERER"
    )]
    pub no_media_renderer: bool,

    /// Advertise this machine as a UPnP MediaRenderer even when the config
    /// disables it. (It is on by default — this only overrides the TOML.)
    #[arg(
        long = "media-renderer",
        global = true,
        conflicts_with = "no_media_renderer"
    )]
    pub media_renderer: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand, Clone)]
pub enum Command {
    /// (default) launch the neon TUI.
    Tui,

    /// Authenticate against a Jellyfin server and save credentials.
    /// Servers are stored by name — pass `--name` to keep more than one.
    Login {
        /// Server URL — e.g. https://media.example
        url: String,
        /// Username (prompted if omitted).
        #[arg(long, short = 'u')]
        username: Option<String>,
        /// Password (prompted if omitted, hidden while typing).
        #[arg(long, short = 'p')]
        password: Option<String>,
        /// Short name for this server. Defaults to the URL host
        /// (e.g. `media.example.com`). Reusing an existing name updates it.
        #[arg(long, short = 'n')]
        name: Option<String>,
    },

    /// Remove a saved server. If no `--name` is given, the current server is removed.
    Logout {
        #[arg(long, short = 'n')]
        name: Option<String>,
    },

    /// Manage saved Jellyfin servers.
    #[command(subcommand)]
    Server(ServerCmd),

    /// Search the server from the shell.
    Search {
        /// Query string.
        query: String,
        /// Restrict to a kind: audio | music | video | movies (default: all).
        #[arg(long, short = 'k')]
        kind: Option<String>,
        /// Max results.
        #[arg(long, default_value_t = 25)]
        limit: u32,
    },

    /// Search + play the first hit on the configured renderer.
    Play(PlayArgs),

    /// Search + append to the current renderer queue.
    Queue(PlayArgs),

    /// List Chromecasts and UPnP MediaRenderers on the local network.
    Devices {
        /// Discovery scan duration in seconds (mDNS + SSDP run concurrently).
        #[arg(long, default_value_t = 4)]
        scan_seconds: u64,
    },

    /// List playlists, or dump one when given `--list <id>`.
    Playlists {
        /// Playlist id to dump.
        #[arg(long)]
        list: Option<String>,
    },

    /// Inspect or locate the config file.
    Config {
        /// Print the current config.
        #[arg(long)]
        show: bool,
        /// Print the config file path.
        #[arg(long)]
        path: bool,
    },
}

#[derive(Debug, Subcommand, Clone)]
pub enum ServerCmd {
    /// List all saved servers. The current one is marked ▍.
    #[command(alias = "ls")]
    List,
    /// Switch the active server by name.
    Switch {
        /// Server name (see `fin server list`).
        name: String,
    },
    /// Remove a saved server.
    #[command(alias = "remove")]
    Rm {
        /// Server name (see `fin server list`).
        name: String,
    },
    /// Rename a saved server.
    Rename {
        /// Old name.
        from: String,
        /// New name.
        to: String,
    },
}

#[derive(Debug, Args, Clone)]
pub struct PlayArgs {
    /// Search query. The first hit wins.
    pub query: String,
    /// Restrict to a kind: audio | music | video | movies.
    #[arg(long, short = 'k')]
    pub kind: Option<String>,
    /// Force HLS transcoded stream (default for Chromecast).
    #[arg(long)]
    pub hls: bool,
}

// `Queue` reuses `PlayArgs` semantics. Introducing a distinct type keeps the
// derive-based subcommand parsing simple.
#[derive(Debug, Args, Clone)]
pub struct QueueArgs {
    /// Search query. The first hit wins.
    pub query: String,
    /// Restrict to a kind: audio | music | video | movies.
    #[arg(long, short = 'k')]
    pub kind: Option<String>,
    /// Force HLS transcoded stream (default for Chromecast).
    #[arg(long)]
    pub hls: bool,
}

impl From<PlayArgs> for QueueArgs {
    fn from(p: PlayArgs) -> Self {
        Self {
            query: p.query,
            kind: p.kind,
            hls: p.hls,
        }
    }
}
