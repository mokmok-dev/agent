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
          # `target/` and `mutants.out*/` are large and regenerated, so they must
          # not enter the Nix store. crane needs the rest of the tree (flake.nix,
          # docs) for the checks that read it.
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter =
              path: _type:
              let
                base = baseNameOf path;
              in
              !(builtins.elem base [
                "target"
                ".git"
                ".direnv"
                "mutants.out"
                "mutants.out.old"
              ])
              && !(pkgs.lib.hasPrefix "result" base);
          };
          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;
          commonArgs = {
            inherit src;
            strictDeps = true;
          };
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
          # Kani is a separate tool: it ships its own nightly rustc, so it is
          # installed as a standalone Nix package rather than through the
          # workspace toolchain.
          kaniVerifier = import ./nix/kani-package.nix { inherit pkgs system; };
          kaniArgs = commonArgs // {
            cargoArtifacts = null;
            nativeBuildInputs = [ kaniVerifier ];
            doInstallCargoArtifacts = false;
            installPhaseCommand = "mkdir -p $out";
          };
          # A harness proves only what it asserts, so one that stopped asserting
          # the property still reports SUCCESSFUL. Each entry injects one
          # mutation that must break the property its harness states, and the
          # check requires that harness to report a failure. `--replace-fail`
          # and `--harness` make a stale entry fail instead of quietly passing.
          harnessMutations = [
            {
              name = "wal-recovery-keeps-a-full-record";
              # A record that exactly fills the remaining bytes is committed, not
              # a torn trailing write; `>` must not become `>=`.
              file = "src/wal/scan.rs";
              find = "if len > bytes.len() - position {";
              replace = "if len >= bytes.len() - position {";
              harness = "a_single_record_recovers_cleanly";
            }
          ];
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
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--all-targets --all-features -- --deny warnings";
              }
            );
            test = craneLib.cargoTest (
              commonArgs
              // {
                inherit cargoArtifacts;
              }
            );
            kani = craneLib.mkCargoDerivation (
              kaniArgs
              // {
                pname = "agent-kani";
                buildPhaseCargoCommand = "cargo-kani --lib";
              }
            );
          }
          // builtins.listToAttrs (
            map (
              entry:
              let
                name = "kani-harness-mutation-${entry.name}";
              in
              {
                inherit name;
                value = craneLib.mkCargoDerivation (
                  kaniArgs
                  // {
                    pname = name;
                    buildPhaseCargoCommand = ''
                      substituteInPlace ${entry.file} \
                        --replace-fail ${pkgs.lib.escapeShellArg entry.find} ${pkgs.lib.escapeShellArg entry.replace}
                      output="$(cargo-kani --lib --harness ${entry.harness} 2>&1 || true)"
                      if ! grep -q 'VERIFICATION:- FAILED' <<<"$output"; then
                        printf '%s\n' "$output" >&2
                        echo "The ${entry.harness} harness did not fail on the injected mutation." >&2
                        echo "The harness no longer checks that property, the mutation no longer compiles, or it is now equivalent." >&2
                        exit 1
                      fi
                      grep -m1 'VERIFICATION:- FAILED' <<<"$output"
                      echo "verified: ${entry.harness} fails with the injected mutation"
                    '';
                  }
                );
              }
            ) harnessMutations
          );

          devShells.default = pkgs.mkShellNoCC {
            inputsFrom = [ config.pre-commit.devShell ];

            packages =
              with pkgs;
              [
                rustToolchain
                sccache
                skills
                kaniVerifier
                cargo-mutants
              ]
              ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
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
