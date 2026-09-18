{
  # The shared strict Credo config (lib/elixir/credo.exs), injected by
  # lib/default.nix so every Elixir gate enforces the same policy. The Elixir
  # counterpart of how `ruffAnnArgs` is threaded into buildUvApplication.
  credoConfig,
}:
/**
Build the required Elixir quality-lane check derivation, shared by every
repo-owned Elixir package so the lane is defined once.

The lane mirrors what each package's standalone CI ran, now sandboxed and
reproducible:
  1. `mix compile --warnings-as-errors` — the Elixir 1.18+ set-theoretic type
     checker runs here, so type findings (and any warning) fail the build.
  2. `mix format --check-formatted` — formatting is enforced, not advisory.
  3. `mix credo --strict` against the shared `credoConfig` — the static-analysis
     gate (security/perf/modernization/readability, plus the ExSlop plugin's
     LLM-slop checks), single source of truth.
  4. `mix test` — the package's ExUnit suite.

2, 3 and 4 all run on every build and the failure names every one of them that
failed, so a formatting slip can no longer stand in front of a failing test
suite and report itself as the lane's verdict. 1 still ends the build on its
own — credo and ExUnit have nothing to read without it — but it says in the log
that the other three did not run.

The result is meant to be attached as `passthru.tests.elixir` on the package
and wired into `checks` through lib/per-system.nix, exactly as before.

Arguments:
- `pkgs`: nixpkgs instance.
- `pname`, `version`: derivation identity.
- `src`: the package source (a fileset toSource).
- `elixir`, `erlang`: the toolchain pair the package builds against; the launcher
  must use the same pairing so a deploy never runs code the gate did not.
- `sourceRoot`: optional stdenv sourceRoot when the mix project is nested under
  the src (e.g. "source/elixir").
- `mixDeps`: a `beamPackages.fetchMixDeps` result for packages with hex deps;
  `null` for a dependency-free project (offline `mix` against source alone).
- `extraEnv`: extra derivation env (e.g. MIX_REBAR for rebar-built deps, or
  LD_LIBRARY_PATH for a precompiled NIF).
- `extraNativeBuildInputs`: extra packages on PATH for the build.
- `setupHook`: extra shell appended to `postUnpack` (e.g. seeding an
  elixir_make artifact cache to short-circuit a NIF download in the sandbox).
*/
pkgs: {
  pname,
  version ? "0.0.0",
  src,
  elixir,
  erlang,
  sourceRoot ? null,
  mixDeps ? null,
  extraEnv ? {},
  extraNativeBuildInputs ? [],
  setupHook ? "",
}: let
  inherit (pkgs) lib;
  hasDeps = mixDeps != null;
  # With vendored deps present, mix must not try to re-resolve them online.
  depsCheckFlag = lib.optionalString hasDeps "--no-deps-check";
in
  pkgs.stdenv.mkDerivation {
    inherit pname version src;
    sourceRoot =
      if sourceRoot != null
      then sourceRoot
      else null;

    strictDeps = true;
    # Mix >= 1.18 starts Mix.PubSub, which opens a loopback TCP socket at
    # compile time; the darwin sandbox denies plain sockets without this.
    __darwinAllowLocalNetworking = true;

    nativeBuildInputs =
      [
        erlang
        elixir
        pkgs.git
      ]
      # hex is only needed when compiling fetched deps; a no-deps build skips it.
      ++ lib.optional hasDeps (import ./elixir-hex.nix {inherit pkgs elixir;})
      ++ extraNativeBuildInputs;

    env =
      {
        MIX_ENV = "test";
        HEX_OFFLINE = "1";
        LANG = "C.UTF-8";
        LC_CTYPE = "C.UTF-8";
      }
      // extraEnv;

    postUnpack =
      ''
        export MIX_HOME="$TEMPDIR/mix"
        export HEX_HOME="$TEMPDIR/hex"
      ''
      + lib.optionalString hasDeps ''
        export MIX_DEPS_PATH="$TEMPDIR/deps"
        cp --no-preserve=mode -R "${mixDeps}" "$MIX_DEPS_PATH"
      ''
      + setupHook;

    configurePhase =
      ''
        runHook preConfigure
      ''
      + lib.optionalString hasDeps ''
        mix deps.compile --no-deps-check --skip-umbrella-children
        ln -s "$MIX_DEPS_PATH" ./deps
      ''
      + ''
        # The shared Credo policy is injected here rather than committed per-package,
        # so the gate cannot drift between packages.
        cp ${credoConfig} .credo.exs
        runHook postConfigure
      '';

    # Compile is the one phase that is still allowed to end the build, because
    # credo and ExUnit both read the beams it produces and neither says
    # anything useful without them. What it must not do is end the build
    # quietly: without the message below, a compile failure looks from the tail
    # of the log exactly like a lane that ran and passed the rest.
    buildPhase = ''
      # shell
      runHook preBuild
      if mix compile ${depsCheckFlag} --warnings-as-errors; then
        :
      else
        rc=$?
        echo "elixir check: compile FAILED (exit $rc)" >&2
        echo "elixir check: format, credo and test did NOT run" >&2
        exit "$rc"
      fi
      runHook postBuild
    '';

    doCheck = true;

    # Every phase runs, and the report at the end names all of them.
    #
    # These three used to be three bare commands under errexit, so the first
    # non-zero exit ended the derivation and the others never ran -- and
    # nothing in the output said so. A check that stops at its cheapest step is
    # reporting on the cheapest step: in test-ide a formatting slip held `mix
    # format --check-formatted` at exit 1 for a day, and five failing
    # ClickHouse tests underneath it were invisible until somebody fixed the
    # formatting for unrelated reasons (ENG-10061). The cost is not the day, it
    # is that the gate was green-adjacent the whole time and reported a fact
    # about whitespace.
    #
    # Serialising the discovery one build at a time is not the fix either. The
    # three are independent -- formatting is not a precondition for credo, and
    # neither is one for ExUnit -- so all three run, each verdict is kept, and
    # the failure at the end lists every phase that failed rather than the
    # first.
    checkPhase = ''
      # shell
      runHook preCheck

      elixirCheckFailed=""

      # Records a verdict instead of ending the build. The condition spelling
      # is load-bearing: errexit is suspended only for a command that IS a
      # condition, so `if cmd; then rc=0; else rc=$?; fi` survives where
      # `cmd; rc=$?` would have exited at `cmd`.
      elixirCheckRun() {
        local name="$1"
        shift
        local rc
        echo "elixir check: >>> $name"
        if "$@"; then rc=0; else rc=$?; fi
        if [ "$rc" -eq 0 ]; then
          echo "elixir check: <<< $name passed"
        else
          echo "elixir check: <<< $name FAILED (exit $rc)"
          elixirCheckFailed="$elixirCheckFailed $name"
        fi
      }

      elixirCheckRun format mix format --check-formatted
      elixirCheckRun credo mix credo --strict
      elixirCheckRun test mix test ${depsCheckFlag}

      if [ -n "$elixirCheckFailed" ]; then
        # Last, because a failing nix build prints the tail of the log and this
        # is the line that has to survive that truncation.
        echo "elixir check: FAILED phases:$elixirCheckFailed" >&2
        echo "elixir check: compile, format, credo and test all ran; each one's output is above" >&2
        exit 1
      fi

      echo "elixir check: compile, format, credo and test all passed"
      runHook postCheck
    '';

    installPhase = ''
      # shell
      runHook preInstall
      mkdir -p "$out"
      runHook postInstall
    '';
  }
