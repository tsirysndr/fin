//! ReplayGain support — read `REPLAYGAIN_*` tags from decoded tracks.
//!
//! Reference: <https://en.wikipedia.org/wiki/ReplayGain>
//!
//! The gain itself is applied by the Rockbox DSP's pre-gain (PGA) stage
//! (`rockbox_dsp::Dsp::set_replaygain` + `set_replaygain_gains`), in
//! fixed point, as part of the same pipeline that runs the EQ and tone
//! controls. This module only extracts the tags and computes the f32
//! fallback multiplier ([`ReplayGainInfo::linear_gain`]) for the paths
//! the PGA can't cover: the crossfade-incoming track (routed through the
//! voice DSP config, which Rockbox gives no PGA stage), non-stereo
//! output (the DSP is skipped entirely), and the first primed packet.
//!
//! Fallback gain formula (mirrors Rockbox's `dsp_replaygain_update`):
//! ```text
//! gain_dB   = base_gain_dB + preamp_dB
//! linear    = 10 ^ (gain_dB / 20)
//! ```
//!
//! If clip prevention is on and a peak is known, `linear` is reduced so
//! `linear * peak <= 1.0` — this stops the ~0.5 % of tracks whose album
//! gain would otherwise cause clipping.
//!
//! Falls back gracefully: if the requested mode's tags are missing, we try
//! the other mode; if both are missing, gain is 1.0 (i.e. no adjustment).
//! Rockbox's PGA does the same track ↔ album fallback internally, so the
//! two paths resolve the same gain.

use symphonia::core::formats::FormatReader;
use symphonia::core::meta::StandardTagKey;

// Re-export the config-facing types so consumers of fin-player still see
// them under `fin_player::ReplayGain*`. Definitions live in fin-config
// because they're pure config data.
pub use fin_config::{ReplayGainMode, ReplayGainSettings};

/// The four ReplayGain-related metadata values a track can carry.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ReplayGainInfo {
    pub track_gain_db: Option<f32>,
    pub album_gain_db: Option<f32>,
    pub track_peak: Option<f32>,
    pub album_peak: Option<f32>,
}

impl ReplayGainInfo {
    /// Pull whatever RG tags are present in the current metadata revision
    /// of `format`. Handles both Vorbis-comment style
    /// (`REPLAYGAIN_TRACK_GAIN`) and ID3v2 TXXX (`replaygain_track_gain`)
    /// by matching case-insensitively on the raw key, then also
    /// promotes `StandardTagKey::ReplayGain*` if symphonia populated it.
    pub fn extract_from(format: &mut Box<dyn FormatReader>) -> Self {
        let mut info = Self::default();

        // Preferred path: the current metadata revision (post-probe).
        if let Some(rev) = format.metadata().current() {
            for tag in rev.tags() {
                info.absorb_tag(tag);
            }
        }
        // Some formats (FLAC/Ogg) put the ReplayGain tags in the initial
        // container metadata — MP3 tends to put them in ID3v2 which lands
        // in the same rev queue. The single scan above covers both.
        info
    }

    fn absorb_tag(&mut self, tag: &symphonia::core::meta::Tag) {
        let raw = tag.value.to_string();
        let value = raw.trim();
        // Prefer the std_key if symphonia identified it.
        if let Some(std_key) = tag.std_key {
            match std_key {
                StandardTagKey::ReplayGainTrackGain => {
                    self.track_gain_db = self.track_gain_db.or(parse_db(value));
                }
                StandardTagKey::ReplayGainAlbumGain => {
                    self.album_gain_db = self.album_gain_db.or(parse_db(value));
                }
                StandardTagKey::ReplayGainTrackPeak => {
                    self.track_peak = self.track_peak.or(parse_peak(value));
                }
                StandardTagKey::ReplayGainAlbumPeak => {
                    self.album_peak = self.album_peak.or(parse_peak(value));
                }
                _ => {}
            }
            return;
        }
        // Fallback: match the raw key case-insensitively.
        let key = tag.key.to_ascii_lowercase();
        match key.as_str() {
            "replaygain_track_gain" => {
                self.track_gain_db = self.track_gain_db.or(parse_db(value));
            }
            "replaygain_album_gain" => {
                self.album_gain_db = self.album_gain_db.or(parse_db(value));
            }
            "replaygain_track_peak" => {
                self.track_peak = self.track_peak.or(parse_peak(value));
            }
            "replaygain_album_peak" => {
                self.album_peak = self.album_peak.or(parse_peak(value));
            }
            _ => {}
        }
    }

    /// Compute the f32 fallback multiplier for samples that bypass the
    /// Rockbox PGA stage (see module docs). Returns 1.0 when the mode is
    /// `Off` or the requested tags aren't present (with a fall-through to
    /// the other mode's tags).
    pub fn linear_gain(&self, settings: ReplayGainSettings) -> f32 {
        if !settings.mode.is_active() {
            return 1.0;
        }
        // Pick gain + peak based on mode, falling back to the other scope
        // if the requested one is missing.
        let (gain_db, peak) = match settings.mode {
            ReplayGainMode::Album => (
                self.album_gain_db.or(self.track_gain_db),
                self.album_peak.or(self.track_peak),
            ),
            ReplayGainMode::Track => (
                self.track_gain_db.or(self.album_gain_db),
                self.track_peak.or(self.album_peak),
            ),
            ReplayGainMode::Off => unreachable!(),
        };
        let Some(gain_db) = gain_db else {
            return 1.0;
        };
        let total_db = gain_db + settings.preamp_db;
        let mut linear = db_to_linear(total_db);
        if settings.prevent_clip {
            if let Some(peak) = peak {
                if peak > 0.0 && linear * peak > 1.0 {
                    linear = 1.0 / peak;
                }
            }
        }
        linear
    }
}

fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Parse an RG gain value like `"-6.5 dB"`, `"+3.14"`, `"0"`. Returns
/// `None` for empty / unparseable strings.
fn parse_db(s: &str) -> Option<f32> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Strip a trailing " dB" (case-insensitive) if present.
    let num_part = trimmed
        .to_ascii_lowercase()
        .trim_end_matches(" db")
        .trim()
        .to_string();
    num_part.parse::<f32>().ok()
}

/// Parse an RG peak value like `"1.0"` or `"0.988312"`. Same lenient rules
/// as `parse_db` — negative or non-numeric input becomes `None`.
fn parse_peak(s: &str) -> Option<f32> {
    let v: f32 = s.trim().parse().ok()?;
    if v.is_finite() && v >= 0.0 {
        Some(v)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_db_accepts_common_shapes() {
        assert_eq!(parse_db("-6.5 dB"), Some(-6.5));
        assert_eq!(parse_db("  +3.14  "), Some(3.14));
        assert_eq!(parse_db("0"), Some(0.0));
        assert_eq!(parse_db("-6.5 DB"), Some(-6.5));
    }

    #[test]
    fn parse_db_rejects_junk() {
        assert_eq!(parse_db(""), None);
        assert_eq!(parse_db("not a number"), None);
    }

    #[test]
    fn parse_peak_bounds() {
        assert_eq!(parse_peak("0.988"), Some(0.988));
        assert_eq!(parse_peak("1.0"), Some(1.0));
        assert_eq!(parse_peak(""), None);
        assert_eq!(parse_peak("-0.5"), None);
        assert_eq!(parse_peak("nan"), None);
    }

    #[test]
    fn db_to_linear_reference_values() {
        assert!((db_to_linear(0.0) - 1.0).abs() < 1e-6);
        // -6 dB ≈ 0.501187
        assert!((db_to_linear(-6.0) - 0.501187).abs() < 1e-4);
        // +6 dB ≈ 1.995262
        assert!((db_to_linear(6.0) - 1.995262).abs() < 1e-4);
    }

    fn info_with(track: Option<f32>, album: Option<f32>, peak: Option<f32>) -> ReplayGainInfo {
        ReplayGainInfo {
            track_gain_db: track,
            album_gain_db: album,
            track_peak: peak,
            album_peak: peak,
        }
    }

    #[test]
    fn linear_gain_off_is_unity() {
        let info = info_with(Some(-6.0), Some(-4.0), Some(0.9));
        let s = ReplayGainSettings::default(); // Off
        assert!((info.linear_gain(s) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn linear_gain_track_mode_uses_track_gain() {
        let info = info_with(Some(-6.0), Some(-3.0), None);
        let s = ReplayGainSettings {
            mode: ReplayGainMode::Track,
            preamp_db: 0.0,
            prevent_clip: false,
        };
        // -6 dB → ~0.501
        assert!((info.linear_gain(s) - 0.501187).abs() < 1e-4);
    }

    #[test]
    fn linear_gain_falls_back_when_tag_missing() {
        // Album mode requested, but only track gain is tagged → use track.
        let info = info_with(Some(-6.0), None, None);
        let s = ReplayGainSettings {
            mode: ReplayGainMode::Album,
            preamp_db: 0.0,
            prevent_clip: false,
        };
        assert!((info.linear_gain(s) - 0.501187).abs() < 1e-4);
    }

    #[test]
    fn linear_gain_returns_unity_when_all_tags_missing() {
        let info = ReplayGainInfo::default();
        let s = ReplayGainSettings {
            mode: ReplayGainMode::Track,
            preamp_db: 0.0,
            prevent_clip: false,
        };
        assert_eq!(info.linear_gain(s), 1.0);
    }

    #[test]
    fn preamp_is_additive_in_db_domain() {
        let info = info_with(Some(-6.0), None, None);
        let s = ReplayGainSettings {
            mode: ReplayGainMode::Track,
            preamp_db: 6.0,
            prevent_clip: false,
        };
        // -6 + 6 = 0 dB → 1.0 linear
        assert!((info.linear_gain(s) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn clip_prevention_caps_linear_when_peak_would_clip() {
        // +12 dB pushes linear well above 1.0. With peak=1.0 that clips —
        // prevent_clip should hold linear at 1/peak = 1.0.
        let info = info_with(Some(12.0), None, Some(1.0));
        let s = ReplayGainSettings {
            mode: ReplayGainMode::Track,
            preamp_db: 0.0,
            prevent_clip: true,
        };
        assert!((info.linear_gain(s) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn clip_prevention_off_lets_gain_exceed_unity() {
        let info = info_with(Some(12.0), None, Some(1.0));
        let s = ReplayGainSettings {
            mode: ReplayGainMode::Track,
            preamp_db: 0.0,
            prevent_clip: false,
        };
        // +12 dB ≈ 3.981
        assert!((info.linear_gain(s) - 3.981).abs() < 1e-3);
    }
}
