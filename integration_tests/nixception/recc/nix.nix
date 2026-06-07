# integration_tests/nixception/recc/nix.nix
#
# Integration test: build the *entire* Nix package through recc + nixception.
#
# This is a full override of the nixpkgs `nix` package, not a single module.
#
# Why an override of the whole package set
# ────────────────────────────────────────
# The modular Nix package is not one derivation but a *scope* of ~20 meson
# components (nix-util, nix-store, nix-fetchers, nix-expr, nix-flake, nix-main,
# nix-cmd, nix-cli and their `-c` C-API siblings).  The top-level `nix`
# derivation (`nix-everything`) compiles nothing itself — it merely `lndir`s
# the already-built components together.  So overriding a single component
# (e.g. `nix.libs.nix-util`) only routes that one library through recc.
#
# To send *every* compilation through recc + nixception we override the whole
# scope.  The `nix` package exposes `passthru.overrideAllMesonComponents`,
# which applies an overlay-shaped function `finalAttrs: prevAttrs: { … }` to
# *each* component derivation and returns the reassembled package.  We build
# its `.nix-cli` output: the `nix` executable links against every Nix library,
# so realising it forces all of nix-util, nix-store, nix-expr, nix-fetchers,
# nix-flake, nix-main, nix-cmd, nix-cli (+ their `-c` variants) to be compiled
# — the complete Nix C++ codebase — with each translation unit dispatched as a
# remote action.  (We target `.nix-cli` rather than the full `nix-everything`
# so the build does not also pull in and *run* the unit + functional test
# suites, which are unrelated to validating remote compilation.)
#
# Build system: meson (per component)
# ───────────────────────────────────
# meson has no native compiler-launcher mechanism (unlike CMake's
# CMAKE_<LANG>_COMPILER_LAUNCHER used by the spdlog test).  Instead we wrap the
# compiler directly: meson shlex-splits the CC / CXX environment variables, so
# the multi-word value "recc g++" parses as [recc, g++] — exactly recc's own
# calling convention (`recc <compiler> <args…>`).  The compiler itself stays
# the stdenv gcc-wrapper picked up from $CC/$CXX, so NIX_CFLAGS_COMPILE-driven
# -isystem flags keep working.  meson bakes the compiler in at configure time,
# so recc is in effect for both the configure-time probes and the real build;
# the nixception hook starts the server in a preConfigurePhase, before meson
# probes the compiler.
#
# Reproducing local compiles on the remote
# ─────────────────────────────────────────
# recc treats /nix/store include paths as "global" and does not upload them —
# they must already exist in the runner sandbox.  Two things ensure the remote
# compilations match the local ones, per component:
#
#   1. RECC_ENV_TO_READ is populated with every NIX_* variable in that
#      component's build environment, so recc forwards the gcc-wrapper's
#      include / link flags (NIX_CFLAGS_COMPILE, NIX_CC_WRAPPER_TARGET_HOST_…,
#      …) to the remote.
#
#   2. nixceptionHook.withPackages injects, for that component, the stdenv
#      compiler plus the dev output of every (propagated) build input into the
#      runner sandbox, so those /nix/store paths are present remotely.
#
# Build type (and cache decoupling)
# ─────────────────────────────────
# We need two things from the build type:
#
#   * LTO off — the upstream `release` build enables `-Db_lto`, under which the
#     per-TU compile only emits GIMPLE bytecode and all real codegen is
#     deferred to the link step, which recc runs *locally*.  That makes the
#     remote actions trivial and defeats the point of the test.
#
#   * No debug info — with a `debug*` build type (`-g`), the compiled `.o`
#     embeds the full `-isystem /nix/store/…-dev/include` header paths in its
#     DWARF.  Nix then scans those strings and records a *runtime* reference
#     from every cached `reapi-action` output to all the `nix-*-dev` (and thus
#     `nix-*` lib) outputs.  The cache's closure becomes the whole component
#     closure, so the meson components can never be garbage-collected while the
#     recc cache is kept.
#
# `mesonBuildType = "plain"` satisfies both: the meson layer only enables LTO
# for `release`/`minsize`, and `plain` passes no `-g`.  Real codegen still
# happens in each remote compile, but the resulting objects contain no
# `/nix/store/…-dev` strings, so the cache is decoupled from the component
# closure and the libraries can be GC'd independently of the cache.
# `separateDebugInfo = false` drops the (now-empty) `-debug` output.
#
# Requirements (nix.conf / NixOS config):
#   experimental-features = nix-command recursive-nix
#   system-features       = recursive-nix
#
# Called from the top-level flake, e.g.:
#
#   recc-nix = pkgs.callPackage integration_tests/nixception/recc/nix.nix {
#     inherit nixceptionHook;
#     inherit (pkgs) buildbox;
#   };
#
{
  nixceptionHook,
  buildbox, # provides the `recc` binary
  nix, # the nixpkgs nix package (the whole component scope, via passthru)
  stdenv,
  lib,
}: let
  # Overlay applied to *every* meson component of the Nix package set.
  # Receives the usual (finalAttrs: prevAttrs) pair; we only read prevAttrs so
  # there is no fixpoint recursion.
  reccOverlay = _finalAttrs: prevAttrs: {
    # ── recursive-nix ─────────────────────────────────────────────────────
    # Exposes the Nix daemon socket inside each component's build sandbox so
    # nixception can use the local Nix store as its remote-execution backend.
    requiredSystemFeatures = (prevAttrs.requiredSystemFeatures or []) ++ ["recursive-nix"];

    # ── nixception hook + per-component runner sandbox ────────────────────
    # The hook starts/stops the nixception server.  withPackages additionally
    # injects, for this specific component, the compiler and the dev output of
    # every dependency into the runner sandbox so the remote compilations find
    # their headers (recc does not upload /nix/store "global" paths — they must
    # already exist remotely).
    nativeBuildInputs =
      (prevAttrs.nativeBuildInputs or [])
      ++ [
        (nixceptionHook.withPackages (
          [stdenv.cc]
          ++ map lib.getDev (
            lib.filter (x: x != null) (
              (prevAttrs.buildInputs or [])
              ++ (prevAttrs.propagatedBuildInputs or [])
            )
          )
        ))
      ];

    # ── build type: LTO off + no debug info ───────────────────────────────
    # "plain" disables -Db_lto (so remote actions do real codegen) AND emits
    # no -g, so cached .o objects don't embed -dev header paths — this keeps
    # the recc cache's runtime closure independent of the nix-* components,
    # allowing the components to be GC'd while the cache is retained.
    mesonBuildType = "plain";
    # Drop the (now-empty, since no -g) separate debug output.
    separateDebugInfo = false;

    # ── recc environment ──────────────────────────────────────────────────
    # recc reads these to locate the remote-execution, CAS, and action-cache
    # endpoints served by nixception.
    #RECC_VERBOSE = "1";
    #RECC_LOG_PROGRESS = "1";
    RECC_INSTANCE = "main";
    RECC_SERVER = "127.0.0.1:50051";
    RECC_CAS_SERVER = "127.0.0.1:50051";
    RECC_ACTION_CACHE_SERVER = "127.0.0.1:50051";
    RECC_PROJECT_ROOT = "/build";

    # ── recc wiring (runs last, after all meson-layer preConfigures) ──────
    preConfigure =
      (prevAttrs.preConfigure or "")
      + ''
        # Route all C/C++ compilation through recc.  meson shlex-splits
        # CC/CXX, so "recc g++" becomes [recc, g++] — recc's native form.
        export CC="${buildbox}/bin/recc $CC"
        export CXX="${buildbox}/bin/recc $CXX"

        # Forward every NIX_* variable so the gcc-wrapper reproduces the same
        # include / link flags inside the runner sandbox.
        nix_vars=$(env | sed -n 's/^\(NIX_[^=]*\)=.*/\1/p' | sort -u | tr '\n' ',')
        export RECC_ENV_TO_READ="PATH,SOURCE_DATE_EPOCH,''${nix_vars%,}"
        echo "nixception: RECC_ENV_TO_READ=$RECC_ENV_TO_READ" >&2
      '';
  };

  # The whole Nix component scope, with every meson component routed through
  # recc + nixception.  `.nix-cli` is the `nix` executable; building it pulls
  # in (and compiles, remotely) every Nix library.
  reccNix = nix.overrideAllMesonComponents reccOverlay;
in
  reccNix.nix-cli.overrideAttrs (old: {
    pname = "nix-cli-recc-test";

    # Build timing for CI visibility (covers the CLI link/compile; each library
    # component is built — and timed in its own log — as a separate derivation).
    preBuild = (old.preBuild or "") + "BUILD_START=$SECONDS";
    postBuild =
      (old.postBuild or "")
      + ''
        BUILD_END=$SECONDS
        echo "buildPhase completed in $((BUILD_END - BUILD_START)) seconds"
      '';

    meta =
      (old.meta or {})
      // {
        description = "Integration-test: build the whole Nix package through recc + nixception";
      };
  })
