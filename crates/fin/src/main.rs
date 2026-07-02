mod cli;
mod preflight;

use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use fin_config::{derive_server_name, Config, RendererPref, ServerConfig};
use fin_jellyfin::{ItemKind, JellyfinClient, StreamFormat};
use fin_player::{
    discover_chromecasts, CastDevice, ChromecastRenderer, MpvRenderer, QueueItem, Renderer,
    RendererKind,
};
use fin_tui::{run_tui, App};
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command, PlayArgs, ServerCmd};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    // rustls 0.23 requires an explicit CryptoProvider. reqwest picks one via
    // its own feature flags, but `rust_cast` pulls in rustls without a
    // provider selection, so we install one process-wide before anything
    // touches TLS. `install_default` is a no-op if a provider was already
    // installed by another crate.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    // preflight: mpv must be installed no matter what renderer the user picks
    preflight::ensure_mpv()?;

    // Merge CLI flags into the on-disk config so any setting can be overridden inline.
    let mut config = load_and_merge(&cli)?;

    match cli.command.clone().unwrap_or(Command::Tui) {
        Command::Tui => run_tui_cmd(config).await,
        Command::Login {
            url,
            username,
            password,
            name,
        } => cmd_login(&mut config, url, username, password, name).await,
        Command::Logout { name } => cmd_logout(&mut config, name),
        Command::Server(cmd) => cmd_server(&mut config, cmd),
        Command::Search { query, kind, limit } => cmd_search(&config, &query, kind, limit).await,
        Command::Play(args) => cmd_play(&config, args, false).await,
        Command::Queue(args) => cmd_play(&config, args, true).await,
        Command::Devices { scan_seconds } => cmd_devices(scan_seconds).await,
        Command::Playlists { list } => cmd_playlists(&config, list).await,
        Command::Config { show, path } => cmd_config(&config, show, path),
    }
}

fn init_tracing(verbose: u8) {
    // mdns-sd logs an ERROR line when its ServiceDaemon shuts down
    // ("failed to send response of shutdown: sending on a closed channel")
    // during clean process exit. Cosmetic — silence it unless the user
    // explicitly asked for debug output.
    let level = match verbose {
        0 => "warn,fin=info,mdns_sd=off",
        1 => "info,fin=debug,mdns_sd=warn",
        _ => "debug",
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| level.into()))
        .with_target(false)
        .with_writer(io::stderr)
        .try_init();
}

fn load_and_merge(cli: &Cli) -> Result<Config> {
    let mut cfg = Config::load()?;

    // 1) Pick which server this invocation should use.
    if let Some(name) = cli.server_name.as_deref() {
        // Non-fatal: if the name doesn't exist yet, we let the actual
        // command (e.g. `login`) create it.
        if cfg.find_server(name).is_some() {
            cfg.switch_to(name)?;
        } else {
            cfg.current_server = Some(name.to_string());
        }
    }

    // 2) Layer inline flag overrides onto the *current* server, if any.
    //    `--server URL` on its own (no `--server-name`) points to an
    //    ephemeral server used for this run only, so we upsert one under
    //    the URL host as its name.
    if let Some(url) = &cli.server {
        let name = cli
            .server_name
            .clone()
            .unwrap_or_else(|| derive_server_name(url));
        let existing = cfg.find_server(&name).cloned();
        let merged = ServerConfig {
            name: name.clone(),
            url: url.clone(),
            user_id: cli
                .user_id
                .clone()
                .or_else(|| existing.as_ref().map(|s| s.user_id.clone()))
                .unwrap_or_default(),
            user_name: cli
                .user_name
                .clone()
                .or_else(|| existing.as_ref().map(|s| s.user_name.clone()))
                .unwrap_or_default(),
            access_token: cli
                .token
                .clone()
                .or_else(|| existing.as_ref().map(|s| s.access_token.clone()))
                .unwrap_or_default(),
            device_id: cli
                .device_id
                .clone()
                .or_else(|| existing.as_ref().map(|s| s.device_id.clone()))
                .unwrap_or_default(),
        };
        cfg.add_or_update_server(merged);
    } else {
        // No URL override — still let per-field flags patch the current server.
        if let Some(cur_name) = cfg.current_server.clone() {
            if let Some(server) = cfg.servers.iter_mut().find(|s| s.name == cur_name) {
                if let Some(t) = &cli.token {
                    server.access_token = t.clone();
                }
                if let Some(u) = &cli.user_id {
                    server.user_id = u.clone();
                }
                if let Some(u) = &cli.user_name {
                    server.user_name = u.clone();
                }
                if let Some(d) = &cli.device_id {
                    server.device_id = d.clone();
                }
            }
        }
    }

    // Shortcut flags win over `--renderer` and the on-disk pref.
    // Default (when nothing is set) → mpv.
    if cli.mpv {
        cfg.renderer = RendererPref::Mpv;
    } else if let Some(name) = &cli.chromecast {
        cfg.renderer = RendererPref::Chromecast;
        if !name.is_empty() {
            cfg.last_chromecast = Some(name.clone());
        }
    } else if let Some(r) = cli.renderer {
        cfg.renderer = r.into();
    }
    Ok(cfg)
}

