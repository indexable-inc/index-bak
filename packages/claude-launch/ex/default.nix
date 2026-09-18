{ix}:
# The consumable artifact is the compiled `:claude_launch` OTP app
# (unibind-generated Elixir host modules plus the NIF under priv/native), so
# a BEAM can `Code.append_path`/`Application.load` it the way ix-mcp-ex
# already loads tui-ex. `passthru.tests.run` is the proof: compile the mix
# package warnings-as-errors and drive real child processes through the
# generated bindings with ExUnit, gated in CI as
# `checks.<system>.claude-launch-ex-run`.
#
# The suite spawns a stub `claude` written by the test itself, not the real
# CLI: the CLI needs credentials no sandbox has. The tests tagged `e2e` do
# use the real binary and are excluded unless CLAUDE_LAUNCH_E2E=1.
let
  inherit (ix) pkgs;

  built = ix.unibind.build {
    crate = "claude-launch-ex";
    targets.ex = {
      mixSource = ./mix;
    };
  };

  # Must match ix-mcp-ex exactly: it loads these .beam files at runtime, so a
  # different OTP is a runtime failure no build catches (mirrors
  # packages/tui/ex).
  elixir = ix.languages.elixir.toolchain pkgs {version = "1.20";};
  erlang = ix.languages.erlang.toolchain pkgs {version = "29";};

  version = "0.1.0"; # keep in sync with unibind's generated mix.exs

  sharedEnv = {
    HEX_OFFLINE = "1";
    LANG = "C.UTF-8";
    LC_CTYPE = "C.UTF-8";
  };

  common = {
    src = built.ex.mixPackage;
    strictDeps = true;
    nativeBuildInputs = [erlang elixir];

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
  };

  app = pkgs.stdenv.mkDerivation (common
    // {
      pname = "claude-launch-ex";
      inherit version;
      env = sharedEnv // {MIX_ENV = "prod";};

      # `-L` resolves mix's priv symlink so the app dir is self-contained;
      # the layout ($out/lib/claude_launch/{ebin,priv}) is what
      # `:code.priv_dir` expects once ebin is on the code path.
      installPhase = ''
        # shell
        runHook preInstall
        mkdir -p "$out/lib"
        cp -RL _build/prod/lib/claude_launch "$out/lib/claude_launch"
        runHook postInstall
      '';

      meta.description = "Compiled claude_launch OTP app: typed Claude Code launcher over unibind";
    });

  run = pkgs.stdenv.mkDerivation (common
    // {
      pname = "claude-launch-ex-run";
      inherit version;

      # The ExUnit suite has zero hex deps, so the sandbox never fetches;
      # HEX_OFFLINE guards regressions.
      env = sharedEnv // {MIX_ENV = "test";};

      doCheck = true;
      checkPhase = ''
        # shell
        runHook preCheck
        mix format --check-formatted
        # --trace prints every test name, so the CI log is the evidence.
        mix test --trace
        runHook postCheck
      '';

      installPhase = ''
        # shell
        runHook preInstall
        mkdir -p "$out"
        runHook postInstall
      '';

      meta.description = "claude-launch Elixir binding suite: real child processes driven through the generated bindings (ExUnit)";
    });
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
