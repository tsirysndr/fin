# @tsiry/fin

[![npm](https://img.shields.io/npm/v/@tsiry/fin.svg)](https://www.npmjs.com/package/@tsiry/fin)

A fast terminal music player and Subsonic/Jellyfin client written in Rust.

This package distributes the prebuilt `fin` binary via npm. On install it
downloads the release build matching your platform from
[GitHub Releases](https://github.com/tsirysndr/fin/releases) and verifies its
SHA-256 checksum.

## Run without installing

```sh
npx @tsiry/fin
```

## Install globally

```sh
npm install -g @tsiry/fin
# or
bun add -g @tsiry/fin

fin --help
```

## Supported platforms

| OS      | x64 | arm64 |
| ------- | :-: | :---: |
| macOS   |  ✅ |  ✅   |
| Linux   |  ✅ |  ✅   |
| FreeBSD |  ✅ |  ✅   |
| NetBSD  |  ✅ |  ✅   |
| OpenBSD |  ✅ |  —    |

On any other platform, [build from source](https://github.com/tsirysndr/fin)
with `cargo install --git https://github.com/tsirysndr/fin --bin fin`.

## Notes

- Local audio is baked into the binary. **`mpv`** is only required on `$PATH`
  when you play video locally — install it via your OS package manager.
- Set `FIN_SKIP_DOWNLOAD=1` to skip the download during install (the binary is
  then fetched on first run).
- Extraction uses the system `tar`, which is present on all supported targets.

Full documentation: <https://github.com/tsirysndr/fin>

## License

MPL-2.0