fn make_client(cfg: &Config) -> Result<JellyfinClient> {
    let server = cfg.require_current()?;
    JellyfinClient::with_credentials(
        &server.url,
        &server.device_id,
        &server.user_id,
        &server.access_token,
    )
}

async fn build_renderer(cfg: &Config) -> Result<(Arc<dyn Renderer>, String)> {
    match cfg.renderer {
        RendererPref::Mpv => {
            let r = MpvRenderer::new(None);
            Ok((Arc::new(r), "this machine".to_string()))
        }
        RendererPref::Chromecast => {
            // Try to reconnect to the previously used device, otherwise
            // pick the first one we discover.
            let devices = discover_chromecasts(Duration::from_secs(3)).await?;
            let target = pick_chromecast(&devices, cfg.last_chromecast.as_deref())?;
            let name = target.display_name();
            let r = ChromecastRenderer::connect(target).await?;
            Ok((Arc::new(r), name))
        }
    }
}

fn pick_chromecast(devices: &[CastDevice], preferred: Option<&str>) -> Result<CastDevice> {
    if devices.is_empty() {
        anyhow::bail!("no Chromecasts found on the local network");
    }
    if let Some(name) = preferred {
        if let Some(d) = devices
            .iter()
            .find(|d| d.display_name().eq_ignore_ascii_case(name))
        {
            return Ok(d.clone());
        }
    }
    Ok(devices[0].clone())
}

async fn run_tui_cmd(cfg: Config) -> Result<()> {
    let client = make_client(&cfg)?;
    let (renderer, _) = build_renderer(&cfg).await?;
    let app = App::new(cfg, client, renderer);
    run_tui(app).await
}

async fn cmd_login(
    config: &mut Config,
    url: String,
    username: Option<String>,
    password: Option<String>,
    name: Option<String>,
) -> Result<()> {
    let url = normalize_url(&url);
    let server_name = name.unwrap_or_else(|| derive_server_name(&url));
    let username = match username {
        Some(u) => u,
        None => prompt_line("Username: ")?,
    };
    let password = match password {
        Some(p) => p,
        None => prompt_password("Password: ")?,
    };
    let mut client = JellyfinClient::new(&url)?;
    let auth = client.login(&username, &password).await?;
    let server = ServerConfig {
        name: server_name.clone(),
        url,
        user_id: auth.user.id.clone(),
        user_name: auth.user.name.clone(),
        access_token: auth.access_token.clone(),
        device_id: client.device_id().to_string(),
    };
    config.add_or_update_server(server);
    config.save()?;
    println!(
        "✓ signed in as {} on `{}` — now the active server",
        auth.user.name, server_name
    );
    if config.servers.len() > 1 {
        println!(
            "  ({} servers saved — switch anytime with `fin server switch <name>`)",
            config.servers.len()
        );
    }
    Ok(())
}

fn cmd_logout(config: &mut Config, name: Option<String>) -> Result<()> {
    let target = name
        .or_else(|| config.current_server.clone())
        .context("no server to log out of")?;
    config.remove_server(&target)?;
    config.save()?;
    println!("✓ removed `{}`", target);
    if let Some(cur) = &config.current_server {
        println!("  now active: `{}`", cur);
    } else {
        println!("  no servers left — run `fin login <url>` to add one");
    }
    Ok(())
}

fn cmd_server(config: &mut Config, sub: ServerCmd) -> Result<()> {
    match sub {
        ServerCmd::List => {
            if config.servers.is_empty() {
                println!("no servers yet — run `fin login <url>` to add one");
                return Ok(());
            }
            let cur = config.current_server.as_deref();
            for s in &config.servers {
                let marker = if Some(s.name.as_str()) == cur {
                    "▍"
                } else {
                    " "
                };
                println!(
                    "{} {:<20} {:<45}  as {}",
                    marker, s.name, s.url, s.user_name
                );
            }
        }
        ServerCmd::Switch { name } => {
            config.switch_to(&name)?;
            config.save()?;
            let s = config.current().unwrap();
            println!(
                "✓ active server → `{}` ({}, as {})",
                s.name, s.url, s.user_name
            );
        }
        ServerCmd::Rm { name } => {
            config.remove_server(&name)?;
            config.save()?;
            println!("✓ removed `{}`", name);
        }
        ServerCmd::Rename { from, to } => {
            let Some(idx) = config.servers.iter().position(|s| s.name == from) else {
                anyhow::bail!("no server named `{}`", from);
            };
            if config.servers.iter().any(|s| s.name == to) {
                anyhow::bail!("a server named `{}` already exists", to);
            }
            config.servers[idx].name = to.clone();
            if config.current_server.as_deref() == Some(from.as_str()) {
                config.current_server = Some(to.clone());
            }
            config.save()?;
            println!("✓ `{}` → `{}`", from, to);
        }
    }
    Ok(())
}

