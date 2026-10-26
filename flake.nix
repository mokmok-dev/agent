{
  inputs = {
    nixpkgs.url = "https://flakehub.com/f/NixOS/nixpkgs/0.2605.1016691";
    flake-parts.url = "https://flakehub.com/f/hercules-ci/flake-parts/0.1.497";
    git-hooks.url = "https://flakehub.com/f/cachix/git-hooks.nix/0.1.1262";
    treefmt-nix.url = "https://flakehub.com/f/numtide/treefmt-nix/0.1.684";
    rust-overlay.url = "https://flakehub.com/f/oxalica/rust-overlay/0.1.2346";
    crane.url = "https://flakehub.com/f/ipetkov/crane/0.24.0";
  };

  outputs =
    {
      self,
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

      flake = {
        nixosModules.default = import ./nix/module.nix { inherit self; };
      };

      perSystem =
        {
          config,
          lib,
          pkgs,
          system,
          ...
        }:
        let
          rustToolchain = pkgs.rust-bin.stable.latest.default;
          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;
          src = pkgs.lib.cleanSourceWith { src = ./.; };
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
            # The module is checked by evaluating a configuration and reading the
            # unit systemd would be given. A virtual machine test would be stronger
            # and cannot run in this sandbox, so this is the part that can be kept
            # honest here: the options typecheck, the unit carries the settings the
            # design asks for, and the daemon it points at builds.
            module-eval =
              let
                configured = inputs.nixpkgs.lib.nixosSystem {
                  system = "x86_64-linux";
                  modules = [
                    self.nixosModules.default
                    {
                      services.mokmok-agent = {
                        apiKeyFile = "/dev/null";
                        baseUrl = "https://example.test/v1";
                        directive = "be careful";
                        enable = true;
                        model = "test-model";
                        tools = [
                          {
                            argv = [
                              "/run/current-system/sw/bin/true"
                            ];
                            name = "shell";
                            risk = "read";
                            schema = "{}";
                          }
                        ];
                      };
                    }
                  ];
                };
                unit = configured.config.systemd.units."mokmok-agent.service".text;
                settings =
                  let
                    line = builtins.head (
                      builtins.filter (l: builtins.match ".*--config (/nix/store/[^ ]+).*" l != null) (
                        inputs.nixpkgs.lib.splitString "\n" unit
                      )
                    );
                  in
                  builtins.head (builtins.match ".*--config (/nix/store/[^ ]+).*" line);
              in
              pkgs.runCommand "mokmok-agent-module-eval" { } ''
                unit=${pkgs.writeText "unit" unit}
                # The module writes the configuration the daemon reads, and the
                # daemon refuses a tool that does not say what it may touch. A
                # module that omits the field would be a service that cannot start.
                grep -q "argv" ${settings} || {
                  echo "the module did not write a tool's argv" >&2
                  exit 1
                }
                grep -q "risk = \"read\"" ${settings} || {
                  echo "the module did not write a tool's risk" >&2
                  exit 1
                }
                grep -q "Type=notify" $unit
                grep -q "WatchdogSec=30s" $unit
                # The rate limit belongs to [Unit]; in [Service] systemd ignores it
                # and logs that it did, so a grep over the whole file would pass
                # while the setting did nothing.
                ${pkgs.gawk}/bin/awk '/^\[Unit\]/{unit=1} /^\[Service\]/{unit=0} unit' $unit \
                  | grep -q "^StartLimitIntervalSec=0$"
                if ${pkgs.gawk}/bin/awk '/^\[Service\]/{service=1} /^\[Install\]/{service=0} service' $unit \
                  | grep -q "StartLimitIntervalSec"; then
                  echo "StartLimitIntervalSec is in [Service], where systemd ignores it" >&2
                  exit 1
                fi
                grep -q "StateDirectory=mokmok-agent" $unit
                grep -q "LoadCredential=api_key:/dev/null" $unit
                grep -q "NoNewPrivileges=true" $unit
                grep -q "ProtectSystem=strict" $unit
                grep -q "agentd --config /nix/store/" $unit
                touch $out
              '';

            test = craneLib.cargoNextest (
              commonArgs
              // {
                inherit
                  src
                  cargoArtifacts
                  ;
                cargoNextestExtraArgs = "--workspace --all-features";
              }
            );
          };

          devShells.default = pkgs.mkShellNoCC {
            inputsFrom = [ config.pre-commit.devShell ];

            packages =
              (with pkgs; [
                cargo-mutants
                cargo-nextest
                rustToolchain
                sccache
              ])
              ++ lib.optionals pkgs.stdenv.hostPlatform.isLinux (
                with pkgs;
                [
                  clang
                  mold
                ]
              );

            shellHook = ''
              export RUSTC_WRAPPER="${pkgs.sccache}/bin/sccache"
              ${lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
                export RUSTFLAGS="-C link-arg=-fuse-ld=mold"
                export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="clang"
                export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="clang"
              ''}
            '';
          };

          packages = {
            agentd = craneLib.buildPackage (
              commonArgs
              // {
                inherit cargoArtifacts;
                cargoBuildFlags = [
                  "-p"
                  "agentd"
                ];
                pname = "agentd";
                version = "0.0.0";

                # One binary. The dependency cache that comes in with
                # cargoArtifacts leaves every workspace binary in target/release,
                # so the default install hook would ship the repo's own task
                # runner alongside the daemon.
                installPhase = ''
                  install -Dm755 target/release/agentd $out/bin/agentd
                '';
              }
            );
            default = config.packages.agentd;
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
            };
          };
        };

      systems = [
        "x86_64-linux"
      ];
    };
}
