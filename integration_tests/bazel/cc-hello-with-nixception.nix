# integration_tests/bazel/cc-hello-with-nixception.nix
#
# Integration test: build a minimal C++ project via Bazel + nixception.
#
# This test exercises the full REAPI round-trip:
#   Bazel → gRPC → nixception → recursive-nix daemon → nix build
#
# Bazel natively speaks the Remote Execution API, so no recc/buildbox
# intermediary is needed.  All compilation and linking actions are dispatched
# to nixception via --remote_executor, which in turn creates nix derivations
# for each action and builds them through the recursive-nix daemon socket.
#
# Build flow
# ──────────
#   1. unpackPhase           – copy the minimal Bazel project into $TMPDIR
#   2. nixceptionStartPhase  – (preConfigurePhase) server comes UP
#   3. configurePhase        – (no-op, but the hook runs before it)
#   4. buildPhase            – bazel build //src:hello-world via remote exec
#   5. installPhase          – copy binary to $out, verify it runs
#   6. exitHook/failureHook  – server comes DOWN
#
# The Bazel project lives in integration_tests/bazel/project/ and uses
# WORKSPACE mode with an empty WORKSPACE file, so no *user-declared* external
# dependencies are needed.  However, Bazel 7's DEFAULT.WORKSPACE.SUFFIX still
# implicitly pulls in bazel_skylib, rules_cc, and rules_python for its built-in
# toolchain resolution.  Since the Nix build sandbox blocks network access, we
# pre-fetch those archives via fetchurl and populate Bazel's --repository_cache
# so it finds them offline.
#
# Requirements (nix.conf / NixOS config):
#   experimental-features = nix-command recursive-nix
#   system-features       = recursive-nix
#
# Called from the top-level flake, e.g.:
#
#   bazel-cc-hello-nixception-test = pkgs.callPackage
#     integration_tests/bazel/cc-hello-with-nixception.nix {
#       inherit nixceptionHook;
#     };
#
{
  nixceptionHook,
  gcc,
  stdenv,
  bazel_7,
  fetchurl,
  lib,
}: let
  # ── Pre-fetched Bazel implicit dependencies ──────────────────────────────
  # Bazel 7's DEFAULT.WORKSPACE.SUFFIX declares http_archive rules for these
  # three repos.  They are fetched during the analysis phase (before any build
  # actions) and there is no flag to disable them.  We pre-fetch the exact
  # archives Bazel expects and populate its --repository_cache so it can
  # resolve them without network access.
  bazelSkylib = fetchurl {
    url = "https://github.com/bazelbuild/bazel-skylib/releases/download/1.6.1/bazel-skylib-1.6.1.tar.gz";
    hash = "sha256-nziIakBUjG6WwQa3UvJCEw7hGqoGila6flb0UR8z5PI=";
  };
  rulesCC = fetchurl {
    url = "https://github.com/bazelbuild/rules_cc/releases/download/0.0.9/rules_cc-0.0.9.tar.gz";
    hash = "sha256-IDeHW5pEVtzkp50RKorohbvEqtlo5lh9ym5k86CQDN8=";
  };
  rulesPython = fetchurl {
    url = "https://github.com/bazelbuild/rules_python/releases/download/0.24.0/rules_python-0.24.0.tar.gz";
    hash = "sha256-CoADsEQpTXhArH2dc+7wXWzraC11FngaTsYu6zRwJXg=";
  };
