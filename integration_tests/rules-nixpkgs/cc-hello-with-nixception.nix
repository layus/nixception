# integration_tests/rules-nixpkgs/cc-hello-with-nixception.nix
#
# Integration test: build a C++ project via Bazel 8 + rules_nixpkgs + nixception.
#
# This test exercises the full REAPI round-trip with a Nix-provided CC
# toolchain:
#   Bazel → gRPC → nixception → recursive-nix daemon → nix build
#
# The CC toolchain is configured by rules_nixpkgs (nixpkgs_cc_configure),
# which calls nix-build to obtain the compiler from nixpkgs.  All actual
# compilation actions are then dispatched to nixception via --remote_executor.
#
# Dependency fetching
# ───────────────────
# We use nixpkgs' bazelPackage (from bazel_8 build-support) to create a
# fixed-output derivation (FOD) that caches all HTTP downloads into a
# Bazel --repository_cache.  The FOD only needs network access — not
# recursive-nix — because `bazel fetch` downloads archives but does NOT
# evaluate repo rules like nixpkgs_cc_configure.  Those rules are
# evaluated lazily at build time, when recursive-nix is available.
#
# Build flow
# ──────────
#   1. [FOD]  bazelRepoCache     – fetch HTTP deps into repository_cache
#   2. unpackPhase               – copy the Bazel project into $TMPDIR
#   3. preBuildPhase             – symlink repo cache for offline resolution
#   4. nixceptionStartPhase      – server comes UP
#   5. buildPhase                – bazel build //src:hello-world via remote exec
#   6. installPhase              – copy binary to $out, verify it runs
#   7. exitHook/failureHook      – server comes DOWN
#
# Requirements (nix.conf / NixOS config):
#   experimental-features = nix-command recursive-nix
#   system-features       = recursive-nix
#
# Called from the top-level flake, e.g.:
#
#   bazel-nixpkgs-cc-hello-nixception-test = pkgs.callPackage
#     integration_tests/rules-nixpkgs/cc-hello-with-nixception.nix {
#       inherit nixceptionHook;
#     };
#
{
  nixceptionHook,
  gcc,
  bazel_8,
  fetchFromGitHub,
  nix,
  cacert,
  callPackage,
  path,
  lib,
}: let
  # ── bazelPackage from nixpkgs bazel_8 build-support ────────────────────
  # This is the same helper used by the bazel_8 examples in nixpkgs.
  # It handles FOD creation, repo cache / vendor dir setup, and the final
  # bazel build invocation.
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
    name = "bazel-nixpkgs-cc-hello-nixception-test";
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
    # archives (BCR modules, rules_nixpkgs release tarball, etc.) into a
    # content-addressed repository_cache.
    #
    # Crucially, this does NOT evaluate repo rules (like nixpkgs_cc_configure),
    # so no recursive-nix is needed here — just plain network access.
    bazelRepoCacheFOD = {
      outputHash = "sha256-50gtbhmbIw8TyDYsVmwVGNJ7qek5GYf7k0SjMjU3tT4=";
      outputHashAlgo = "sha256";
    };
  }).overrideAttrs (old: {
    # The final build needs recursive-nix so that:
    # 1. rules_nixpkgs can call nix-build to configure the CC toolchain
    # 2. nixception can delegate compilation actions to the nix daemon
    requiredSystemFeatures = ["recursive-nix"];

    # nixceptionHook, nix, and cacert must only be in the final build — not
    # the FOD.  (bazelPackage passes nativeBuildInputs to both, so we add
    # them here via overrideAttrs.)
    nativeBuildInputs =
      (old.nativeBuildInputs or [])
      ++ [
        (nixceptionHook.withPackages [gcc])
        nix
        cacert
      ];

    env =
      (old.env or {})
      // {
        # Recursive-nix daemon socket for rules_nixpkgs nix-build calls.
        NIX_REMOTE = "unix:///build/.nix-socket";
        # SSL certs so nix can fetch from binary caches.
        NIX_SSL_CERT_FILE = "${cacert}/etc/ssl/certs/ca-bundle.crt";
        SSL_CERT_FILE = "${cacert}/etc/ssl/certs/ca-bundle.crt";
      };

    meta = {
      description = "Integration-test: build a C++ hello-world through Bazel 8 + rules_nixpkgs + nixception";
      license = lib.licenses.asl20;
      maintainers = with lib.maintainers; [layus];
      platforms = lib.platforms.linux;
    };
  })
