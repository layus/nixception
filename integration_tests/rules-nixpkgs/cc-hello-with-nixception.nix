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
# We use nixpkgs' bazel_8 build-support infrastructure (bazelPackage) to
# create a vendor-deps fixed-output derivation (FOD).  The FOD runs
# `bazel vendor` with both network access and recursive-nix, so that:
#   1. HTTP archives (BCR modules, rules_nixpkgs release) are downloaded.
#   2. rules_nixpkgs repo rules can call nix-build via the recursive-nix
#      daemon to configure the CC toolchain.
#   3. Nix-store-referencing repos are stripped from the vendor_dir so the
#      FOD stays self-contained (no /nix/store references).
#
# At build time, the vendored HTTP deps are used offline, while the
# nix-built repos (toolchain config) are re-evaluated via recursive-nix.
#
# Build flow
# ──────────
#   1. [FOD]  bazelVendorDeps    – vendor HTTP deps (nix-built repos stripped)
#   2. unpackPhase               – copy the Bazel project into $TMPDIR
#   3. nixceptionStartPhase      – server comes UP
#   4. buildPhase                – bazel build //src:hello-world via remote exec
#   5. installPhase              – copy binary to $out, verify it runs
#   6. exitHook/failureHook      – server comes DOWN
#
# Requirements (nix.conf / NixOS config):
#   experimental-features = nix-command recursive-nix
#   system-features       = recursive-nix
#
{
  nixceptionHook,
  gcc,
  stdenv,
  bazel_8,
  fetchFromGitHub,
  lndir,
  nix,
  cacert,
  callPackage,
  path,
  lib,
}: let
  # ── Bazel Central Registry snapshot ────────────────────────────────────
  registry = fetchFromGitHub {
    owner = "bazelbuild";
    repo = "bazel-central-registry";
    rev = "722299976c97e5191045c8016b7c8532189fc3f6";
    hash = "sha256-hi5BKI94am2LCXD93GBeT0gsODxGeSsd0OrhTwpNAgM=";
  };

  # ── Vendor deps via bazelDerivation (from nixpkgs bazel_8 build-support)
  # This is the low-level component used by bazelPackage internally.
  # We call it directly so we can inject requiredSystemFeatures for
  # recursive-nix (needed by rules_nixpkgs' nix-build calls).
  bazelDerivation = callPackage "${path}/pkgs/by-name/ba/bazel_8/build-support/bazelDerivation.nix" {};

  bazelVendorDeps = bazelDerivation {
    name = "bazel-nixpkgs-hello-vendor-deps";

    src = ./project;

    bazel = bazel_8;
    targets = ["//src:hello-world"];
    command = "vendor";
    inherit registry;

    nativeBuildInputs = [nix cacert];

    commandArgs = ["--vendor_dir=vendor_dir"];

    bazelPreBuild = ''
      mkdir vendor_dir

      # The recursive-nix daemon socket.
      export NIX_REMOTE=unix:///build/.nix-socket

      # SSL certs for nix to fetch from binary caches.
      export NIX_SSL_CERT_FILE=${cacert}/etc/ssl/certs/ca-bundle.crt
      export SSL_CERT_FILE=$NIX_SSL_CERT_FILE
    '';

    bazelPostBuild = ''
      # ── Clean up vendor_dir for FOD compatibility ────────────────────
      # Fixed-output derivations must not reference nix store paths.  The repos
      # created by rules_nixpkgs (via nix-build) contain symlinks and
      # files pointing into /nix/store.  Remove those entire repo dirs
      # so Bazel will re-evaluate them at build time via recursive-nix.
      echo "Removing nix-store-referencing repos from vendor_dir..." >&2
      for repo_dir in vendor_dir/*/; do
        [ -d "$repo_dir" ] || continue
        if find "$repo_dir" -type l -lname '/nix/store/*' -print -quit | grep -q .; then
          echo "  stripping (nix store symlinks): $repo_dir" >&2
          rm -rf "$repo_dir"
        elif grep -rIqm1 '/nix/store/' "$repo_dir" 2>/dev/null; then
          echo "  stripping (nix store references): $repo_dir" >&2
          rm -rf "$repo_dir"
        fi
      done

      # Remove symlinks pointing into the build directory.
      find vendor_dir -type l -lname "$HOME/*" -exec rm '{}' \;
      # Remove broken symlinks.
      find vendor_dir -xtype l -exec rm '{}' \;
      # Remove .marker files that reference the nix store.
      (grep -rI '/nix/store/' vendor_dir --files-with-matches --include="*.marker" --null 2>/dev/null || true) \
        | xargs -0 --no-run-if-empty rm

      echo "Remaining vendor_dir entries:" >&2
      ls vendor_dir/ >&2
    '';

    installPhase = ''
      mkdir -p $out/vendor_dir
      cp -r --reflink=auto vendor_dir/* $out/vendor_dir

      bazel shutdown || true
    '';

    # Do NOT patch shebangs etc. — that would inject nix store references.
    dontFixup = true;

    # Network access (FOD) + nix daemon (recursive-nix).
    requiredSystemFeatures = ["recursive-nix"];
    outputHashMode = "recursive";
    outputHashAlgo = "sha256";
    outputHash = "sha256-f1yj7U+2cOHafTky08gO5gT4IhixIWSEKZAuJ4Hx0X4=";
  };
in
  stdenv.mkDerivation {
    pname = "bazel-nixpkgs-cc-hello-nixception-test";
    version = "0.1.0";

    src = ./project;

    requiredSystemFeatures = ["recursive-nix"];

    nativeBuildInputs = [
      (nixceptionHook.withPackages [gcc])
      bazel_8
      lndir
      nix
    ];

    # ── Bazel environment setup ────────────────────────────────────────────
    preConfigure = ''
      export HOME=$(mktemp -d)
      export TEST_TMPDIR=$(mktemp -d)

      # Recursive-nix daemon for rules_nixpkgs nix-build calls.
      export NIX_REMOTE=unix:///build/.nix-socket

      # SSL certs for nix to fetch from binary caches.
      export NIX_SSL_CERT_FILE=${cacert}/etc/ssl/certs/ca-bundle.crt
      export SSL_CERT_FILE=$NIX_SSL_CERT_FILE

      # ── Populate writable vendor_dir from FOD ───────────────────────
      mkdir vendor_dir
      ${lndir}/bin/lndir -silent ${bazelVendorDeps}/vendor_dir vendor_dir

      # Pin only the repos that survived FOD cleanup (HTTP-only repos).
      # Nix-built repos (stripped from vendor_dir) will be re-evaluated
      # at build time via recursive-nix.
      rm -f vendor_dir/VENDOR.bazel
      find vendor_dir -mindepth 1 -maxdepth 1 -type d -printf 'pin("@@%P")\n' > vendor_dir/VENDOR.bazel
      echo "VENDOR.bazel pins:" >&2
      cat vendor_dir/VENDOR.bazel >&2

      echo "vendor_dir top-level entries:" >&2
      ls vendor_dir/ >&2
    '';

    # ── build ──────────────────────────────────────────────────────────────
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
        --vendor_dir=vendor_dir \
        --action_env=PATH \
        --jobs=4 \
        --verbose_failures \
        --subcommands

      BUILD_END=$SECONDS
      echo "buildPhase completed in $((BUILD_END - BUILD_START)) seconds"
      runHook postBuild
    '';

    # ── install & verify ───────────────────────────────────────────────────
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

    dontCheck = true;
    enableParallelBuilding = false;

    postInstall = ''
      bazel shutdown || true
    '';

    meta = {
      description = "Integration-test: build a C++ hello-world through Bazel 8 + rules_nixpkgs + nixception";
      license = lib.licenses.asl20;
      maintainers = with lib.maintainers; [layus];
      platforms = lib.platforms.linux;
    };
  }