in
  stdenv.mkDerivation {
    pname = "bazel-cc-hello-nixception-test";
    version = "0.1.0";

    src = ./project;

    # ── recursive-nix ────────────────────────────────────────────────────────
    # Exposes the Nix daemon socket at /build/.nix-socket inside the sandbox
    # so nixception can use the local Nix store as its remote-execution backend.
    requiredSystemFeatures = ["recursive-nix"];

    # ── nixception setup hook ────────────────────────────────────────────────
    # gcc is injected into the runner sandbox so remote compilation/linking
    # actions can find it.  The hook registers nixceptionStartPhase as a
    # preConfigurePhase and tears the server down via exitHook / failureHook.
    #
    # bazel_7 is needed as a build tool to drive the build.
    nativeBuildInputs = [
      (nixceptionHook.withPackages [gcc])
      bazel_7
    ];

    # ── Bazel environment setup ──────────────────────────────────────────────
    # Bazel needs a writable HOME for its output base, caches, and internal
    # state.  Inside the nix sandbox $HOME may not exist or be writable.
    # We also explicitly set CC/CXX so Bazel's auto-detected C++ toolchain
    # matches what is available in the nixception runner sandbox.
    #
    # The distdir is populated with the pre-fetched archives so that Bazel can
    # resolve its implicit DEFAULT.WORKSPACE.SUFFIX dependencies without
    # network access.  --distdir is Bazel's standard offline mechanism: it
    # looks for files by basename and verifies their SHA-256 before using them.
    preConfigure = ''
      export HOME=$(mktemp -d)
      export CC="${gcc}/bin/gcc"
      export CXX="${gcc}/bin/g++"

      # Bazel may also want a writable TEST_TMPDIR
      export TEST_TMPDIR=$(mktemp -d)

      # ── Populate Bazel distdir ───────────────────────────────────────────
      # Symlink pre-fetched archives with their original basenames so that
      # Bazel's --distdir can find them by name and validate by hash.
      BAZEL_DISTDIR="$HOME/bazel-distdir"
      mkdir -p "$BAZEL_DISTDIR"
      ln -s ${bazelSkylib} "$BAZEL_DISTDIR/bazel-skylib-1.6.1.tar.gz"
      ln -s ${rulesCC} "$BAZEL_DISTDIR/rules_cc-0.0.9.tar.gz"
      ln -s ${rulesPython} "$BAZEL_DISTDIR/rules_python-0.24.0.tar.gz"
      echo "distdir contents:" >&2
      ls -la "$BAZEL_DISTDIR" >&2
    '';

    # ── build ────────────────────────────────────────────────────────────────
    # --remote_executor:         send all actions to nixception
    # --remote_instance_name:    must match nixception's configured instance
    # --spawn_strategy=remote:   bypass Bazel's local sandbox (which conflicts
    #                            with the nix build sandbox)
    # --noremote_local_fallback: fail loudly instead of silently retrying local
    # --remote_download_all:     pull outputs back so we can install them
    # --noenable_bzlmod:         safety belt to ensure pure WORKSPACE mode
    # --distdir:                 pre-fetched archives for offline resolution
    # --verbose_failures:        show full command lines on error
    # --action_env:              forward CC/CXX to action environment so Bazel's
    #                            toolchain detection uses the right compiler
    buildPhase = ''
      runHook preBuild
      BUILD_START=$SECONDS

      bazel build //src:hello-world \
        --remote_executor=grpc://127.0.0.1:50051 \
        --remote_instance_name=main \
        --spawn_strategy=remote \
        --noremote_local_fallback \
        --remote_download_all \
        --noenable_bzlmod \
        --distdir="$BAZEL_DISTDIR" \
        --action_env=CC="${gcc}/bin/gcc" \
        --action_env=CXX="${gcc}/bin/g++" \
        --action_env=PATH \
        --jobs=4 \
        --verbose_failures \
        --subcommands

      BUILD_END=$SECONDS
      echo "buildPhase completed in $((BUILD_END - BUILD_START)) seconds"
      runHook postBuild
    '';

    # ── install & verify ─────────────────────────────────────────────────────
    # Copy the compiled binary to $out and verify it actually runs and
    # produces the expected output.
    installPhase = ''
      runHook preInstall
      mkdir -p $out/bin

      cp bazel-bin/src/hello-world $out/bin/
      chmod +x $out/bin/hello-world

      echo "Running hello-world binary to verify correctness..."
      output=$($out/bin/hello-world)
      echo "Output: $output"

      if echo "$output" | grep -q "Hello world"; then
        echo "SUCCESS: hello-world produced expected output"
      else
        echo "FAIL: unexpected output from hello-world: $output"
        exit 1
      fi

      echo "Test passed" > $out/result.txt
      runHook postInstall
    '';

    # No separate check phase — the installPhase validates the output.
    dontCheck = true;

    # Bazel manages its own parallelism via --jobs.
    enableParallelBuilding = false;

    # Shut down Bazel's server before the derivation finishes, otherwise the
    # background Java process keeps file handles open and can cause the nix
    # sandbox teardown to hang.
    postInstall = ''
      bazel shutdown || true
    '';

    meta = {
      description = "Integration-test: build a C++ hello-world through Bazel + nixception (remote execution)";
      license = lib.licenses.asl20;
      maintainers = with lib.maintainers; [layus];
      platforms = lib.platforms.linux;
    };
  }
