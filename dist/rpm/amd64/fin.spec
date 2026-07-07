Name:           fin
Version:        0.4.0
Release:        1%{?dist}
Summary:        A neon-electric Jellyfin TUI client for mpv & Chromecast

License:        MPL-2.0
URL:            https://github.com/tsirysndr/fin

BuildArch:      x86_64

Requires: glibc, alsa-lib, mpv

%description
fin is a Rust TUI + one-shot CLI that talks to your Jellyfin server, searches
your library, manages playlists, and pushes streams to either your local mpv
window or any Chromecast on your network. Chromecast playback is fully queued
— enqueue, play-next, skip, resume, all from the terminal.

%prep
# Nothing to prep — the binary is prebuilt.

%build
# Nothing to build — the binary is prebuilt.

%install
mkdir -p %{buildroot}/usr/local/bin
cp -r %{_sourcedir}/amd64/usr %{buildroot}/

%files
/usr/local/bin/fin

%post
if [ "$1" -eq 1 ]; then
    echo "fin: installed. Sign in with:  fin login <your-jellyfin-url>"
fi
