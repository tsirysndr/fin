{
  description = "fin - a neon-electric Jellyfin TUI client for mpv & Chromecast";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    crane = {
      url = "github:ipetkov/crane";
      inputs.nixpkgs.follows = "nixpkgs";
    };

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
          version = "0.1.0";
          strictDeps = true;

          # No native TLS or system libs needed — pure Rust deps.
          nativeBuildInputs = [
            pkgs.pkg-config
          ];

          buildInputs = lib.optionals pkgs.stdenv.isDarwin [
            pkgs.libiconv
            pkgs.darwin.apple_sdk.frameworks.SystemConfiguration
            pkgs.darwin.apple_sdk.frameworks.Security
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

          # Everything you need to `cargo run` and actually play back media.
          nativeBuildInputs = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            rust-analyzer
            mpv
            pkg-config
          ] ++ lib.optionals pkgs.stdenv.isDarwin [
            libiconv
            darwin.apple_sdk.frameworks.SystemConfiguration
            darwin.apple_sdk.frameworks.Security
          ];

          shellHook = ''
            echo "⚡ fin dev shell — mpv $(mpv --version | head -n1 | cut -d' ' -f2) ready"
          '';
        };
      });
}
