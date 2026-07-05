# Changelog

All notable changes to `fin` are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.1] - 2026-07-05

### Added
- **Shuffle-play shortcut** — `Shift+X` plays the highlighted container
  (album, artist, playlist, series) or, on flat views, everything in the
  current list (all favorite tracks, all videos, an open album/playlist's
  tracks) in random order. The pool is shuffled up front so the first track
  is random too, and shuffle mode is switched on for whatever joins the
  queue later.

### Changed
- **ReplayGain now runs in the Rockbox DSP** — upgraded `rockbox-dsp` to
  0.2.0 and moved gain application into its pre-gain (PGA) stage, the same
  fixed-point pipeline as the EQ and tone controls. Tag extraction stays in
  fin; the old f32 multiplier survives only as a fallback for paths the PGA
  can't reach (crossfade-incoming track, non-stereo output, first primed
  packet).

## [0.3.0] - 2026-07-05

### Added
- **Subsonic backend** with auto-detection at login — point `fin login` at a
  Jellyfin *or* Subsonic server (Navidrome, Airsonic, Gonic, Astiga, …) and
  the flavour is probed automatically. Multiple servers of either kind can be
  saved side by side.
- **Favorites tab** (`♥`, key `4`) listing everything you've liked — Jellyfin
  favorites and Subsonic stars alike. Supports the usual actions (Enter to
  play/drill, `a` enqueue, `n` play next, `x` play container, `r` refresh).
- **Like / dislike shortcuts** — `Shift+L` favorites (stars) the highlighted
  item, `Shift+D` removes it. Both fall back to the now-playing track on
  screens without a library selection, so you can like a song from anywhere
  while it plays. Disliking from the Favorites tab drops the row immediately.
- **Server-backend badge** — the header shows `◈ Jellyfin` / `≋ Subsonic` for
  the active server, and each row in the Settings server list shows its
  backend between the name and URL.
- **In-process audio path** — HTTP streaming, `symphonia` decode, resampling,
  and `cpal` output all run in-process; mpv is now used only for video.
- **10-band Rockbox equalizer** with interactive band selection and per-band
  gain nudging on the Settings screen.
- **Bass & treble shelves** (Rockbox tone controls), `b`/`Shift+B` and
  `y`/`Shift+Y`.
- **ReplayGain** (off / track / album) and **crossfade** (crossfade / mixed
  modes, cyclable duration).
- **Queue persistence**, shuffle, repeat, and queue-management keys; Enter on
  the Queue screen jumps the playhead instead of collapsing the queue.
- **Album detail view** with disc grouping, track numbers, year, and a
  client-side sort safety net.
- **`?` keyboard-shortcuts help modal.**

### Changed
- Screen tab shortcuts: Music `1`, Videos `2`, Playlists `3`, Favorites `4`,
  Queue `5`, Search `6`, Devices `7`, Settings `8`.
- README, in-app header, and help modal describe fin as a Jellyfin **and
  Subsonic** client rather than Jellyfin-only.

### Fixed
- "Recent listens not updated" on the Jellyfin/Subsonic scrobble path.
- Scrobble/session errors now stay warn-only instead of interrupting playback.
- Now-playing marker no longer shifts by one after a play-triggered crossfade.

## [0.2.0] - 2026-07-03

### Added
- **UPnP MediaRenderer** discovery and streaming (Sonos, Kodi, Roon endpoints,
  Samsung/LG TVs, gmediarender, …) alongside the existing local and Chromecast
  renderers.

## [0.1.0] - 2026-07-03

### Added
- Initial release — a Ratatui TUI plus one-shot CLI for Jellyfin.
- Local **mpv** playback and **Chromecast** streaming.
- fzf-style instant search, playlist browsing, and drill-in navigation.
- Full pagination of the Items endpoint so large libraries load completely.

[0.3.1]: https://github.com/tsirysndr/fin/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/tsirysndr/fin/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/tsirysndr/fin/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/tsirysndr/fin/releases/tag/v0.1.0
