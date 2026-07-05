# fin

[![Release](https://github.com/tsirysndr/fin/actions/workflows/release.yml/badge.svg)](https://github.com/tsirysndr/fin/actions/workflows/release.yml)

> a Jellyfin client for the terminal — powered by `symphonia`, `mpv`, Chromecast, and UPnP

![fin — neon-electric Jellyfin TUI](.github/assets/preview.png)

`fin` is a Rust TUI + one-shot CLI that talks to your Jellyfin server, searches
your library, manages playlists, and pushes streams to your local machine
(**symphonia** for audio, **mpv** for video), any **Chromecast** on your
network, or any **UPnP MediaRenderer** (Sonos, Kodi, Roon endpoints, Samsung/LG
TVs, gmediarender, …). Local playback is now audio-native — HTTP streaming,
decoding, resampling, and output all run in-process, and audio never touches
mpv. Remote playback is fully queued, with client-side auto-advance.

## Contents

- [Features](#features)
- [Install](#install)
  - [macOS / Linux — Homebrew](#macos--linux--homebrew)
  - [Debian / Ubuntu — `.deb`](#debian--ubuntu--deb)
  - [Fedora / RHEL / openSUSE — `.rpm`](#fedora--rhel--opensuse--rpm)
  - [Arch — from AUR / source](#arch--from-aur--source)
  - [Prebuilt tarballs](#prebuilt-tarballs)
  - [From source](#from-source)
  - [Nix](#nix)
- [Getting started](#getting-started)
- [Renderer selection](#renderer-selection)
- [All settings](#all-settings)
- [Multiple servers](#multiple-servers)
- [Sub-commands](#sub-commands)
- [Keybindings (TUI)](#keybindings-tui)
- [Playback modes & effects](#playback-modes--effects)
  - [Shuffle](#shuffle)
  - [Repeat](#repeat)
  - [ReplayGain](#replaygain)
  - [Crossfade](#crossfade)
  - [Equalizer](#equalizer)
  - [Bass & treble](#bass--treble)
- [Queue persistence](#queue-persistence)
- [Remote-renderer queue](#remote-renderer-queue)
- [Streams & transcoding](#streams--transcoding)
- [Development](#development)
- [License](#license)

## Features

- **Ratatui-based TUI** with a neon-electric palette (teal / cyan / violet).
- **In-process audio** — HTTP streaming + `symphonia` decode (MP3, FLAC, AAC,
  Opus, Vorbis, ALAC, WAV, …) + resampling + `cpal` output. mpv is used only
  for video.
- **fzf-style instant search** — results update on every keystroke.
- **Drill-in navigation** — Enter on an album lists its tracks, Enter on a
  series lists its episodes, Enter on a playlist lists its items. `x`
  plays the whole container in one go.
- **No list truncation** — Music, Videos, and Playlists fetch every item
  the server has, so nothing stays hidden past an arbitrary limit.
- **Three renderers**, one interface:
  - **local** (default) — symphonia + cpal for audio, mpv for video, spawned
    only when needed.
  - **chromecast** — device discovery via mDNS, playback through the Default
    Media Receiver, with a **local queue** that auto-advances on `FINISHED`.
  - **upnp** — SSDP discovery of any UPnP AV MediaRenderer, playback via
    AVTransport (`SetAVTransportURI` / `Play` / `Pause` / `Stop` / `Seek`)
    and volume via RenderingControl. Same auto-advancing queue.
- **Playback modes** — shuffle, repeat-off/all/one, [ReplayGain](#replaygain)
  (track / album), [crossfade](#crossfade) between adjacent tracks
  (traditional cosine curves *or* additive DJ-mixed), and a
  10-band [equalizer](#equalizer) powered by the Rockbox DSP pipeline.
- **Queue persistence** — the audio queue, shuffle/repeat state, and the exact
  playhead within the current track survive restarts. Restore lands paused;
  `Space` picks up where you left off.
- **Real queue management** — enqueue, play next, jump between tracks, remove
  a single entry, clear the whole queue, and see the queue in its own tab
  with a `▶` marker on the actually-playing track.
- **Playlists** — browse, open, and play the playlists you've saved on the
  server.
- **Now Playing bar** with title, subtitle, elapsed / total time, a neon
  progress gauge, volume, and mode badges (shuffle ⇄, repeat ↻/↺,
  ReplayGain, crossfade ⋈/≈).
- **CLI shortcuts** for scripting: `fin play "kind of blue"`,
  `fin queue --chromecast "Living Room" "wednesday"`,
  `fin play --upnp "Kitchen Speaker" "solaris"`, `fin devices`.
- **All settings** are available as **CLI flags _or_ TOML keys** — one
  workflow scales from ad-hoc invocation to per-machine config.
- **Pure Rust TLS** (`rustls`) everywhere — no OpenSSL required.

## Install

Local audio needs no extra binaries — everything is baked into the `fin`
binary. **`mpv`** is only needed on `$PATH` when you actually play video
locally. Every install path below either bundles it or pulls it in as a
soft dependency.

### macOS / Linux — Homebrew

```bash
brew install tsirysndr/tap/fin
```

The formula pulls in `mpv` automatically.

### Debian / Ubuntu — `.deb`

Download the `.deb` for your architecture from the
[latest release](https://github.com/tsirysndr/fin/releases/latest) and:

```bash
# amd64
curl -LO https://github.com/tsirysndr/fin/releases/latest/download/fin_0.3.0_amd64.deb
sudo apt install ./fin_0.3.0_amd64.deb

# arm64 (Raspberry Pi 4/5, Apple-silicon VM, …)
curl -LO https://github.com/tsirysndr/fin/releases/latest/download/fin_0.3.0_arm64.deb
sudo apt install ./fin_0.3.0_arm64.deb
```

`apt` will pull in `libasound2` (ALSA runtime for cpal) and `mpv` automatically.

Or add the Gemfury apt repo once and `apt install` normally:

```bash
echo "deb [trusted=yes] https://apt.fury.io/tsirysndr/ /" \
  | sudo tee /etc/apt/sources.list.d/tsirysndr.list
sudo apt update && sudo apt install fin
```

### Fedora / RHEL / openSUSE — `.rpm`

```bash
sudo dnf install \
  https://github.com/tsirysndr/fin/releases/latest/download/fin-0.3.0-1.x86_64.rpm
```

Or via the Gemfury yum repo:

```bash
sudo tee /etc/yum.repos.d/tsirysndr.repo <<'EOF'
[tsirysndr]
name=tsirysndr
baseurl=https://yum.fury.io/tsirysndr/
enabled=1
gpgcheck=0
EOF
sudo dnf install fin
```

### Arch — from AUR / source

`mpv` from the official repos, then:

```bash
sudo pacman -S mpv alsa-lib
cargo install --git https://github.com/tsirysndr/fin --bin fin
```

### Prebuilt tarballs

For any other platform, grab the tarball for your arch from the
[releases page](https://github.com/tsirysndr/fin/releases/latest):

- `fin-<version>-linux-amd64.tar.gz`
- `fin-<version>-linux-aarch64.tar.gz`
- `fin-<version>-macos-amd64.tar.gz`
- `fin-<version>-macos-aarch64.tar.gz`

Each includes the `fin` binary + README + LICENSE. Install runtime deps
yourself:

```bash
# macOS
brew install mpv                  # video only; audio is in-process
# Debian / Ubuntu
sudo apt install libasound2 mpv
# Arch
sudo pacman -S alsa-lib mpv
```

### From source

```bash
git clone https://github.com/tsirysndr/fin
cd fin
cargo install --path crates/fin
```

Build-time on Linux needs `libasound2-dev` + `pkg-config` (cpal's ALSA
backend); on macOS the Core Audio SDK is already in the toolchain.

### Nix

A flake is provided — mpv is baked into the wrapper, so no extra install
step is needed:

```bash
# One-off run:
nix run github:tsirysndr/fin

# Install into your user profile:
nix profile install github:tsirysndr/fin

# Dev shell (rust toolchain + mpv + alsa-lib + clippy + rust-analyzer):
nix develop
```

## Getting started

```bash
# 1. Sign in
fin login https://media.example.com

# 2. Launch the TUI (default sub-command)
fin

# 3. Or drive it entirely from the shell
fin search "daft punk"
fin play  "kind of blue"
fin queue "wednesday season 1"
fin devices                          # list Chromecasts + UPnP renderers on your LAN
fin play --chromecast "Living Room" "solaris"
fin play --upnp       "Kitchen"    "solaris"
```

## Renderer selection

Three ways to choose a renderer — all equivalent:

| Shortcut flag                          | Long flag                 | Config key                     |
|----------------------------------------|---------------------------|--------------------------------|
| `--mpv`                                | `--renderer mpv`          | `renderer = "mpv"`             |
| `--chromecast "Living Room"`           | `--renderer chromecast`   | `renderer = "chromecast"`      |
| `--upnp "Kitchen Speaker"`             | `--renderer upnp`         | `renderer = "upnp"`            |
| _(none — falls back to local)_         |                           |                                |

The `--mpv` / `renderer = "mpv"` flag name is historical; it selects the
**local** renderer, which uses symphonia+cpal for audio and mpv for video.

When you pass `--chromecast NAME` or `--upnp NAME`, the renderer is switched
to that protocol automatically and the named device is preferred on connect.
If the name is not found on the network, fin picks the first device discovered.

## All settings

Every setting exists as both a CLI flag and a TOML key. Flags win.

| CLI flag              | Env var             | TOML key                  | Default          |
|-----------------------|---------------------|---------------------------|------------------|
| `--server URL`        | `FIN_SERVER`        | `servers[].url`           | _(none)_         |
| `--server-name NAME`  | `FIN_SERVER_NAME`   | `current_server`          | _(latest login)_ |
| `--token TOKEN`       | `FIN_TOKEN`         | `servers[].access_token`  | _(from login)_   |
| `--user-id ID`        | `FIN_USER_ID`       | `servers[].user_id`       | _(from login)_   |
| `--user-name NAME`    |                     | `servers[].user_name`     | _(from login)_   |
| `--device-id ID`      | `FIN_DEVICE_ID`     | `servers[].device_id`     | random UUID      |
| `--renderer <mpv/chromecast/upnp>` | `FIN_RENDERER` | `renderer`      | `mpv`            |
| `--mpv`               |                     | `renderer = "mpv"`        |                  |
| `--chromecast [NAME]` | `FIN_CHROMECAST`    | `last_chromecast`         |                  |
| `--upnp [NAME]`       | `FIN_UPNP`          | `last_upnp`               |                  |
| `-v`, `-vv`           |                     | _(log level)_             | `warn`           |

Audio-side playback settings live under TOML sub-tables and are toggled from
the TUI (see [Playback modes & effects](#playback-modes--effects)):

| TOML                              | Default        | Notes                                                    |
|-----------------------------------|----------------|----------------------------------------------------------|
| `replaygain.mode`                 | `off`          | `off` / `track` / `album`                                |
| `replaygain.preamp_db`            | `0.0`          | additive in dB before clip guard                         |
| `replaygain.prevent_clip`         | `true`         | caps gain so `linear * peak <= 1.0`                      |
| `crossfade.mode`                  | `off`          | `off` / `crossfade` / `mixed`                            |
| `crossfade.duration_secs`         | `5.0`          | overlap window in seconds                                |
| `eq_enabled`                      | `false`        | toggle the Rockbox 10-band EQ pipeline                   |
| `[[eq_band_settings]]`            | ISO octave     | 10 bands (see [Equalizer](#equalizer)); Rockbox-compatible |
| `bass`                            | `0`            | bass shelf gain in whole dB (−24…+24)                    |
| `treble`                          | `0`            | treble shelf gain in whole dB (−24…+24)                  |
| `bass_cutoff`                     | `0`            | bass shelf cutoff in Hz (`0` = Rockbox default 200)      |
| `treble_cutoff`                   | `0`            | treble shelf cutoff in Hz (`0` = Rockbox default 3500)   |

Find the on-disk config with `fin config --path`; print it with
`fin config --show`.

## Multiple servers

fin authenticates against as many Jellyfin servers as you like and keeps
their tokens side-by-side in one config file:

```bash
fin login https://home.example.com    --name home
fin login https://work.example.com    --name work
fin login https://mom.dyndns.example  --name mom

fin server                            # list all servers (▍ marks the current one)
fin server switch work                # make `work` the active server
fin server rm mom                     # remove one
fin server rename home casa           # rename `home` → `casa`

# One-off — hit `work` without changing the current pointer:
fin --server-name work search "spirited away"
fin --server-name work play  "spirited away"
```

Inside the TUI, the Settings screen shows every saved server; **Enter** on
one switches to it. **`t`** anywhere in the TUI cycles to the next server
without leaving the current screen.

## Sub-commands

```
fin                         # launch the TUI (default)
fin login <url> [--name N]  # sign in and save credentials for server `N`
fin logout [--name N]       # remove server `N` (defaults to the current one)
fin server                  # list saved servers
fin server switch <name>    # change the active server
fin server rm <name>        # remove one
fin server rename <a> <b>   # rename
fin search <query>          # print matches from the active library
fin play <query>            # search + play the top hit
fin queue <query>           # search + append to the current queue
fin devices                 # list Chromecasts + UPnP MediaRenderers on the local network
fin playlists               # list playlists
fin playlists --list <id>   # dump items of a playlist
fin config --show|--path    # inspect config
```

## Keybindings (TUI)

Tab order — the default screen is **Music**:

  `1` Music • `2` Videos • `3` Playlists • `4` Queue • `5` Search • `6` Devices • `7` Settings

| Key                          | Action                              |
|------------------------------|-------------------------------------|
| `?`                          | show / hide the full keyboard-shortcuts help modal |
| `Tab` / `Shift+Tab`          | next / prev screen                  |
| `1`…`7`                      | jump to Music / Videos / Playlists / Queue / Search / Devices / Settings |
| `/`                          | jump to Search & focus input        |
| `↑` `↓` / `k` `j`            | move selection                      |
| `PgUp` / `PgDown`            | jump 10 rows                        |
| `Enter`                      | **drill in** on a container (album, series, playlist) — plays a leaf (track, episode, movie); on Queue → **jump** the playhead to the selected entry; on Devices → connect to the selected Chromecast / UPnP renderer; on Settings → switch server |
| `x`                          | play the highlighted container as one queue **without** drilling in (album → all tracks, playlist → all items) |
| `a`                          | enqueue the highlighted item        |
| `n`                          | play the highlighted item **next**  |
| `z`                          | toggle shuffle                      |
| `Shift+R`                    | cycle repeat mode (off → all → one) |
| `g`                          | cycle ReplayGain (off → track → album) |
| `f` / `Shift+F`              | cycle crossfade mode / cycle crossfade duration (3, 5, 8, 12 s) |
| `Shift+E`                    | toggle 10-band Rockbox EQ           |
| `[` / `]`                    | (Settings) select previous / next EQ band |
| `Shift+↑` / `Shift+↓`        | (Settings) nudge the selected EQ band's gain by ±1 dB |
| `b` / `Shift+B`              | bass shelf −1 dB / +1 dB            |
| `y` / `Shift+Y`              | treble shelf −1 dB / +1 dB          |
| `Space` or `p`               | pause / resume                      |
| `s`                          | stop                                |
| `<` / `>` or `h` / `l`       | previous / next track               |
| `+` / `-`                    | volume up / down                    |
| `m`                          | switch to local renderer            |
| `t`                          | cycle to the next saved Jellyfin server |
| `d`                          | (Queue screen) remove the highlighted entry |
| `Shift+C`                    | (Queue screen) clear the entire queue |
| `Esc`                        | pop the current drill-in (back to the parent list) |
| `r`                          | refresh the current screen          |
| `Esc`                        | leave the search input / close open playlist |
| `q` / `Ctrl-C`               | quit                                |

## Playback modes & effects

All of these run only on the local renderer (audio path). Chromecast and
UPnP receivers each do their own thing; toggles no-op on those renderers.
Settings persist to `config.toml` and are mirrored back on next launch.

### Shuffle

`z` toggles shuffle. Enabling it reshuffles every item **after** the
currently-playing track using a Fisher-Yates permutation — the playing track
stays put so the audio doesn't jump.

### Repeat

`Shift+R` cycles the repeat mode off → all → one → off. `all` wraps in both
directions (Prev at row 0 goes to the last item); `one` sticks on the
current track until the mode changes.

### ReplayGain

`g` cycles Off → Track → Album → Off. Reads `REPLAYGAIN_TRACK_GAIN`,
`REPLAYGAIN_ALBUM_GAIN`, and the matching peak tags off decoded tracks
(Vorbis-comment or ID3v2), computes a linear gain multiplier
(`10^((gain + preamp) / 20)`), and folds it into the sample push loop. If
the requested scope's tag is missing, the other scope is used as a fallback.
Clip prevention is on by default — it caps the multiplier so peaks stay ≤ 1.0.

### Crossfade

`f` cycles Off → **Crossfade** → **Mixed** → Off.

- **Crossfade** — cosine/sine curves (out² + in² = 1); perceived loudness
  stays constant across the overlap.
- **Mixed** — no curves; both tracks play at full volume during the overlap
  and sum additively (louder DJ-style mix).

`Shift+F` cycles the duration through 3, 5, 8, 12 s, preserving the current
mode. Duration is also editable directly in `config.toml`.

Under the hood, `fin` runs a second decoder + cpal output stream during the
overlap and the OS mixer sums them. The overlap kicks in both on natural
end-of-track transitions AND when you Play a new album or jump to a new
queue entry — so switching tracks manually still fades cleanly.

### Equalizer

`fin` links the Rockbox DSP pipeline
([`rockbox-dsp`](https://crates.io/crates/rockbox-dsp)) for a fixed-point
10-band EQ with high-quality biquad filters — band 0 is a low shelf, band 9
a high shelf, bands 1–8 are peaking filters. On the Settings screen you'll
see 10 vertical sliders with `dB` labels above and cutoff frequency labels
below. The controls:

| Key            | Action                                        |
|----------------|-----------------------------------------------|
| `E`            | toggle EQ on / off                            |
| `[` / `]`      | move the highlighted band left / right        |
| `Shift+↑` / `↓`| bump the highlighted band's gain by ±1 dB     |

Adjustments persist to `config.toml` immediately. Fresh installs get the
ISO-octave flat preset (32 Hz, 63, 125, 250, 500, 1 kHz, 2, 4, 8, 16 kHz,
Q 7.0, 0 dB across the board) so the DSP is a bit-exact bypass until you
start tweaking. Values in `[[eq_band_settings]]` use Rockbox tenths — `q =
70` means Q 7.0, `gain = -125` means −12.5 dB — so a Rockbox preset drops
in unchanged.

**License note:** enabling EQ links the Rockbox DSP C sources (GPL-2.0-or-later),
which makes the resulting `fin` binary GPL. The rest of `fin` remains MPL-2.0
in source form.

Behavioral notes:

- **Only local playback runs through EQ.** Chromecast and UPnP receivers each
  do their own DSP; the toggle is a no-op there.
- During a crossfade the outgoing *and* incoming tracks both route through
  the same Rockbox DSP config. The biquad delay lines get briefly stirred
  when the two tracks alternate through the pipeline — the audible transient
  is well under a millisecond at 48 kHz.

### Bass & treble

The Rockbox tone-control stage runs in the same DSP pipeline as the EQ —
shelving filters at fixed cutoffs (default 200 Hz bass, 3500 Hz treble).
Adjust with `b` / `Shift+B` for bass and `y` / `Shift+Y` for treble; every
press is a 1 dB step in the ±24 dB range and is persisted to `config.toml`
immediately. The Settings screen shows the current values, and the player
bar shows a compact `B+3/T-2` badge whenever either is non-zero.

Custom shelf cutoffs can be set in `config.toml` via `bass_cutoff` /
`treble_cutoff` (Hz); `0` means the Rockbox defaults. The keys and this
stage go through the same singleton pipeline as EQ, so the licensing note
above applies.

## Queue persistence

The audio queue, shuffle/repeat state, and the exact playhead within the
currently-playing track are written to `cache_dir/queue.json` on every
mutation and every ~3 s while playing. Writes are debounced and atomic
(rename-in-place), so a crash mid-write can't leave a truncated file.

On startup, `fin` reads the snapshot and restores the queue paused at the
saved position. `Space` (or `p`) resumes from exactly where you left off.
Video items in a saved queue are filtered out silently — the persistence
path lives on the audio side; the mpv-driven video path is transient.

Find the file with `fin config --path` (adjacent to the config dir).

## Remote-renderer queue

For both Chromecast and UPnP, `fin` maintains the queue **on the client**,
polls the device for its current transport state, and loads the next item
automatically the moment the current one finishes (Chromecast:
`IDLE / FINISHED`; UPnP: `STOPPED` after having been `PLAYING`). That means:

- `a` (queue) and `n` (play-next) do the right thing while something is
  already streaming.
- Skipping (`>` / `<`) triggers a `load` for the next queue item
  immediately — no waiting for the current one to finish.
- Stopping clears the local queue and stops the receiver's playback.

UPnP renderers without a `RenderingControl` service (rare, but it happens)
still work for transport — volume changes are just no-ops on the device.

## Streams & transcoding

- Local **audio** uses the original stream — `symphonia` decodes MP3 / FLAC
  / AAC / ALAC / Opus / Vorbis / WAV / … directly in-process, resampled to
  the output device's rate. No transcoding round-trip; no mpv on the audio
  path.
- Local **video** shells out to **mpv** with `Static=true` — the fastest
  direct-stream path, mpv handles any container Jellyfin can hand it.
- **Chromecast** playback defaults to Jellyfin's HLS output (`main.m3u8`),
  because the Default Media Receiver's codec matrix is much narrower than
  mpv's. Jellyfin will transcode when it needs to.
- **UPnP** playback uses the direct stream by default — most UPnP
  MediaRenderers speak MP3 / AAC / FLAC natively and the direct path avoids
  the transcode. Fall back to HLS with `--hls` if your renderer needs it.
- Force one or the other from the CLI with `--hls` on `play`/`queue`.

## Development

```bash
cargo check --workspace
cargo build --release -p fin
./target/release/fin --help
cargo test --workspace
```

The workspace layout:

```
fin/
├── crates/
│   ├── fin/               # binary — clap CLI + startup
│   ├── fin-config/        # TOML config file, credentials, mode enums
│   ├── fin-jellyfin/      # Jellyfin HTTP API client
│   ├── fin-player/        # Renderer trait, queue, symphonia audio path,
│   │                      # mpv video, Chromecast + UPnP, replaygain,
│   │                      # crossfade, queue persistence
│   └── fin-tui/           # Ratatui neon TUI
└── Cargo.toml             # workspace + shared deps (rustls only, no openssl)
```

## License

`fin` is released under the [MPL-2.0](LICENSE).
