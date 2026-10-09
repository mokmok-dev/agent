{
  description = "";

  inputs = {
    nixpkgs.url = "https://flakehub.com/f/NixOS/nixpkgs/0.1";
    flake-parts.url = "https://flakehub.com/f/hercules-ci/flake-parts/0";
    flake-parts.inputs.nixpkgs-lib.follows = "nixpkgs";
    git-hooks.url = "https://flakehub.com/f/cachix/git-hooks.nix/0";
    git-hooks.inputs.nixpkgs.follows = "nixpkgs";
    treefmt-nix.url = "https://flakehub.com/f/numtide/treefmt-nix/0";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      nixpkgs,
      flake-parts,
      git-hooks,
      treefmt-nix,
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
          # A prefetched pnpm store holds the native packages of the platform that
          # fetched it, so each system needs the hash its own fetch produces. With
          # one shared hash, the second system installs the first system's store
          # and its test run fails to find a native binding. Regenerate a value by
          # setting it to "" and copying the `got:` hash from the build log.
          pnpmDepsHash = {
            # apps/sandbox-broker adds @anthropic-ai/sandbox-runtime to the
            # lockfile, so every system's store dump changes. Only the
            # x86_64-linux value is filled in here; the two aarch64 values come
            # from a CI run on those machines, which reports them in the
            # mismatch error.
            aarch64-darwin = pkgs.lib.fakeHash;
            aarch64-linux = pkgs.lib.fakeHash;
            x86_64-linux = "sha256-keMnMeHRgoelUE46G2OegB8icOuNsFUB8WJ+I87/Hy8=";
          };

          # Every check runs one script from the repo's package.json against an
          # unpacked source tree, so they share one derivation body.
          mkCheck =
            name:
            pkgs.stdenvNoCC.mkDerivation (finalAttrs: {
              inherit name;
              src = ./.;

              pnpmDeps = pkgs.fetchPnpmDeps {
                pname = name;
                inherit (finalAttrs) src;
                pnpm = pkgs.pnpm_12;
                fetcherVersion = 4;
                hash = pnpmDepsHash.${system};
              };

              nativeBuildInputs = [
                pkgs.nodejs_26
                pkgs.pnpm_12
                pkgs.pnpmConfigHook
                # stryker kills its test runner processes with `ps`, and with
                # `pgrep` on darwin, where both are system tools
                pkgs.procps
                pkgs.typescript
              ]
              ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [
                (pkgs.writeShellScriptBin "pgrep" ''exec /usr/bin/pgrep "$@"'')
              ];

              buildPhase = ''
                runHook preBuild
                pnpm run ${name}
                runHook postBuild
              '';

              installPhase = ''
                runHook preInstall
                mkdir -p $out
                runHook postInstall
              '';
            });
        in
        {
          _module.args.pkgs = import nixpkgs {
            inherit system;
          };

          checks = {
            test = mkCheck "test";
            typecheck = mkCheck "typecheck";
            mutate = mkCheck "mutate";
          };

          devShells.default = pkgs.mkShellNoCC {
            inputsFrom = [ config.pre-commit.devShell ];

            packages =
              (with pkgs; [
                ni
                nodejs_26
                oxfmt
                oxlint
                pnpm_12
                typescript
                # srt resolves ripgrep on every platform it sandboxes
                ripgrep
              ])
              # @agent/sandbox-broker's integration test drives a real sandbox,
              # and skips itself unless these resolve on PATH. bubblewrap and
              # socat are linux-only in nixpkgs, and `nix flake check` evaluates
              # every system's devShell, so naming them on darwin is an error.
              ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
                pkgs.bubblewrap
                pkgs.socat
              ];
          };

          pre-commit.settings = {
            hooks = {
              actionlint.enable = true;
              deadnix.enable = true;
              oxlint.enable = true;
              statix.enable = true;
            };
          };

          treefmt = {
            projectRootFile = "flake.nix";
            programs = {
              nixfmt.enable = true;
              oxfmt.enable = true;
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
