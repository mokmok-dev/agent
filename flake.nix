{
  description = "";

  inputs = {
    nixpkgs.url = "https://flakehub.com/f/NixOS/nixpkgs/0";
    flake-parts.url = "https://flakehub.com/f/hercules-ci/flake-parts/0";
    flake-parts.inputs.nixpkgs-lib.follows = "nixpkgs";
    git-hooks.url = "https://flakehub.com/f/cachix/git-hooks.nix/0";
    git-hooks.inputs.nixpkgs.follows = "nixpkgs";
    treefmt-nix.url = "https://flakehub.com/f/numtide/treefmt-nix/0";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
    rust-overlay.url = "https://flakehub.com/f/oxalica/rust-overlay/0";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    crane.url = "https://flakehub.com/f/ipetkov/crane/0";
  };

  outputs =
    {
      nixpkgs,
      flake-parts,
      git-hooks,
      treefmt-nix,
      rust-overlay,
      crane,
      ...
    }@inputs:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [
        git-hooks.flakeModule
        treefmt-nix.flakeModule
      ];

      perSystem =
        {
          config,
          pkgs,
          system,
          ...
        }:
        let
          rustToolchain = pkgs.rust-bin.stable.latest.default;
          src = pkgs.lib.cleanSourceWith { src = ./.; };
          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;
          commonArgs = {
            inherit src;
            strictDeps = true;
          };
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
        in
        {
          _module.args.pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };

          checks = {
            clippy = craneLib.cargoClippy (
              commonArgs
              // {
                inherit
                  src
                  cargoArtifacts
                  ;
                cargoClippyExtraArgs = "--all-targets --all-features -- --deny warnings";
              }
            );
            test = craneLib.cargoTest (
              commonArgs
              // {
                inherit
                  src
                  cargoArtifacts
                  ;
                # `reqwest` builds a rustls platform verifier for every client,
                # including the plain-HTTP loopback endpoint `agentd-telemetry`
                # exports to, and panics when the platform exposes no roots. The
                # build sandbox has no system CA bundle, so hand it one.
                preCheck = "export SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
              }
            );
          };

          packages.default = craneLib.buildPackage (
            commonArgs
            // {
              inherit
                src
                cargoArtifacts
                ;
              # The daemon spawns `agentd-sandbox-helper` and
              # `agentd-egress-forward`; the executor resolves them as siblings
              # of the running binary, so all three must land in the same
              # `bin/`. `agentd up` likewise runs `agentd-agent` as its confined
              # session, resolved as a sibling, so the node binaries ship too.
              # `agentd-client` is the operator's out-of-process relay to a TUI
              # or browser, so it ships beside them.
              # `agentd/sandbox` is what lets `agentd` supervise at all.
              cargoBuildExtraArgs = "--bins --package agentd --package agentd-client --package agentd-sandbox --package agentd-node --features agentd/sandbox";
              # The package is named `agent` (from `workspace.metadata.crane`)
              # while its entry binary is `agentd`, so `nix run` must be told
              # which program to execute: without this it assumes `bin/agent`.
              meta.mainProgram = "agentd";
              doCheck = false;
            }
          );

          devShells.default = pkgs.mkShell {
            inputsFrom = [ config.pre-commit.devShell ];

            packages =
              with pkgs;
              [
                skills
                rustToolchain
                sccache
                # The OTLP collector the daemon exports spans to. `otelcol` with
                # `nix/otelcol.yaml` prints spans to its stdout; point the
                # daemon's `OTEL_EXPORTER_OTLP_ENDPOINT` at it to see a trace.
                opentelemetry-collector
              ]
              ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
                # The confinement backend. On Linux a policy that needs a
                # private network namespace fails closed without bubblewrap
                # (`agentd-sandbox/src/linux.rs`), so the dev shell ships it
                # rather than leaving the strongest confinement to the host.
                bubblewrap
                mold
              ];

            shellHook = ''
              export RUSTC_WRAPPER="${pkgs.sccache}/bin/sccache"
            ''
            + pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
              export RUSTFLAGS="''${RUSTFLAGS:+$RUSTFLAGS }-C link-arg=-fuse-ld=mold"
            '';
          };

          pre-commit.settings = {
            hooks = {
              actionlint.enable = true;
              deadnix.enable = true;
              statix.enable = true;
            };
          };

          treefmt = {
            projectRootFile = "flake.nix";
            programs = {
              nixfmt.enable = true;
              rustfmt.enable = true;
              rustfmt.package = rustToolchain;
              taplo.enable = true;
              yamlfmt.enable = true;
            };
          };
        };

      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
    };
}
