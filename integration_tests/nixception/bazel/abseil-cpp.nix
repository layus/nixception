# Build abseil-cpp via Bazel + rules_nixpkgs + nixception.
#
# Source: layus/abseil-cpp@rules_nixpkgs — a fork that adds bzlmod +
# rules_nixpkgs CC toolchain configuration to upstream abseil-cpp,
# based on the 20260107.1 LTS release.
#
# The fork's MODULE.bazel uses `nix_repo.github` so the FOD only needs
# plain HTTP access (no nix daemon).  CC toolchain configuration lives
# in a module extension (Bazel 8 compatible).
#
# The nixpkgs tarball is fetched by nix_repo.github at analysis time
# via ctx.download_and_extract (bypasses --repository_cache).  We
# pre-download it and expose it through --distdir so the air-gapped
# final build can find it.  Using nix_repo.file + recursive-nix is not
# an option: fixed-output derivations inside recursive-nix have no
# network access.
{
  nixceptionHook,
  gcc,
  bazel_8,
  fetchFromGitHub,
  fetchurl,
  nix,
  cacert,
  callPackage,
  runCommand,
  path,
  lib,
}: let
  bazelPackage =
    callPackage
    "${path}/pkgs/by-name/ba/bazel_8/build-support/bazelPackage.nix"
    {};

  nixpkgsTarball = fetchurl {
    url = "https://github.com/NixOS/nixpkgs/archive/refs/tags/25.11.tar.gz";
    sha256 = "bcc12f1c35344a6b5c1f3319923e6d7317cd5f52ad7126e0a5f32e08cbb0a213";
  };
  nixpkgsDistdir = runCommand "nixpkgs-distdir" {} ''
    mkdir -p $out
    ln -s ${nixpkgsTarball} $out/25.11.tar.gz
  '';

  registry = fetchFromGitHub {
    owner = "bazelbuild";
    repo = "bazel-central-registry";
    rev = "566fb61e2d81fc2ec33fc625566a44d4eb618c68";
    hash = "sha256-vuUK3nP35G1Xndb390PBTu/du0J3PFB5IWEitQ9brnc=";
  };

  src = fetchFromGitHub {
    owner = "layus";
    repo = "abseil-cpp";
    rev = "8d39d9632bcd8261b66093cfb9cc6071f1e3985e";
    hash = "sha256-OSSrh1Ljxa/zlcPtdx5t9+hCsHwwRLYMn1ovQMQWg8A=";
  };
in
  (bazelPackage {
    name = "abseil-cpp-nixception-test";
    inherit src registry;

    targets = ["//..."];
    bazel = bazel_8;

    commandArgs = [
      "--remote_executor=grpc://127.0.0.1:50051"
      "--remote_instance_name=main"
      "--spawn_strategy=remote"
      "--noremote_local_fallback"
      "--remote_download_all"
      "--action_env=PATH"
      "--verbose_failures"
      "--lockfile_mode=off"
    ];

    installPhase = ''
      runHook preInstall

      # --- Headers (mirrors nixpkgs: include/absl/…) ---
      mkdir -p $out/include
      cp -r absl $out/include/absl
      # Keep only header files and strip BUILD / non-header artifacts
      find $out/include -type f \
        ! -name '*.h' ! -name '*.inc' \
        -delete
      find $out/include -type d -empty -delete

      # --- Static libraries (mirrors nixpkgs: lib/libabsl_*.{a,so}) ---
      mkdir -p $out/lib
      find bazel-out/k8-fastbuild/bin/absl -name 'lib*.a' \
          -not -path '*test*' -not -path '*benchmark*' \
        | while IFS= read -r src; do
          # src example: bazel-out/k8-fastbuild/bin/absl/log/internal/libcheck_op.a
          rel="''${src#bazel-out/k8-fastbuild/bin/absl/}"   # log/internal/libcheck_op.a
          dir="$(dirname "$rel")"                            # log/internal
          base="$(basename "$rel")"                          # libcheck_op.a
          name="''${base#lib}"                               # check_op.a
          name="''${name%.a}"                                # check_op

          # Build a CMake-style name: libabsl_{pkg}_{name}.a
          # When the last directory component equals the library name,
          # drop it to avoid duplication (e.g. absl/base/libbase.a → libabsl_base.a).
          pkg="$(echo "$dir" | tr '/' '_')"                  # log_internal
          last="''${dir##*/}"                                # internal (or base, strings…)

          if [ "$last" = "$name" ]; then
            dst="libabsl_''${pkg}.a"
          else
            dst="libabsl_''${pkg}_''${name}.a"
          fi

          cp "$src" "$out/lib/$dst"
        done

      runHook postInstall
    '';

    bazelRepoCacheFOD = {
      outputHash = "sha256-7mPbhXW1BLFTNtk03ZDz1JpSaEK/tB+xO2Z4aaZc0Ms=";
      outputHashAlgo = "sha256";
    };
  }).overrideAttrs (old: {
    requiredSystemFeatures = ["recursive-nix"];

    # These flags live in .bazelrc.user so they only affect the final
    # build, not the FOD.  The FOD must not resolve the nixpkgs CC
    # toolchain (which calls nix-build) because it has no nix daemon.
    postPatch =
      (old.postPatch or "")
      + ''
        # Bazel 8 errors on stale lockfiles from a different version.
        rm -f MODULE.bazel.lock

        cat >> .bazelrc.user <<EOF
        build --host_platform=@rules_nixpkgs_core//platforms:host
        build --extra_execution_platforms=@rules_nixpkgs_core//platforms:host
        build --distdir=${nixpkgsDistdir}
        EOF
      '';

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
        NIX_REMOTE = "unix:///build/.nix-socket";
        NIX_SSL_CERT_FILE = "${cacert}/etc/ssl/certs/ca-bundle.crt";
        SSL_CERT_FILE = "${cacert}/etc/ssl/certs/ca-bundle.crt";
      };

    meta = {
      description = "Integration test: build abseil-cpp //... via Bazel 8 + rules_nixpkgs + nixception";
      license = lib.licenses.asl20;
      maintainers = with lib.maintainers; [layus];
      platforms = lib.platforms.linux;
    };
  })
