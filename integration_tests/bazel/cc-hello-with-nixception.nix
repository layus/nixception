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
# We use nixpkgs' bazelPackage (from bazel_8 build-support) to create a
# fixed-output derivation (FOD) that caches all HTTP downloads into a
# Bazel --repository_cache.  The FOD only needs network access — the
# actual build uses the cached archives offline.
#
# The project uses MODULE.bazel (bzlmod) — no WORKSPACE file needed.
#
# Build flow
# ──────────
#   1. [FOD]  bazelRepoCache    – fetch external deps into repository_cache
#   2. unpackPhase              – copy the minimal Bazel project into $TMPDIR
#   3. preBuildPhase            – symlink repo cache for offline resolution
#   4. nixceptionStartPhase     – server comes UP
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
  bazel_8,
  fetchFromGitHub,
  callPackage,
  path,
  lib,
}: let
  # ── bazelPackage from nixpkgs bazel_8 build-support ────────────────────
  # This is the same helper used by the bazel_8 examples in nixpkgs.
  # It handles FOD creation, repo cache setup, and the final bazel build.
  bazelPackage = callPackage "${path}/pkgs/by-name/ba/bazel_8/build-support/bazelPackage.nix" {};

  # ── Bazel Central Registry snapshot ────────────────────────────────────
  # Pin a specific BCR revision so module resolution is fully reproducible.
  registry = fetchFromGitHub {
    owner = "bazelbuild";
    repo = "bazel-central-registry";
    rev = "722299976c97e5191045c8016b7c8532189fc3f6";
    hash = "sha256-hi5BKI94am2LCXD93GBeT0gsODxGeSsd0OrhTwpNAgM=";
  };
in
  (bazelPackage {
    name = "bazel-cc-hello-nixception-test";
    src = ./project;
    inherit registry;

    targets = ["//src:hello-world"];
    bazel = bazel_8;

    # Remote execution flags: send all spawn actions to nixception.
    commandArgs = [
      "--remote_executor=grpc://127.0.0.1:50051"
      "--remote_instance_name=main"
      "--spawn_strategy=remote"
      "--noremote_local_fallback"
      "--remote_download_all"
      "--action_env=CC=${gcc}/bin/gcc"
      "--action_env=CXX=${gcc}/bin/g++"
      "--action_env=PATH"
      "--jobs=4"
      "--verbose_failures"
      "--subcommands"
    ];

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

    # ── Repo cache FOD ──────────────────────────────────────────────────────
    # The FOD runs `bazel fetch` with network access to download all HTTP
    # archives (BCR modules, rules_cc, etc.) into a content-addressed
    # repository_cache.  No recursive-nix needed here.
    bazelRepoCacheFOD = {
      outputHash = "sha256-Tcf0QP5DXwGb5z0vGlKH9rFcFVcIIjzTLh3SKFA52Ak=";
      outputHashAlgo = "sha256";
    };
  }).overrideAttrs (old: {
    # The final build needs recursive-nix so nixception can delegate
    # compilation actions to the nix daemon.
    requiredSystemFeatures = ["recursive-nix"];

    # nixceptionHook and gcc must only be in the final build — not the FOD.
    # (bazelPackage passes nativeBuildInputs to both, so we add them here.)
    nativeBuildInputs =
      (old.nativeBuildInputs or [])
      ++ [
        (nixceptionHook.withPackages [gcc])
      ];

    env =
      (old.env or {})
      // {
        CC = "${gcc}/bin/gcc";
        CXX = "${gcc}/bin/g++";
      };

    meta = {
      description = "Integration-test: build a C++ hello-world through Bazel 8 + nixception (remote execution)";
      license = lib.licenses.asl20;
      maintainers = with lib.maintainers; [layus];
      platforms = lib.platforms.linux;
    };
  })
