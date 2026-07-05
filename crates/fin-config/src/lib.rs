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
    pub last_upnp: Option<String>,
    #[serde(default)]
    pub client: ClientInfo,
    #[serde(default)]
    pub replaygain: ReplayGainSettings,
    #[serde(default)]
    pub crossfade: CrossfadeSettings,
    /// Rockbox-style top-level equalizer on/off. Matches Rockbox
    /// `settings.toml` schema so users can share EQ presets.
    #[serde(default)]
    pub eq_enabled: bool,
    /// 10 EQ bands. Band 0 is a low shelf, band 9 a high shelf, the rest
    /// are peaking filters. `q` and `gain` are in Rockbox tenths (Q × 10,
    /// dB × 10); `cutoff` is plain Hz. Fresh configs get the ISO-octave
    /// flat preset (32 Hz…16 kHz, Q 7.0, 0 dB) via `#[serde(default = …)]`.
    #[serde(default = "default_eq_band_settings")]
    pub eq_band_settings: Vec<EqBand>,
    /// Bass shelf gain in whole dB (matches Rockbox `bass`). Range −24…+24.
    #[serde(default)]
    pub bass: i32,
    /// Treble shelf gain in whole dB (matches Rockbox `treble`). Range −24…+24.
    #[serde(default)]
    pub treble: i32,
    /// Bass shelf cutoff in Hz. `0` = Rockbox default 200 Hz.
    #[serde(default)]
    pub bass_cutoff: i32,
    /// Treble shelf cutoff in Hz. `0` = Rockbox default 3500 Hz.
    #[serde(default)]
    pub treble_cutoff: i32,
}

/// The ISO-octave 10-band flat preset used when a fresh config has no
/// `[[eq_band_settings]]` section. Cutoffs are the standard ISO center
/// frequencies (32 / 63 / 125 / 250 / 500 / 1000 / 2000 / 4000 / 8000 /
/// 16000 Hz); Q is 7.0 across the board; every gain is 0 dB, so the DSP
/// output is bit-identical to bypass until the user starts tweaking.
pub fn default_eq_band_settings() -> Vec<EqBand> {
    const CUTOFFS_HZ: [i32; 10] = [
        32, 63, 125, 250, 500, 1000, 2000, 4000, 8000, 16000,
    ];
    CUTOFFS_HZ
        .iter()
        .map(|&hz| EqBand {
            cutoff: hz,
            q: 70,
            gain: 0,
        })
        .collect()
}

/// One EQ band, in the exact on-disk format Rockbox's `[[eq_band_settings]]`
/// uses so presets round-trip losslessly between the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EqBand {
    /// Cutoff frequency in Hz.
    pub cutoff: i32,
    /// Q × 10 (e.g. `70` = Q 7.0).
    pub q: i32,
    /// Gain × 10 in dB (e.g. `-125` = −12.5 dB).
    pub gain: i32,
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
    Upnp,
}

impl RendererPref {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Mpv => "mpv",
            Self::Chromecast => "chromecast",
            Self::Upnp => "upnp",
        }
    }
}

/// Which scope of ReplayGain to honor at playback time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ReplayGainMode {
    #[default]
    Off,
    Track,
    Album,
}

impl ReplayGainMode {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Track => "track",
            Self::Album => "album",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::Track,
            Self::Track => Self::Album,
            Self::Album => Self::Off,
        }
    }

    pub fn is_active(&self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// Config-facing ReplayGain settings. Behavior (tag extraction + linear
/// gain computation) lives in `fin_player::replaygain`; this is just data.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ReplayGainSettings {
    #[serde(default)]
    pub mode: ReplayGainMode,
    #[serde(default = "default_replaygain_preamp_db")]
    pub preamp_db: f32,
    #[serde(default = "default_replaygain_prevent_clip")]
    pub prevent_clip: bool,
}

impl Default for ReplayGainSettings {
    fn default() -> Self {
        Self {
            mode: ReplayGainMode::Off,
            preamp_db: default_replaygain_preamp_db(),
            prevent_clip: default_replaygain_prevent_clip(),
        }
    }
}

fn default_replaygain_preamp_db() -> f32 {
    0.0
}

