{
  lib,
  writeNushellApplication,
}: let
  darwinXcrunShimFor = import ./darwin-xcrun-shim.nix {inherit lib writeNushellApplication;};
  zigWarmCacheFor = import ./zig-warm-cache.nix {};
in
  /**
  Build libghostty-vt: ghostty's terminal VT engine as a standalone C library.

  Ghostty's `build.zig` exposes a VT-only artifact through `-Demit-lib-vt=true`,
  which skips the GUI app, xcframework, and docs and emits just the parser,
  screen model, and render-state API. The result is a static `libghostty-vt.a`
  plus a self-contained `libghostty-vt.<ver>.dylib`/`.so`, the `ghostty/` C
  headers, and a pkg-config file.

  This is also, as of index#3768, the ONLY subtree of ghostty's darwin build
  that is buildable purely inside the Nix sandbox. Every other artifact --
  `GhosttyLib` (the macOS app's Zig glue library), `GhosttyKit.xcframework`,
  and the `.app` bundle -- reaches, unconditionally, for real Apple toolchain
  binaries at hardcoded absolute paths ghostty's own `build.zig` never routes
  through `PATH`:

  - `SharedDeps.initTarget` unconditionally creates a `MetallibStep` for any
    darwin target and every artifact built through `deps.add()` (the exe, the
    test binary, `GhosttyLib`) depends on it; `MetallibStep` shells out to
    `/usr/bin/xcrun -sdk macosx metal`, the proprietary Metal shader compiler,
    with no nixpkgs package and no OSS equivalent. This does not depend on the
    selected renderer (`-Drenderer=opengl` does not skip it) -- confirmed by
    attempting a `-Demit-test-exe=true` build against this recipe's darwin SDK
    shim, which reached `metallib Ghostty (Ghostty.ir)` and failed with
    `PermissionDenied` on `/usr/bin/xcrun` (the sandbox correctly denies execing
    an undeclared host binary).
  - `GhosttyLibVt.initLib`'s SIMD-dependency fat-archive path (and
    `GhosttyLib.initStatic`'s always-taken one) shell to `/bin/cp` and
    `/usr/bin/ranlib` to combine archives, hit in that same attempt right after
    the metallib failure.
  - `GhosttyXCFramework`/the macOS app additionally need
    `xcodebuild -create-xcframework` (`src/build/XCFrameworkStep.zig`).

  `-Demit-lib-vt=true` avoids all three: `GhosttyLibVt.initStatic`/`initShared`
  build directly off the `vt`/`vt_c` Zig modules, which never calls
  `deps.add()` and has no SIMD dependencies to fat-archive. That is a real
  structural boundary, not a flag we chose for convenience: the VT engine
  (parser, screen model) is deliberately renderer- and app-independent.

  Consequence: `packages/ghostty`, index#3768's vendored fork, builds and
  validates this same VT-only subtree from the checked Ghostty view. It
  does not reach the `Surface`/`apprt` process-lifecycle code the follow-up
  teardown fix touches -- getting there is real, separate follow-up work
  (likely: patch `build.zig` to route its tool calls through `PATH` so a
  future Nix recipe can substitute `xcrun`/`ranlib`, which unblocks the second
  bullet above but not the first; the Metal compiler has no substitute short of
  a real macOS builder outside the sandbox).

  Arguments:
  - `pkgs`: package set to build against; the artifact is host-system specific.
  - `ghosttySource`: Ghostty source tree from the checked `views.ghostty`
    path. Must ship `build.zig`, `build.zig.zon`, and
    `build.zig.zon.nix` (the zon2nix output that vendors every lazy Zig
    dependency with SRI hashes for a network-free build).
  - `baseSource`: optional. When set, zig's own `--cache-dir` (a
    content-addressed cache keyed by each file's digest, its `Manifest`
    system) is warmed once from this UNPATCHED base and copied into the real
    build as a starting point, so a view change to `ghosttySource`
    recompiles only what it touches instead of the whole tree -- the same
    "small delta, small rebuild" property cargo-unit gets by splitting Cargo
    builds per-crate, without needing a per-translation-unit Nix decomposition
    zig's build graph has no boundary for. `warmCache` is keyed on `baseSource`
    alone, so it survives every view change and rebuilds only when the
    upstream pin moves. Omit (the default) only when `ghosttySource` never
    diverges from `baseSource`, where warming a separate cache buys nothing.
  - `version`: derivation version. Defaults to the value in `build.zig.zon`.

  The static archive does not bundle its C++ dependencies (`libhighway`,
  `libsimdutf`, `libutfcpp`) and needs `-lc++`; the dylib is self-contained, so
  `ix-vt-sys` links the dylib to avoid that archive dance.
  */
  pkgs: {
    ghosttySource,
    baseSource ? ghosttySource,
    version ? "1.3.2-dev",
  }: let
    inherit (pkgs) stdenv;

    # zon2nix output checked into the ghostty tree. Keyed on `baseSource` (equal
    # to `ghosttySource` when the caller omits it) so a view change never
    # invalidates the dependency fetch, matching `warmCache` below.
    deps = pkgs.callPackage (baseSource + "/build.zig.zon.nix") {
      inherit (pkgs) zig_0_15;
      name = "libghostty-vt-deps-${version}";
    };

    isDarwin = stdenv.hostPlatform.isDarwin;

    # zig 0.15's darwin SDK probe shells out to `xcrun`/`xcode-select` via
    # `PATH` (unlike the hardcoded-absolute-path calls documented above), so
    # this one is shimmable to the pinned nixpkgs apple-sdk.
    inherit (darwinXcrunShimFor pkgs) appleSdk appleSdkRoot darwinSdkInputs;

    # Since the 49a43bf5 ghostty pin the vt static lib vendors SIMD deps
    # (simdutf, highway, utfcpp) and combines the archives on darwin with
    # `libtool`/`ranlib` resolved from PATH (fork patch
    # 0004-build-keep-Demit-lib-vt-buildable-without-Apple-abso); cctools
    # supplies both in the sandbox.
    commonNativeBuildInputs =
      [pkgs.zig_0_15 pkgs.pkg-config]
      ++ darwinSdkInputs
      ++ lib.optional isDarwin pkgs.darwin.cctools;
    commonBuildInputs = [pkgs.zlib] ++ lib.optional isDarwin appleSdk;

    seedGlobalCache = ''
      export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-cache"
      mkdir -p "$ZIG_GLOBAL_CACHE_DIR/p"
      cp -R --no-preserve=mode ${deps}/. "$ZIG_GLOBAL_CACHE_DIR/p/"
      ${lib.optionalString isDarwin ''
        export SDKROOT="${appleSdkRoot}"
        export DEVELOPER_DIR="${appleSdk}"
      ''}
    '';

    # An explicit -Dtarget (never `native`) keeps the artifact byte-identical
    # across build hosts: zig resolves a native target by probing the RUNNING
    # kernel and glibc (std.zig.system.resolveTargetQuery) and embeds those
    # versions in compiler_rt.o's `.data.rel.ro.builtin.target`, so a fleet
    # kernel upgrade flips the narHash of every rebuild and cache publish
    # proofs can never converge (indexable-inc/ix#8465: 7.0.11 vs 7.1.3
    # workers). An explicit triple selects zig's fixed default OS/glibc
    # version range instead.
    zigTargetTriple =
      {
        x86_64-linux = "x86_64-linux-gnu";
        aarch64-linux = "aarch64-linux-gnu";
        x86_64-darwin = "x86_64-macos-none";
        aarch64-darwin = "aarch64-macos-none";
      }
      .${
        stdenv.hostPlatform.system
      }
      or (throw "libghostty-vt: no zig target triple mapped for ${stdenv.hostPlatform.system}");

    zigInstallArgs = ''
      -Demit-lib-vt=true \
      -Dtarget=${zigTargetTriple} \
      -Dcpu=baseline \
      -Doptimize=ReleaseFast \
      -fsys=zlib --search-prefix ${pkgs.zlib}'';

    zigWarmCache = zigWarmCacheFor pkgs;

    warmCache =
      if baseSource == ghosttySource
      then null
      else
        zigWarmCache.mkWarmCache {
          pname = "libghostty-vt";
          inherit version baseSource;
          setup = seedGlobalCache;
          zigArgs = ''
            --global-cache-dir "$ZIG_GLOBAL_CACHE_DIR" \
            ${zigInstallArgs}'';
          nativeBuildInputs = commonNativeBuildInputs;
          buildInputs = commonBuildInputs;
        };
  in
    stdenv.mkDerivation {
      pname = "libghostty-vt";
      inherit version;

      src = builtins.path {
        name = "ghostty-source";
        path = ghosttySource;
      };

      strictDeps = true;
      nativeBuildInputs = commonNativeBuildInputs;
      buildInputs = commonBuildInputs;

      dontConfigure = true;
      dontBuild = true;

      installPhase = ''
        # shell
        runHook preInstall

        ${seedGlobalCache}
        mkdir -p "$TMPDIR/zig-local-cache"
        ${zigWarmCache.seedFrom warmCache}

        buildCores=1
        if [ "''${enableParallelBuilding-1}" ]; then
          buildCores="$NIX_BUILD_CORES"
        fi

        zig build \
          "-j$buildCores" \
          --global-cache-dir "$ZIG_GLOBAL_CACHE_DIR" \
          --cache-dir "$TMPDIR/zig-local-cache" \
          ${zigInstallArgs} \
          --prefix "$out" \
          --summary all

        runHook postInstall
      '';

      doCheck = false;

      passthru =
        lib.optionalAttrs (warmCache != null) {inherit warmCache;}
        // {
          # Ghostty's own vt/vt_c module unit tests (`zig build test-lib-vt`).
          # Unlike the full `zig build test` exe this step never goes through
          # `deps.add()`, so it has no `/usr/bin/xcrun metal` dependency and
          # runs inside the sandbox on every platform -- the one lane that
          # executes fork-patch test additions to the VT engine (e.g. the
          # per-cell hyperlink URI patch, index#3835).
          unitTests = stdenv.mkDerivation {
            pname = "libghostty-vt-unit-tests";
            inherit version;

            src = builtins.path {
              name = "ghostty-source";
              path = ghosttySource;
            };

            strictDeps = true;
            nativeBuildInputs = commonNativeBuildInputs;
            buildInputs = commonBuildInputs;

            dontConfigure = true;
            dontBuild = true;

            # This derivation IS the test run (`zig build test-lib-vt` in
            # installPhase); there is no separate checkPhase to switch on.
            doCheck = false;

            installPhase = ''
              # shell
              runHook preInstall

              ${seedGlobalCache}
              mkdir -p "$TMPDIR/zig-local-cache"
              ${zigWarmCache.seedFrom warmCache}

              buildCores=1
              if [ "''${enableParallelBuilding-1}" ]; then
                buildCores="$NIX_BUILD_CORES"
              fi

              zig build test-lib-vt \
                "-j$buildCores" \
                --global-cache-dir "$ZIG_GLOBAL_CACHE_DIR" \
                --cache-dir "$TMPDIR/zig-local-cache" \
                -Dcpu=baseline \
                -fsys=zlib --search-prefix ${pkgs.zlib} \
                --summary all

              mkdir -p "$out"

              runHook postInstall
            '';
          };
        };

      meta = {
        description = "Ghostty's terminal VT engine as a standalone C library (parser, screen, render state)";
        homepage = "https://ghostty.org/";
        license = lib.licenses.mit;
        platforms = lib.platforms.unix;
      };
    }
