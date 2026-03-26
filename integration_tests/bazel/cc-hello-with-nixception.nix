# integration_tests/bazel/cc-hello-with-nixception.nix
#
# Integration test: build a minimal C++ project via Bazel 8 + nixception.
#
# This test exercises the full REAPI round-trip:
#   Bazel → gRPC → nixception → recursive-nix daemon → nix build
#
# Bazel natively speaks the Remote Execution API, so no recc/buildbox
# intermediary is needed.  All compilation and linking actions are dispatched
# to nixception via --remote_executor, which in turn creates nix derivations
# for each action and builds them through the recursive-nix daemon socket.
#
# Dependency fetching
# ───────────────────
# Rather than manually declaring fetchurl for each implicit Bazel dependency,
# we follow the pattern from nixpkgs' pkgs/by-name/ba/bazel_8/build-support/:
#
#   1. Pin a snapshot of the Bazel Central Registry (BCR) via fetchFromGitHub.
#   2. Create a fixed-output derivation (FOD) that runs `bazel fetch` with
#      network access, populating a --repository_cache.
#   3. In the actual (sandboxed) build, symlink the repo cache so Bazel
#      resolves all modules offline.
#
# The project uses MODULE.bazel (bzlmod) — no WORKSPACE file needed.
#
# Build flow
# ──────────
#   1. [FOD]  bazelRepoCache   – fetch external deps into repository_cache
#   2. unpackPhase              – copy the minimal Bazel project into $TMPDIR
#   3. nixceptionStartPhase     – (preConfigurePhase) server comes UP
#   4. configurePhase           – (no-op, but the hook runs before it)
#   5. buildPhase               – bazel build //src:hello-world via remote exec
#   6. installPhase             – copy binary to $out, verify it runs
#   7. exitHook/failureHook     – server comes DOWN
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
  bazel_8,
  fetchFromGitHub,
  lndir,
  lib,
}: let
  # ── Bazel Central Registry snapshot ────────────────────────────────────
  # Pin a specific BCR revision so module resolution is fully reproducible.
  # This same rev is used by the bazel_8 examples in nixpkgs.
  registry = fetchFromGitHub {
    owner = "bazelbuild";
    repo = "bazel-central-registry";
    rev = "722299976c97e5191045c8016b7c8532189fc3f6";
    hash = "sha256-hi5BKI94am2LCXD93GBeT0gsODxGeSsd0OrhTwpNAgM=";
  };

  # ── Fixed-output derivation: repository cache ─────────────────────────
  # Runs `bazel fetch` with network access (allowed for fixed-output derivations) to download
  # all external modules declared in MODULE.bazel into a repository_cache.
  # The cache is content-addressed, so the hash is stable across builds.
  #
  # This is the standard nixpkgs pattern for offline Bazel builds.
  # See: pkgs/by-name/ba/bazel_8/build-support/bazelPackage.nix
  bazelRepoCache = stdenv.mkDerivation {
    name = "bazel-hello-world-repo-cache";

    src = ./project;

    nativeBuildInputs = [bazel_8];

    buildPhase = ''
      runHook preBuild

      export HOME=$(mktemp -d)
      mkdir -p "$HOME/repo_cache"

      bazel --batch fetch \
        --registry=file://${registry} \
        --repository_cache="$HOME/repo_cache" \
        //src:hello-world

      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall

      mkdir -p $out/repo_cache
      cp -r --reflink=auto "$HOME/repo_cache"/* $out/repo_cache

      bazel shutdown || true

      runHook postInstall
    '';

    outputHashMode = "recursive";
    outputHashAlgo = "sha256";
    outputHash = "sha256-Tcf0QP5DXwGb5z0vGlKH9rFcFVcIIjzTLh3SKFA52Ak=";
  };
in
  stdenv.mkDerivation {
    pname = "bazel-cc-hello-nixception-test";
    version = "0.1.0";

    src = ./project;

    # ── recursive-nix ──────────────────────────────────────────────────────
    # Exposes the Nix daemon socket at /build/.nix-socket inside the sandbox
    # so nixception can use the local Nix store as its remote-execution backend.
    requiredSystemFeatures = ["recursive-nix"];

    # ── nixception setup hook ──────────────────────────────────────────────
    # gcc is injected into the runner sandbox so remote compilation/linking
    # actions can find it.  The hook registers nixceptionStartPhase as a
    # preConfigurePhase and tears the server down via exitHook / failureHook.
    #
    # bazel_8 is needed as a build tool to drive the build.
    nativeBuildInputs = [
      (nixceptionHook.withPackages [gcc])
      bazel_8
      lndir
    ];

    # ── Bazel environment setup ────────────────────────────────────────────
    # Bazel needs a writable HOME for its output base, caches, and internal
    # state.  Inside the nix sandbox $HOME may not exist or be writable.
    # We also explicitly set CC/CXX so Bazel's auto-detected C++ toolchain
    # matches what is available in the nixception runner sandbox.
    #
    # The repo cache is symlinked (via lndir) so that Bazel can write marker
    # files while still reading the pre-fetched content from the Nix store.
    preConfigure = ''
      export HOME=$(mktemp -d)
      export CC="${gcc}/bin/gcc"
      export CXX="${gcc}/bin/g++"

      # Bazel may also want a writable TEST_TMPDIR
      export TEST_TMPDIR=$(mktemp -d)

      # ── Populate writable repo cache from FOD ───────────────────────────
      mkdir repo_cache
      lndir -silent ${bazelRepoCache}/repo_cache repo_cache
      echo "repo_cache contents:" >&2
      find repo_cache -maxdepth 2 -type f | head -20 >&2
    '';

    # ── build ──────────────────────────────────────────────────────────────
    # --remote_executor:         send all actions to nixception
    # --remote_instance_name:    must match nixception's configured instance
    # --spawn_strategy=remote:   bypass Bazel's local sandbox (which conflicts
    #                            with the nix build sandbox)
    # --noremote_local_fallback: fail loudly instead of silently retrying local
    # --remote_download_all:     pull outputs back so we can install them
    # --registry:                use pinned BCR snapshot for module resolution
    # --repository_cache:        pre-fetched deps for offline build
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
        --registry=file://${registry} \
        --repository_cache=repo_cache \
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

    # ── install & verify ───────────────────────────────────────────────────
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
      description = "Integration-test: build a C++ hello-world through Bazel 8 + nixception (remote execution)";
      license = lib.licenses.asl20;
      maintainers = with lib.maintainers; [layus];
      platforms = lib.platforms.linux;
    };
  }
