{
  pkgs,
  system,
}:

let
  inherit (pkgs) lib;

  version = "0.67.0";
  rustTarget =
    {
      aarch64-darwin = "aarch64-apple-darwin";
      aarch64-linux = "aarch64-unknown-linux-gnu";
      x86_64-linux = "x86_64-unknown-linux-gnu";
    }
    .${system};

  bundle = pkgs.fetchurl {
    url = "https://github.com/model-checking/kani/releases/download/kani-${version}/kani-${version}-${rustTarget}.tar.gz";
    hash =
      {
        aarch64-darwin = "sha256-f9C3ETCqN70eNG66Z1qtCem2bks4P3B/xlczDOsp7uw=";
        aarch64-linux = "sha256-l0Eo9E3UNhigbSHl/m2f9nGI3lmG/gvFe1NLDkY577k=";
        x86_64-linux = "sha256-O196/TtRYD7nINt7wbxP5GtaT1022q2ZOcS0xli1GsA=";
      }
      .${system};
  };

  # Kani's release bundle records this exact nightly in
  # rust-toolchain-version. `kani-compiler` dynamically links rustc_driver from
  # the matching toolchain, so this package cannot use the workspace stable
  # toolchain.
  toolchain = pkgs.rust-bin.nightly."2025-11-21".minimal;

  proxy = pkgs.rustPlatform.buildRustPackage rec {
    pname = "kani-verifier";
    inherit version;

    src = pkgs.fetchCrate {
      inherit pname version;
      hash = "sha256-m0khwmHJAiEtICN/f2IE70A2/0JNKwaL3so429YtdOY=";
    };

    cargoHash = "sha256-KAFLA97yi74riDkBO3EJ9Uv6SdVQrJ1wLNJ68Jf9yWk=";
    doCheck = false;
  };

  # CBMC's goto-cc defaults to the executable name `gcc`. Darwin's stdenv
  # provides Clang as `cc`/`clang`, while /usr/bin/gcc may only be an unusable
  # xcrun shim in a pure Nix environment.
  darwinGcc = pkgs.writeShellScriptBin "gcc" ''
    exec "${pkgs.stdenv.cc}/bin/clang" "$@"
  '';
in
pkgs.stdenv.mkDerivation {
  pname = "kani-verifier";
  inherit version;

  src = bundle;
  dontUnpack = true;

  nativeBuildInputs = [
    pkgs.makeWrapper
  ]
  ++ lib.optionals pkgs.stdenv.hostPlatform.isLinux [ pkgs.autoPatchelfHook ];
  buildInputs = lib.optionals pkgs.stdenv.hostPlatform.isLinux [
    toolchain
    pkgs.stdenv.cc.cc.lib
    pkgs.zlib
  ];

  installPhase = ''
    runHook preInstall

    mkdir -p "$out/bin" "$out/share/kani"
    tar -xzf "$src" -C "$out/share/kani"
    ln -s "${toolchain}" "$out/share/kani/kani-${version}/toolchain"

    makeWrapper "${proxy}/bin/cargo-kani" "$out/bin/cargo-kani" \
      --set KANI_HOME "$out/share/kani"
    makeWrapper "${proxy}/bin/kani" "$out/bin/kani" \
      --set KANI_HOME "$out/share/kani"

    ${lib.optionalString pkgs.stdenv.hostPlatform.isDarwin ''
      wrapProgram "$out/bin/cargo-kani" \
        --prefix PATH : "${lib.makeBinPath [ darwinGcc ]}"
      wrapProgram "$out/bin/kani" \
        --prefix PATH : "${lib.makeBinPath [ darwinGcc ]}"
    ''}

    runHook postInstall
  '';

  passthru = {
    inherit
      bundle
      proxy
      toolchain
      ;
  };

  meta = {
    description = "Bit-precise model checker for Rust";
    homepage = "https://github.com/model-checking/kani";
    license = with lib.licenses; [
      asl20
      mit
    ];
    mainProgram = "cargo-kani";
    platforms = [
      "aarch64-darwin"
      "aarch64-linux"
      "x86_64-linux"
    ];
  };
}
