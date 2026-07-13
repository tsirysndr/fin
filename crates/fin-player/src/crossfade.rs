//! Crossfade configuration types.
//!
//! The cross-track blending itself — keeping the outgoing track's tail
//! playing while the incoming track fades in, on a manual skip or an
//! automatic advance — is a faithful port of Rockbox's `pcmbuf` mixer that
//! lives in the `rockbox-playback` engine. fin only carries the user's
//! settings, so this module just re-exports the config-facing types (defined
//! in `fin-config` because they're pure config data) under
//! `fin_player::Crossfade*`.

pub use fin_config::{CrossfadeMode, CrossfadeSettings};
