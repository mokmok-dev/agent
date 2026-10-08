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
                hash = "sha256-d3WH2+I0SrQWUm7HEkm6p3OLorf6Wmi9DEjcx4SCYHo=";
              };

              nativeBuildInputs = [
                pkgs.nodejs_26
                pkgs.pnpm_12
                pkgs.pnpmConfigHook
                # stryker kills its test runner processes with `ps`
                pkgs.procps
                pkgs.typescript
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

            packages = with pkgs; [
              ni
              nodejs_26
              oxfmt
              oxlint
              pnpm_12
              typescript
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
