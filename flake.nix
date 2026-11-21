{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs";
    flake-parts.url = "github:hercules-ci/flake-parts";
    flake-parts.inputs.nixpkgs-lib.follows = "nixpkgs";
    git-hooks.url = "github:cachix/git-hooks.nix";
    git-hooks.inputs.nixpkgs.follows = "nixpkgs";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
    nix-bun.url = "github:ryoppippi/nix-bun";
    nix-bun.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      nixpkgs,
      flake-parts,
      git-hooks,
      treefmt-nix,
      nix-bun,
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
          lib,
          pkgs,
          system,
          ...
        }:
        let
          sources = lib.cleanSourceWith {
            src = ./.;
            filter =
              path: type:
              let
                base = baseNameOf path;
                ignored = builtins.elem base [
                  "node_modules"
                  ".direnv"
                  ".git"
                  ".mutator"
                  "result"
                ];
              in
              type != "symlink" && !ignored && !(lib.hasPrefix "result-" base);
          };

          # Dependency store as a fixed-output derivation: `bun install` may use
          # the network here, and every other check reuses the same node_modules.
          bunDeps = pkgs.stdenvNoCC.mkDerivation {
            pname = "agent-node-modules";
            version = "1";
            src = sources;
            nativeBuildInputs = [ pkgs.bun ];
            dontFixup = true;
            buildPhase = ''
              export HOME=$TMPDIR
              bun install --frozen-lockfile --linker hoisted
            '';
            installPhase = ''
              mkdir -p $out
              cp -r node_modules $out/node_modules
              cp -r packages $out/packages
            '';
            outputHashMode = "recursive";
            outputHash = "sha256-Vw1eTxbwF6NpdrVs0oWABThBQnPfq4cmCtbJqdjC/+A=";
          };

          # One derivation per check. `node_modules` and the workspace packages
          # come from `bunDeps`, so a check never reaches the network.
          projectCheck =
            name: command:
            pkgs.stdenvNoCC.mkDerivation {
              inherit name;
              src = sources;
              nativeBuildInputs = [
                pkgs.bun
                pkgs.git
              ];
              buildPhase = ''
                export HOME=$TMPDIR
                cp -r ${bunDeps}/node_modules ./node_modules
                chmod -R u+w node_modules
                ${command}
              '';
              installPhase = "touch $out";
            };
        in
        {
          _module.args.pkgs = import nixpkgs {
            inherit system;
            overlays = [ nix-bun.overlays.default ];
          };

          apps = { };

          checks = {
            fmt = projectCheck "fmt" "bun fmt --check";
            types = projectCheck "types" "bun check -b";
            test = projectCheck "test" "bun test";
            mutation = projectCheck "mutation" "bun run mutate";
          };

          devShells.default = pkgs.mkShellNoCC {
            inputsFrom = [ config.pre-commit.devShell ];

            packages = with pkgs; [
              ni
              bun
            ];
          };

          packages = { };

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
            };
          };
        };

      systems = [
        "x86_64-linux"
      ];
    };
}
