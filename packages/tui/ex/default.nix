{ix}:
# The consumable artifact is the compiled `:tui_ex` OTP app (generated
# Elixir host modules + the NIF under priv/native), laid out so a BEAM can
# `Code.append_path`/`Application.load` it at runtime -- ix-mcp-ex does
# exactly that via `IX_MCP_TUI_EX`. `passthru.tests.run` is the
# conformance-style proof (mirrors packages/plumb/ex): compile the mix
# package warnings-as-errors and drive real PTY children through the
# generated bindings with ExUnit, gated in CI as
# `checks.<system>.tui-ex-run`.
let
  inherit (ix) pkgs;

  built = ix.unibind.build {
    crate = "tui-ex";
    targets.ex = {
      mixSource = ./mix;
    };
  };

  # Must match ix-mcp-ex exactly: it loads these .beam files at runtime, so a
  # different OTP is a runtime failure no build catches. Both are on 1.20/29, and
  # the BEAM does not load bytecode from a newer OTP than its own.
  elixir = ix.languages.elixir.toolchain pkgs {version = "1.20";};
  erlang = ix.languages.erlang.toolchain pkgs {version = "29";};

  version = "0.1.0"; # keep in sync with unibind's generated mix.exs

  sharedEnv = {
    HEX_OFFLINE = "1";
    LANG = "C.UTF-8";
    LC_CTYPE = "C.UTF-8";
  };

  app = pkgs.stdenv.mkDerivation {
    pname = "tui-ex";
    inherit version;
    src = built.ex.mixPackage;
    strictDeps = true;
    # MIX_ENV=prod compile of generated bindings; no ExUnit suite ships here.
    doCheck = false;

    nativeBuildInputs = [erlang elixir];
    env = sharedEnv // {MIX_ENV = "prod";};

    # Mix 1.18+ opens a loopback socket unconditionally (Mix.Sync.PubSub)
    # and the darwin sandbox denies it (:eperm) without this.
    __darwinAllowLocalNetworking = true;

    postUnpack = ''
      export HOME="$TEMPDIR"
      export MIX_HOME="$TEMPDIR/mix"
      export HEX_HOME="$TEMPDIR/hex"
    '';

    buildPhase = ''
      # shell
      runHook preBuild
      mix compile --warnings-as-errors
      runHook postBuild
    '';

    # `-L` resolves mix's priv symlink so the app dir is self-contained;
    # the layout ($out/lib/tui_ex/{ebin,priv}) is what `:code.priv_dir`
    # expects once $out/lib/tui_ex/ebin is on the code path.
    installPhase = ''
      # shell
      runHook preInstall
      mkdir -p "$out/lib"
      cp -RL _build/prod/lib/tui_ex "$out/lib/tui_ex"
      # Mix's compile manifests (.mix/compile.elixir etc.) carry build-time
      # mtimes, so shipping them makes every rebuild's narHash differ and the
      # cache publish proof never converges (indexable-inc/ix#8465). Nothing
      # reads them at runtime: :code.priv_dir wants only ebin/ and priv/.
      rm -rf "$out/lib/tui_ex/.mix"
      runHook postInstall
    '';

    meta.description = "Compiled tui_ex OTP app: unibind-generated Elixir bindings over the local tui PTY driver";
  };

  run = pkgs.stdenv.mkDerivation {
    pname = "tui-ex-run";
    inherit version;
    src = built.ex.mixPackage;
    strictDeps = true;

    # The ExUnit suite has zero hex deps, so the sandbox never fetches;
    # HEX_OFFLINE guards regressions.
    nativeBuildInputs = [erlang elixir];
    env = sharedEnv // {MIX_ENV = "test";};

    __darwinAllowLocalNetworking = true;

    postUnpack = ''
      export HOME="$TEMPDIR"
      export MIX_HOME="$TEMPDIR/mix"
      export HEX_HOME="$TEMPDIR/hex"
    '';

    buildPhase = ''
      # shell
      runHook preBuild
      mix compile --warnings-as-errors
      runHook postBuild
    '';

    doCheck = true;
    checkPhase = ''
      # shell
      runHook preCheck
      mix format --check-formatted
      # --trace prints every test name and runs sequentially, which the
      # process-global terminal registry wants anyway.
      mix test --trace
      runHook postCheck
    '';

    installPhase = ''
      # shell
      runHook preInstall
      mkdir -p "$out"
      runHook postInstall
    '';

    meta.description = "tui Elixir binding smoke suite: real PTY children driven through the generated bindings (ExUnit)";
  };
in
  app.overrideAttrs (old: {
    passthru =
      (old.passthru or {})
      // {
        tests =
          (old.passthru.tests or {})
          // {
            inherit run;
          };
      };
  })
