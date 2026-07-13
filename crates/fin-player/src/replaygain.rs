//! ReplayGain configuration types.
//!
//! Reference: <https://en.wikipedia.org/wiki/ReplayGain>
//!
//! The actual gain — reading the `REPLAYGAIN_*` tags off each track and
//! applying track/album gain with a preamp and optional clip-prevention — is
//! done natively by the `rockbox-playback` engine's DSP. fin only carries the
//! user's settings, so this module just re-exports the config-facing types
//! (defined in `fin-config` because they're pure config data) under
//! `fin_player::ReplayGain*`.

pub use fin_config::{ReplayGainMode, ReplayGainSettings};