fn default_replaygain_prevent_clip() -> bool {
    true
}

/// How to blend adjacent tracks in the playback queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CrossfadeMode {
    #[default]
    Off,
    /// Cosine/sine fade curves — outgoing fades out while incoming fades
    /// in. Perceived loudness stays constant across the overlap.
    Crossfade,
    /// No fade — both tracks play at full volume during the overlap and
    /// sum additively. Louder in the overlap window; sounds like a DJ mix.
    Mixed,
}

impl CrossfadeMode {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Crossfade => "crossfade",
            Self::Mixed => "mixed",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::Crossfade,
            Self::Crossfade => Self::Mixed,
            Self::Mixed => Self::Off,
        }
    }

    pub fn is_active(&self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// Config-facing crossfade settings. Behavior (dual-track decode + mixing)
/// lives in fin-player; this is just data.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CrossfadeSettings {
    #[serde(default)]
    pub mode: CrossfadeMode,
    /// Overlap duration in seconds. Only meaningful when `mode` is active.
    #[serde(default = "default_crossfade_secs")]
    pub duration_secs: f32,
}

impl Default for CrossfadeSettings {
    fn default() -> Self {
        Self {
            mode: CrossfadeMode::Off,
            duration_secs: default_crossfade_secs(),
        }
    }
}

