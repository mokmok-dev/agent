# Upstream's workspace holds one crate per language adapter. The Kotlin grammar
# it takes from git is vendored through the hash crane needs, so the build stays
# offline after the vendor step.
{
  craneLib,
  lib,
  pkgs,
}:
let
  version = "1.8.5";
  src = pkgs.fetchFromGitHub {
    owner = "moznion";
    repo = "cccc";
    tag = "v${version}";
    hash = "sha256-8QfPfyg7Y05wbiWnLyGLpDe4F1SkvTJuZPLn+5ZQIpw=";
  };
in
craneLib.buildPackage {
  pname = "cccc";
  inherit src version;

  strictDeps = true;
  # ruby-prism builds vendored C and binds it, so the crate needs a C compiler
  # and libclang; the tree-sitter grammars need the compiler alone. The cache
  # tests shell out to git.
  nativeBuildInputs = [
    pkgs.git
    pkgs.rustPlatform.bindgenHook
  ];
  cargoVendorDir = craneLib.vendorCargoDeps {
    inherit src;
    outputHashes = {
      "git+https://github.com/exoego/tree-sitter-kotlin?rev=97ab2826bf686723ca7dcc34d55f758d9c41a59d#97ab2826bf686723ca7dcc34d55f758d9c41a59d" =
        "sha256-O9zCo8G8321cpqVp4z8USwyoLtnJReNhWs1FpYa9IVQ=";
    };
  };
  cargoBuildFlags = [
    "-p"
    "cccc-cli"
  ];
  meta = {
    description = "Cognitive and cyclomatic complexity measurement across bundled languages";
    homepage = "https://github.com/moznion/cccc";
    license = lib.licenses.mit;
    mainProgram = "cccc";
  };
}
