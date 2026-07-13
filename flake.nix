{
  description = "fin - a neon-electric Jellyfin TUI client for mpv & Chromecast";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    # Current crane doesn't expose a `nixpkgs` input, so we don't follow it.
    crane.url = "github:ipetkov/crane";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.rust-analyzer-src.follows = "";
    };

    flake-utils.url = "github:numtide/flake-utils";

    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };
  };

  outputs = { self, nixpkgs, crane, fenix, flake-utils, advisory-db, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
        };

        inherit (pkgs) lib;

        craneLib = crane.mkLib pkgs;

        src = craneLib.cleanCargoSource ./.;

        # fin uses rustls end-to-end, so no openssl.
        # mpv is a *runtime* requirement (we spawn it for local playback and
        # the CLI does a preflight `mpv --version` at startup). We fold it
        # into PATH via a makeWrapper post-fixup below.
        commonArgs = {
          inherit src;

          pname = "fin";
          version = "0.5.0";
          strictDeps = true;

          # rockbox-playback pulls in rockbox-codecs + rockbox-dsp, whose
          # build scripts compile Rockbox's C codec/DSP sources with the `cc`
          # crate — so a C compiler must be on PATH. `stdenv.cc` is the
          # toolchain for this platform (clang on Darwin, gcc on Linux).
          # TLS is pure-Rust (rustls), so still no openssl.
          # Modern nixpkgs (post-25.05) auto-links the Darwin SDK, so no
          # framework references here — `darwin.apple_sdk_11_0` was removed
          # as a legacy compatibility stub.
          nativeBuildInputs = [
            pkgs.pkg-config
            pkgs.stdenv.cc
          ] ++ lib.optionals pkgs.stdenv.isDarwin [
            # coreaudio-sys generates its CoreAudio bindings with bindgen at
            # build time; bindgenHook provides libclang (LIBCLANG_PATH) and
            # points clang at the Nix Apple SDK headers.
            pkgs.rustPlatform.bindgenHook
          ];

          buildInputs = lib.optionals pkgs.stdenv.isDarwin [
            pkgs.libiconv
          ] ++ lib.optionals pkgs.stdenv.isLinux [
            # cpal links against ALSA on Linux for the local audio output path.
            # MPRIS needs no dbus dev lib: zbus is a pure-Rust D-Bus
            # implementation that speaks to the session bus socket directly.
            pkgs.alsa-lib
          ];

          # Workspace has one bin target — build just that.
          cargoExtraArgs = "--locked --bin fin";
        };

        craneLibLLvmTools = craneLib.overrideToolchain
          (fenix.packages.${system}.complete.withComponents [
            "cargo"
            "llvm-tools"
            "rustc"
          ]);

        # Cache the dependency graph separately from the crate source.
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        finUnwrapped = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          doCheck = false;
        });

        # Wrap the binary so `mpv` is always discoverable at runtime, even
        # when installed via `nix profile install`.
        fin = pkgs.symlinkJoin {
          name = "fin-${finUnwrapped.version}";
          paths = [ finUnwrapped ];
          nativeBuildInputs = [ pkgs.makeWrapper ];
          postBuild = ''
            wrapProgram $out/bin/fin \
              --prefix PATH : ${lib.makeBinPath [ pkgs.mpv ]}
          '';
          meta = {
            description = "A neon-electric Jellyfin TUI client for mpv & Chromecast";
            homepage = "https://github.com/tsirysndr/fin";
            license = lib.licenses.mpl20;
            mainProgram = "fin";
            platforms = lib.platforms.unix;
          };
        };

      in
      {
        checks = {
          inherit fin;

          fin-clippy = craneLib.cargoClippy (commonArgs // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          });

          fin-doc = craneLib.cargoDoc (commonArgs // {
            inherit cargoArtifacts;
          });

          fin-fmt = craneLib.cargoFmt {
            inherit src;
          };

          fin-audit = craneLib.cargoAudit {
            inherit src advisory-db;
          };

          fin-nextest = craneLib.cargoNextest (commonArgs // {
            inherit cargoArtifacts;
            partitions = 1;
            partitionType = "count";
          });
        } // lib.optionalAttrs (system == "x86_64-linux") {
          fin-coverage = craneLib.cargoTarpaulin (commonArgs // {
            inherit cargoArtifacts;
          });
        };

        packages = {
          default = fin;
          fin = fin;
          fin-unwrapped = finUnwrapped;

          fin-llvm-coverage = craneLibLLvmTools.cargoLlvmCov (commonArgs // {
            inherit cargoArtifacts;
          });
        };

        apps.default = flake-utils.lib.mkApp {
          drv = fin;
          name = "fin";
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = builtins.attrValues self.checks.${system};

          # Build-time tools. pkg-config is required so cpal's build.rs can
          # resolve libasound on Linux; stdenv.cc supplies the C compiler the
          # rockbox-codecs / rockbox-dsp build scripts need to compile
          # Rockbox's C sources.
          nativeBuildInputs = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            rust-analyzer
            mpv
            pkg-config
            stdenv.cc
          ];

          # Link-time libraries. Position matters: pkg-config only picks up
          # `.pc` files from `buildInputs`, so alsa-lib MUST live here (not
          # in nativeBuildInputs) for the cpal → ALSA link to resolve.
          buildInputs = with pkgs; lib.optionals stdenv.isDarwin [
            libiconv
          ] ++ lib.optionals stdenv.isLinux [
            alsa-lib
          ];

          shellHook = ''
            echo "⚡ fin dev shell — mpv $(mpv --version | head -n1 | cut -d' ' -f2) ready"
          '';
        };
      });
}