fn default_crossfade_secs() -> f32 {
    5.0
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

/// Where the persisted playback queue lives. Kept in the cache dir (not
/// config) because it's ephemeral state — a stale file just means the user
/// starts with an empty queue.
pub fn queue_path() -> Result<PathBuf> {
    Ok(cache_dir()?.join("queue.json"))
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            let mut cfg = Self::default();
            cfg.ensure_eq_bands();
            return Ok(cfg);
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("reading config at {}", path.display()))?;
        let mut cfg: Self = toml::from_str(&text).context("parsing config file")?;
        cfg.migrate_legacy_server();
        cfg.ensure_eq_bands();
        Ok(cfg)
    }

    /// Guarantee the EQ always has the full 10 bands. Fills missing slots
    /// from `default_eq_band_settings()` so the DSP has something to pass
    /// through and the TUI can render 10 sliders regardless of prior state.
    fn ensure_eq_bands(&mut self) {
        if self.eq_band_settings.is_empty() {
            self.eq_band_settings = default_eq_band_settings();
        } else if self.eq_band_settings.len() < 10 {
            let defaults = default_eq_band_settings();
            for i in self.eq_band_settings.len()..10 {
                self.eq_band_settings.push(defaults[i]);
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn srv(name: &str) -> ServerConfig {
        ServerConfig {
            name: name.into(),
            url: format!("http://{name}"),
            user_id: "u".into(),
            user_name: "user".into(),
            access_token: "tok".into(),
            device_id: "dev".into(),
        }
    }

    // ------------------------------------------------------------------
    // derive_server_name
    // ------------------------------------------------------------------

    #[test]
    fn derive_name_strips_scheme_and_path() {
        assert_eq!(
            derive_server_name("https://media.example.com:8096/path"),
            "media.example.com"
        );
        assert_eq!(derive_server_name("http://192.168.1.42"), "192.168.1.42");
        assert_eq!(derive_server_name("http://host:8096"), "host");
    }

    #[test]
    fn derive_name_falls_back_when_input_is_empty() {
        assert_eq!(derive_server_name(""), "server");
        assert_eq!(derive_server_name("http://"), "server");
    }

    // ------------------------------------------------------------------
    // Server management
    // ------------------------------------------------------------------

    #[test]
    fn add_or_update_inserts_new_and_marks_current() {
        let mut cfg = Config::default();
        cfg.add_or_update_server(srv("a"));
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.current_server.as_deref(), Some("a"));
    }

    #[test]
    fn add_or_update_overwrites_existing_by_name() {
        let mut cfg = Config::default();
        cfg.add_or_update_server(srv("a"));
        let mut updated = srv("a");
        updated.access_token = "new-token".into();
        cfg.add_or_update_server(updated);
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.servers[0].access_token, "new-token");
    }

    #[test]
    fn switch_to_moves_current_pointer() {
        let mut cfg = Config::default();
        cfg.add_or_update_server(srv("a"));
        cfg.add_or_update_server(srv("b"));
        cfg.switch_to("a").unwrap();
        assert_eq!(cfg.current_server.as_deref(), Some("a"));
    }

    #[test]
    fn switch_to_unknown_server_errors() {
        let mut cfg = Config::default();
        cfg.add_or_update_server(srv("a"));
        assert!(cfg.switch_to("missing").is_err());
        // Current unchanged.
        assert_eq!(cfg.current_server.as_deref(), Some("a"));
    }

    #[test]
    fn remove_current_falls_back_to_first_remaining() {
        let mut cfg = Config::default();
        cfg.add_or_update_server(srv("a"));
        cfg.add_or_update_server(srv("b"));
        cfg.add_or_update_server(srv("c"));
        cfg.switch_to("b").unwrap();
        cfg.remove_server("b").unwrap();
        assert_eq!(
            cfg.servers
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>(),
            vec!["a", "c"]
        );
        assert_eq!(cfg.current_server.as_deref(), Some("a"));
    }

    #[test]
    fn remove_non_current_keeps_current_untouched() {
        let mut cfg = Config::default();
        cfg.add_or_update_server(srv("a"));
        cfg.add_or_update_server(srv("b"));
        // add_or_update_server makes the *added* server current; assert current is b, then remove a.
        assert_eq!(cfg.current_server.as_deref(), Some("b"));
        cfg.remove_server("a").unwrap();
        assert_eq!(cfg.current_server.as_deref(), Some("b"));
    }

    #[test]
    fn remove_last_server_clears_current() {
        let mut cfg = Config::default();
        cfg.add_or_update_server(srv("a"));
        cfg.remove_server("a").unwrap();
        assert!(cfg.servers.is_empty());
        assert_eq!(cfg.current_server, None);
    }

    #[test]
    fn remove_unknown_returns_err() {
        let mut cfg = Config::default();
        cfg.add_or_update_server(srv("a"));
        assert!(cfg.remove_server("missing").is_err());
    }

    #[test]
    fn cycle_next_wraps_around() {
        let mut cfg = Config::default();
        cfg.add_or_update_server(srv("a"));
        cfg.add_or_update_server(srv("b"));
        cfg.add_or_update_server(srv("c"));
        cfg.switch_to("a").unwrap();
        assert_eq!(cfg.cycle_next().map(|s| s.name.clone()), Some("b".into()));
        assert_eq!(cfg.cycle_next().map(|s| s.name.clone()), Some("c".into()));
        // Wraps back to the first.
        assert_eq!(cfg.cycle_next().map(|s| s.name.clone()), Some("a".into()));
    }

    #[test]
    fn cycle_next_on_empty_returns_none() {
        let mut cfg = Config::default();
        assert!(cfg.cycle_next().is_none());
    }

    #[test]
    fn find_server_returns_by_exact_name() {
        let mut cfg = Config::default();
        cfg.add_or_update_server(srv("prod"));
        cfg.add_or_update_server(srv("dev"));
        assert!(cfg.find_server("prod").is_some());
        assert!(cfg.find_server("nope").is_none());
    }

    // ------------------------------------------------------------------
    // Playback-mode enum helpers
    // ------------------------------------------------------------------

    #[test]
    fn replaygain_mode_labels_and_cycle() {
        assert_eq!(ReplayGainMode::Off.label(), "off");
        assert_eq!(ReplayGainMode::Track.label(), "track");
        assert_eq!(ReplayGainMode::Album.label(), "album");
        assert_eq!(ReplayGainMode::Off.next(), ReplayGainMode::Track);
        assert_eq!(ReplayGainMode::Track.next(), ReplayGainMode::Album);
        assert_eq!(ReplayGainMode::Album.next(), ReplayGainMode::Off);
        assert!(!ReplayGainMode::Off.is_active());
        assert!(ReplayGainMode::Track.is_active());
        assert!(ReplayGainMode::Album.is_active());
    }

    #[test]
    fn crossfade_mode_labels_and_cycle() {
        assert_eq!(CrossfadeMode::Off.label(), "off");
        assert_eq!(CrossfadeMode::Crossfade.label(), "crossfade");
        assert_eq!(CrossfadeMode::Mixed.label(), "mixed");
        assert_eq!(CrossfadeMode::Off.next(), CrossfadeMode::Crossfade);
        assert_eq!(CrossfadeMode::Crossfade.next(), CrossfadeMode::Mixed);
        assert_eq!(CrossfadeMode::Mixed.next(), CrossfadeMode::Off);
        assert!(!CrossfadeMode::Off.is_active());
        assert!(CrossfadeMode::Crossfade.is_active());
        assert!(CrossfadeMode::Mixed.is_active());
    }

    #[test]
    fn crossfade_default_duration_is_five_seconds() {
        // The public contract: fresh installs get a 5 s overlap when the
        // mode is turned on, matching the README + Settings hint.
        let s = CrossfadeSettings::default();
        assert!((s.duration_secs - 5.0).abs() < 1e-6);
        assert_eq!(s.mode, CrossfadeMode::Off);
    }

    #[test]
    fn replaygain_default_has_clip_guard_on() {
        // Clip prevention on by default — safer for typical listeners.
        let s = ReplayGainSettings::default();
        assert!(s.prevent_clip);
        assert!((s.preamp_db - 0.0).abs() < 1e-6);
        assert_eq!(s.mode, ReplayGainMode::Off);
    }

    // ------------------------------------------------------------------
    // Default EQ preset
    // ------------------------------------------------------------------

    #[test]
    fn default_eq_band_settings_has_ten_iso_octave_bands() {
        let bands = default_eq_band_settings();
        assert_eq!(bands.len(), 10);
        // First = 32 Hz low shelf, last = 16 kHz high shelf.
        assert_eq!(bands[0].cutoff, 32);
        assert_eq!(bands[9].cutoff, 16_000);
        // Every band flat + Q 7.0.
        for b in &bands {
            assert_eq!(b.q, 70);
            assert_eq!(b.gain, 0);
        }
        // Strictly ascending cutoffs — a monotonic sweep across the audible
        // range so users get a meaningful sliders layout out of the box.
        for w in bands.windows(2) {
            assert!(w[0].cutoff < w[1].cutoff);
        }
    }

    #[test]
    fn ensure_eq_bands_fills_empty_with_defaults() {
        let mut cfg = Config::default();
        assert!(cfg.eq_band_settings.is_empty());
        cfg.ensure_eq_bands();
        assert_eq!(cfg.eq_band_settings.len(), 10);
        assert_eq!(cfg.eq_band_settings[0].cutoff, 32);
    }

    #[test]
    fn ensure_eq_bands_pads_short_lists_to_ten() {
        let mut cfg = Config::default();
        // Simulate a truncated preset with three custom bands.
        cfg.eq_band_settings = vec![
            EqBand {
                cutoff: 100,
                q: 70,
                gain: 30,
            },
            EqBand {
                cutoff: 300,
                q: 70,
                gain: 15,
            },
            EqBand {
                cutoff: 900,
                q: 70,
                gain: -20,
            },
        ];
        cfg.ensure_eq_bands();
        assert_eq!(cfg.eq_band_settings.len(), 10);
        // First three preserved verbatim.
        assert_eq!(cfg.eq_band_settings[0].cutoff, 100);
        assert_eq!(cfg.eq_band_settings[1].cutoff, 300);
        assert_eq!(cfg.eq_band_settings[2].cutoff, 900);
        // Slots 3..9 filled from the ISO default at their respective indices.
        let defaults = default_eq_band_settings();
        for i in 3..10 {
            assert_eq!(cfg.eq_band_settings[i], defaults[i]);
        }
    }

    #[test]
    fn ensure_eq_bands_leaves_full_lists_alone() {
        let mut cfg = Config::default();
        let mut custom = default_eq_band_settings();
        // Tweak one so we can spot mutation.
        custom[5].gain = 87;
        cfg.eq_band_settings = custom.clone();
        cfg.ensure_eq_bands();
        assert_eq!(cfg.eq_band_settings, custom);
    }
}