async fn cmd_search(cfg: &Config, query: &str, kind: Option<String>, limit: u32) -> Result<()> {
    let client = make_client(cfg)?;
    let types: Vec<&str> = match kind.as_deref() {
        Some("audio") | Some("music") => vec!["Audio", "MusicAlbum", "MusicArtist"],
        Some("video") | Some("movies") => vec!["Movie", "Series", "Episode"],
        _ => vec![
            "Audio",
            "MusicAlbum",
            "MusicArtist",
            "Movie",
            "Series",
            "Episode",
        ],
    };
    let items = client.search(query, &types, limit).await?;
    for it in &items {
        println!("{}  {}   {}", it.kind().icon(), it.name, it.subtitle());
    }
    if items.is_empty() {
        eprintln!("no matches");
    }
    Ok(())
}

async fn cmd_play(cfg: &Config, args: PlayArgs, queue: bool) -> Result<()> {
    let client = make_client(cfg)?;
    // Direct stream by default — Jellyfin serves the source container as-is,
    // so no unnecessary transcoding happens. `--hls` forces the m3u8 path.
    let format = if args.hls {
        StreamFormat::Hls
    } else {
        StreamFormat::Direct
    };
    let types: Vec<&str> = match args.kind.as_deref() {
        Some("audio") | Some("music") => vec!["Audio", "MusicAlbum"],
        Some("video") | Some("movie") | Some("movies") => vec!["Movie", "Episode"],
        _ => vec!["Audio", "MusicAlbum", "Movie", "Episode"],
    };
    let hits = client.search(&args.query, &types, 10).await?;
    let Some(hit) = hits.into_iter().next() else {
        anyhow::bail!("nothing matched '{}'", args.query);
    };
    let items = match hit.kind() {
        ItemKind::MusicAlbum => {
            client
                .items(
                    Some(&hit.id),
                    &["Audio"],
                    false,
                    Some("ParentIndexNumber,IndexNumber,SortName"),
                    None,
                )
                .await?
        }
        _ => vec![hit],
    };
    let mut queue_items = Vec::new();
    for it in items {
        let url = client.stream_url(&it, format)?;
        let is_video = it.kind().is_video();
        queue_items.push(QueueItem {
            id: it.id.clone(),
            title: it.name.clone(),
            subtitle: it.subtitle(),
            stream_url: url.clone(),
            image_url: None,
            duration_secs: it.duration_secs(),
            is_video,
            // Content-Type follows the URL we just built — the single source
            // of truth so the receiver knows exactly what it's getting.
            content_type: JellyfinClient::content_type_for_url(&url).to_string(),
        });
    }
    let (renderer, label) = build_renderer(cfg).await?;
    if queue {
        renderer.enqueue(queue_items.clone()).await?;
        println!("+ queued {} item(s) on {}", queue_items.len(), label);
    } else {
        renderer.play(queue_items.clone(), 0).await?;
        println!(
            "▶ playing “{}” on {}",
            queue_items.first().map(|q| q.title.as_str()).unwrap_or(""),
            label
        );
    }
    // For chromecast, keep the process alive so playback keeps flowing.
    if renderer.kind() == RendererKind::Chromecast {
        println!("Ctrl+C to disconnect.");
        tokio::signal::ctrl_c().await?;
    }
    Ok(())
}

async fn cmd_devices(scan_seconds: u64) -> Result<()> {
    println!("scanning for Chromecasts ({}s)…", scan_seconds);
    let devices = discover_chromecasts(Duration::from_secs(scan_seconds)).await?;
    if devices.is_empty() {
        eprintln!("no devices found");
        return Ok(());
    }
    for d in devices {
        println!(
            "󰓐 {}   [{}]   {}:{}",
            d.display_name(),
            d.model,
            d.address,
            d.port
        );
    }
    Ok(())
}

async fn cmd_playlists(cfg: &Config, list_id: Option<String>) -> Result<()> {
    let client = make_client(cfg)?;
    match list_id {
        None => {
            let pls = client.playlists().await?;
            for p in pls {
                println!("▤ {}   ({})", p.name, p.id);
            }
        }
        Some(id) => {
            let items = client.playlist_items(&id).await?;
            for it in items {
                println!("{}  {}   {}", it.kind().icon(), it.name, it.subtitle());
            }
        }
    }
    Ok(())
}

fn cmd_config(cfg: &Config, show: bool, path: bool) -> Result<()> {
    if path {
        println!("{}", fin_config::config_path()?.display());
        return Ok(());
    }
    if show {
        let text = toml::to_string_pretty(cfg).context("serialize config")?;
        print!("{}", text);
        return Ok(());
    }
    println!(
        "config file: {}\nuse --show to print, --path to print just the path",
        fin_config::config_path()?.display()
    );
    Ok(())
}

fn normalize_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{}", trimmed)
    }
}

fn prompt_line(msg: &str) -> Result<String> {
    print!("{}", msg);
    io::stdout().flush().ok();
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    Ok(buf.trim().to_string())
}

/// Read a secret from the TTY without echoing it. Falls back to the standard
/// prompt only when stdin is not a TTY (e.g. piped input from a script) so
/// automation still works.
fn prompt_password(msg: &str) -> Result<String> {
    let secret = rpassword::prompt_password(msg).context("reading password")?;
    Ok(secret)
}
