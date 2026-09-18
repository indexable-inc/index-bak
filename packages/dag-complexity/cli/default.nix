{
  ix,
  lib,
  stdenvNoCC,
  makeWrapper,
}: let
  unwrapped = ix.cargoUnit.selectBinaryWithTests ix.rustWorkspace.units {
    binary = "dag-complexity";
    packageName = "dag-complexity-cli";
    meta = {
      description = "Blast radius, critical path, parallelism and leverage over any DAG";
      license = lib.licenses.mit;
      mainProgram = "dag-complexity";
    };
  };

  # `dag-complexity rust` shells out to rust-analyzer, which needs a matching
  # cargo/rustc to load the workspace and rust-src for sysroot analysis. Baking
  # the repo's pinned channel keeps the graph from drifting with whatever
  # toolchain happens to be on the caller's PATH. Same recipe as scipql, which
  # runs the same indexer.
  toolchainFile = lib.importTOML (ix.paths.root + "/rust-toolchain.toml");
  nightlyDate = lib.removePrefix "nightly-" toolchainFile.toolchain.channel;
  rustToolchain = ix.languages.rust.toolchain ix.pkgs {
    channel = "nightly";
    version = nightlyDate;
    components = [
      "cargo"
      "rustc"
      "rust-std"
      "rust-src"
      "rust-analyzer"
    ];
  };
in
  stdenvNoCC.mkDerivation {
    pname = "dag-complexity";
    inherit (unwrapped) version;

    dontUnpack = true;
    strictDeps = true;
    nativeBuildInputs = [makeWrapper];

    installPhase = ''
      # shell
      runHook preInstall
      mkdir -p "$out/bin"
      # Prefix, not suffix: an ambient rustup shim on PATH would otherwise
      # shadow the pinned rust-analyzer and index against a different compiler.
      makeWrapper ${lib.getExe unwrapped} "$out/bin/dag-complexity" \
        --prefix PATH : ${lib.makeBinPath [rustToolchain]}
      runHook postInstall
    '';

    meta = {
      description = "Blast radius, critical path, parallelism and leverage over any DAG";
      license = lib.licenses.mit;
      mainProgram = "dag-complexity";
    };
  }
