# Build abseil-cpp via Bazel + rules_nixpkgs + nixception.
#
# Source: aherrmann/abseil-cpp@bazel-hour — a fork that adds bzlmod +
# rules_nixpkgs CC toolchain configuration to upstream abseil-cpp.
#
# The upstream MODULE.bazel uses `nix_repo.file` which requires a running
# nix daemon at `bazel fetch` time.  We patch it to `nix_repo.github` so
# the FOD only needs plain HTTP access.  We also switch to a module
# extension for `nixpkgs_cc_configure` (required by bazel ≥ 8 where
# WORKSPACE.bzlmod is no longer evaluated).
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
  applyPatches,
  writeText,
}: let
  bazelPackage =
    callPackage
    "${path}/pkgs/by-name/ba/bazel_8/build-support/bazelPackage.nix"
    {};

  nixpkgsTarball = fetchurl {
    url = "https://github.com/NixOS/nixpkgs/archive/refs/tags/24.05.tar.gz";
    sha256 = "911314b81780f26fdaf87e17174210bdbd40c86bac1795212f257cdc236a1e78";
  };
  nixpkgsDistdir = runCommand "nixpkgs-distdir" {} ''
    mkdir -p $out
    ln -s ${nixpkgsTarball} $out/24.05.tar.gz
  '';

  registry = fetchFromGitHub {
    owner = "bazelbuild";
    repo = "bazel-central-registry";
    rev = "722299976c97e5191045c8016b7c8532189fc3f6";
    hash = "sha256-hi5BKI94am2LCXD93GBeT0gsODxGeSsd0OrhTwpNAgM=";
  };

  # Patched MODULE.bazel -------------------------------------------------
  # • nix_repo.file  → nix_repo.github (no nix needed in FOD)
  # • drop nix_pkg / bazel_7 (unused)
  # • CC toolchain via module extension (bazel 8 compat)
  # • bump a few dep versions for bazel 8 / BCR availability
  moduleBazel = writeText "MODULE.bazel" ''
    module(
        name = "abseil-cpp",
        version = "head",
        compatibility_level = 1,
    )

    bazel_dep(name = "rules_nixpkgs_core", version = "0.13.0")

    bazel_dep(name = "rules_nixpkgs_cc", version = "0.13.0")
    archive_override(
        module_name = "rules_nixpkgs_cc",
        integrity = "sha256-MCcfe9OA5OIOTXEywySUbE/bwx6+C7tmOKD2GjfnQ5c=",
        strip_prefix = "rules_nixpkgs-0.13.0/toolchains/cc",
        urls = ["https://github.com/tweag/rules_nixpkgs/releases/download/v0.13.0/rules_nixpkgs-0.13.0.tar.gz"],
    )

    bazel_dep(name = "bazel_skylib", version = "1.7.1")
    bazel_dep(name = "rules_cc", version = "0.1.0")
    bazel_dep(name = "platforms", version = "0.0.10")

    bazel_dep(
        name = "google_benchmark",
        version = "1.8.3",
        repo_name = "com_github_google_benchmark",
        dev_dependency = True,
    )
    bazel_dep(
        name = "googletest",
        version = "1.14.0.bcr.1",
        repo_name = "com_google_googletest",
    )

    nix_repo = use_extension(
        "@rules_nixpkgs_core//extensions:repository.bzl",
        "nix_repo",
    )
    nix_repo.github(
        name = "nixpkgs",
        sha256 = "911314b81780f26fdaf87e17174210bdbd40c86bac1795212f257cdc236a1e78",
        tag = "24.05",
    )
    use_repo(nix_repo, "nixpkgs")

    cc_configure = use_extension("//:extension.bzl", "cc_configure")
    use_repo(cc_configure, "nixpkgs_config_cc")
    use_repo(cc_configure, "nixpkgs_config_cc_toolchains")
    use_repo(cc_configure, "nixpkgs_config_cc_info")

    register_toolchains("@nixpkgs_config_cc_toolchains//:all")
  '';

  # Module extension for nixpkgs CC toolchain ----------------------------
  extensionBzl = writeText "extension.bzl" ''
    """Module extension for configuring the Nixpkgs CC toolchain."""

    load("@rules_nixpkgs_cc//:cc.bzl", "nixpkgs_cc_configure")

    def _cc_configure_impl(_module_ctx):
        nixpkgs_cc_configure(
            name = "nixpkgs_config_cc",
            repository = "@nixpkgs",
            cc_std = "c++14",
            register = False,
        )

    cc_configure = module_extension(
        implementation = _cc_configure_impl,
    )
  '';

  src = applyPatches {
    src = fetchFromGitHub {
      owner = "aherrmann";
      repo = "abseil-cpp";
      rev = "8c43dd6a3e4fcc7c75cd0e5e0e6c7712f821409c";
      hash = "sha256-sQy1Cld3iPqVTAx7t0K4Xg6y/s0Nk6Ewf98UKn2+O4c=";
    };
    postPatch = ''
      cp --no-preserve=mode ${moduleBazel} MODULE.bazel
      cp --no-preserve=mode ${extensionBzl} extension.bzl

      # Remove WORKSPACE.bzlmod — no longer needed, CC config lives in
      # the module extension now.
      rm -f WORKSPACE.bzlmod

      # Bazel 8 errors on stale lockfiles from a different version.
      rm -f MODULE.bazel.lock
    '';
  };
in
  (bazelPackage {
    name = "abseil-cpp-nixception-test";
    inherit src registry;

    targets = ["//absl/..."];
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
      mkdir -p $out
      echo "abseil-cpp //absl/base/... built successfully" > $out/result.txt
      cp --dereference --recursive bazel-out $out/
      runHook postInstall
    '';

    bazelRepoCacheFOD = {
      outputHash = "sha256-jNC0sZY43T1NKO9BX5KDaJP+Gi5uFtCH5PjTG9sbfKU=";
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
      description = "Integration test: build abseil-cpp //absl/base/... via Bazel 8 + rules_nixpkgs + nixception";
      license = lib.licenses.asl20;
      maintainers = with lib.maintainers; [layus];
      platforms = lib.platforms.linux;
    };
  })
