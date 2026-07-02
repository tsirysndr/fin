use std::fs;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    /// Name of the active server (must exist in `servers`).
    #[serde(default)]
    pub current_server: Option<String>,
    /// All servers the user has authenticated against.
    #[serde(default)]
    pub servers: Vec<ServerConfig>,
    /// Legacy single-server slot. Kept only for one-shot migration; new
    /// writes drop it. If both this and `servers` are present, `servers`
    /// wins and this is ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<LegacyServerConfig>,
    #[serde(default)]
    pub renderer: RendererPref,
    #[serde(default)]
    pub last_chromecast: Option<String>,
    #[serde(default)]
    pub client: ClientInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Short label the user picks (or auto-derived from the URL host).
    pub name: String,
    pub url: String,
    pub user_id: String,
    pub user_name: String,
    pub access_token: String,
    pub device_id: String,
}

/// The old, single-server shape we used before multi-server support landed.
/// Loaded once, then folded into `servers` and never written again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyServerConfig {
    pub url: String,
    pub user_id: String,
    pub user_name: String,
    pub access_token: String,
    pub device_id: String,
}

impl From<LegacyServerConfig> for ServerConfig {
    fn from(l: LegacyServerConfig) -> Self {
        let name = derive_server_name(&l.url);
        Self {
            name,
            url: l.url,
            user_id: l.user_id,
            user_name: l.user_name,
            access_token: l.access_token,
            device_id: l.device_id,
        }
    }
}

/// Derive a short server name from a URL — the host, without the scheme.
///
///   https://media.example.com:8096/path  →  "media.example.com"
///   http://192.168.1.42                    →  "192.168.1.42"
pub fn derive_server_name(url: &str) -> String {
    let trimmed = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host: &str = trimmed
        .split(|c: char| c == '/' || c == ':')
        .next()
        .unwrap_or(trimmed);
    if host.is_empty() {
        "server".to_string()
    } else {
        host.to_string()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum RendererPref {
    #[default]
    Mpv,
    Chromecast,
}

impl RendererPref {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Mpv => "mpv",
            Self::Chromecast => "chromecast",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

impl Default for ClientInfo {
    fn default() -> Self {
        Self {
            name: "fin".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        }
    }
}

fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("app", "rocksky", "fin").context("could not determine project dirs")
}

pub fn config_path() -> Result<PathBuf> {
    let dirs = project_dirs()?;
    Ok(dirs.config_dir().join("config.toml"))
}

pub fn config_dir() -> Result<PathBuf> {
    let dirs = project_dirs()?;
    Ok(dirs.config_dir().to_path_buf())
}

pub fn cache_dir() -> Result<PathBuf> {
    let dirs = project_dirs()?;
    Ok(dirs.cache_dir().to_path_buf())
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("reading config at {}", path.display()))?;
        let mut cfg: Self = toml::from_str(&text).context("parsing config file")?;
        cfg.migrate_legacy_server();
        Ok(cfg)
    }

    /// Fold a leftover `[server]` block into the `servers` list. Idempotent.
    fn migrate_legacy_server(&mut self) {
        if let Some(legacy) = self.server.take() {
            let migrated: ServerConfig = legacy.into();
            let already = self.servers.iter().any(|s| s.url == migrated.url);
            if !already {
                let name = migrated.name.clone();
                self.servers.push(migrated);
                if self.current_server.is_none() {
                    self.current_server = Some(name);
                }
            }
        }
        if self.current_server.is_none() {
            self.current_server = self.servers.first().map(|s| s.name.clone());
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).context("creating config dir")?;
        }
        let text = toml::to_string_pretty(self).context("serializing config")?;
        fs::write(&path, text).with_context(|| format!("writing config to {}", path.display()))?;
        Ok(())
    }

    pub fn current(&self) -> Option<&ServerConfig> {
        let name = self.current_server.as_ref()?;
        self.servers.iter().find(|s| &s.name == name)
    }

    pub fn require_current(&self) -> Result<&ServerConfig> {
        self.current()
            .context("no active server — run `fin login <url>` or `fin server switch <name>`")
    }

    pub fn find_server(&self, name: &str) -> Option<&ServerConfig> {
        self.servers.iter().find(|s| s.name == name)
    }

    /// Add or update a server (upsert by name) and mark it as the current one.
    pub fn add_or_update_server(&mut self, server: ServerConfig) {
        if let Some(existing) = self.servers.iter_mut().find(|s| s.name == server.name) {
            *existing = server.clone();
        } else {
            self.servers.push(server.clone());
        }
        self.current_server = Some(server.name);
    }

    /// Switch the active server by name.
    pub fn switch_to(&mut self, name: &str) -> Result<()> {
        if !self.servers.iter().any(|s| s.name == name) {
            return Err(anyhow!(
                "no server named `{}` — try `fin server` to list them",
                name
            ));
        }
        self.current_server = Some(name.to_string());
        Ok(())
    }

    /// Remove a server by name. Falls back to the first remaining server as
    /// the new current one, or clears `current_server` if none remain.
    pub fn remove_server(&mut self, name: &str) -> Result<()> {
        let before = self.servers.len();
        self.servers.retain(|s| s.name != name);
        if self.servers.len() == before {
            return Err(anyhow!("no server named `{}`", name));
        }
        if self.current_server.as_deref() == Some(name) {
            self.current_server = self.servers.first().map(|s| s.name.clone());
        }
        Ok(())
    }

    pub fn cycle_next(&mut self) -> Option<&ServerConfig> {
        if self.servers.is_empty() {
            return None;
        }
        let cur = self
            .current_server
            .as_deref()
            .and_then(|n| self.servers.iter().position(|s| s.name == n))
            .unwrap_or(0);
        let next = (cur + 1) % self.servers.len();
        self.current_server = Some(self.servers[next].name.clone());
        self.servers.get(next)
    }
}
