# integration_tests/recc/zlib-with-nixception.nix
#
# Integration test: build zlib via recc + nixception.
#
# Based on the nixpkgs zlib derivation (pkgs/development/libraries/zlib/default.nix),
# stripped to Linux essentials and extended with recc remote execution through
# a nixception server.
#
# Build flow
# ──────────
#   1. unpackPhase    – untar zlib-1.3.1.tar.gz
#   2. patchPhase     – (none on Linux)
#   3. configurePhase – ./configure --prefix=$out --static --shared  ← stdenv gcc
#   4. nixceptionStartPhase (preBuildPhase)                          ← server UP
#   5. buildPhase     – make CC=recc-gcc                             ← .c → .o via recc
#   6. checkPhase     – make check                                   ← runs test binaries
#   7. installPhase   – make install                                 ← copies to $out
#   8. exitHook / failureHook                                        ← server DOWN
#
# Important: CC is passed via makeFlags, NOT as a derivation env-var.
# ───────────────────────────────────────────────────────────────────
# nixceptionStartPhase is a preBuildPhase, meaning it runs *after*
# configurePhase.  If we exported CC=recc-gcc as an environment variable,
# zlib's ./configure would try to compile its feature-detection programs
# through recc before the nixception server is up, causing connection-refused
# errors.  Putting CC only in makeFlags confines recc to buildPhase (and
# beyond), when nixception is already listening.
#
# Remote vs local actions
# ───────────────────────
# recc detects whether an invocation is a compilation step by looking for the
# -c flag.  Compilation steps (.c → .o) are dispatched remotely; link steps
# (e.g. gcc -shared -o libz.so ...) and archiver invocations (ar) run locally.
# This is the standard recc behaviour and works fine here.
#
# Requirements (nix.conf / NixOS config):
#   experimental-features = nix-command recursive-nix
#   system-features       = recursive-nix
#
# Called from the top-level flake, e.g.:
#
#   zlib-nixception-recc-test = pkgs.callPackage
#     integration_tests/recc/zlib-with-nixception.nix {
#       inherit nixceptionHook buildbox;
#     };
#
{
  nixceptionHook,
  buildbox, # provides the `recc` binary
  gcc,
  stdenv,
  fetchurl,
  writeShellScriptBin,
}:
let
  # ── recc wrapper ───────────────────────────────────────────────────────────
  # Wraps `recc <compiler>` in a single-word executable so make does not choke
  # on a two-word CC value (make splits unquoted whitespace in variables).
  reccGcc = writeShellScriptBin "recc-gcc" ''
    exec ${buildbox}/bin/recc ${gcc}/bin/gcc "$@"
  '';
in
  stdenv.mkDerivation (finalAttrs: {
    pname = "zlib";
    version = "1.3.1";

    src = fetchurl {
      urls = [
        "https://github.com/madler/zlib/releases/download/v${finalAttrs.version}/zlib-${finalAttrs.version}.tar.gz"
        "https://www.zlib.net/fossils/zlib-${finalAttrs.version}.tar.gz"
      ];
      hash = "sha256-mpOyt9/ax3zrpaVYpYDnRmfdb+3kWFuR7vtg8Dty3yM=";
    };

    # ── recursive-nix ────────────────────────────────────────────────────────
    # Exposes the Nix daemon socket at /build/.nix-socket inside the sandbox
    # so nixception can use the local Nix store as its remote-execution backend.
    requiredSystemFeatures = ["recursive-nix"];

    # ── nixception setup hook ────────────────────────────────────────────────
    # gcc is injected into the runner sandbox so remote compilation actions
    # can invoke it by its full store path.  The hook registers
    # nixceptionStartPhase as a preBuildPhase and tears the server down via
    # exitHook / failureHook.
    nativeBuildInputs = [(nixceptionHook.withPackages [gcc])];

    strictDeps = true;

    # ── outputs ───────────────────────────────────────────────────────────────
    # Mirrors the nixpkgs split: headers / pkg-config go to `dev`, the
    # libraries and man page go to `out`.  No separate `static` output – the
    # .a stays in `out` to keep the test derivation simple.
    outputs = [
      "out"
      "dev"
    ];
    setOutputFlags = false;
    outputDoc = "dev";

    # ── configure ─────────────────────────────────────────────────────────────
    # Build the static library only.  zlib's shared-library build stages
    # position-independent objects under an objs/ subdirectory, which does not
    # exist in the fresh remote-execution sandbox that nixception creates for
    # each action.  The static build puts all .o files directly in the working
    # directory, which is always available, so every compilation succeeds
    # without any special runner-side directory setup.
    configureFlags = [
      "--static"
      "--shared"
    ];
    # Prevent stdenv from injecting --disable-static (zlib's configure does not
    # understand that flag and it would be silently ignored, but let's be clean).
    dontDisableStatic = true;
    dontAddStaticConfigureFlags = true;

    # ── compiler override (build phase only) ─────────────────────────────────
    # Passed to every `make` invocation by stdenv.  make command-line variables
    # override Makefile definitions, so the configure-detected $(CC) is
    # replaced by our recc wrapper for all compilation steps.
    # PREFIX is the tool prefix used by the Makefile for ar/ranlib/etc.;
    # on native Linux this is empty, matching stdenv.cc.targetPrefix.
    makeFlags = [
      "CC=${reccGcc}/bin/recc-gcc"
      "PREFIX=${stdenv.cc.targetPrefix}"
    ];

    # Avoid a reference to bootstrap-tools libgcc in the installed library.
    # (Kept from the nixpkgs derivation; harmless on stable stdenv builds too.)
    env.NIX_CFLAGS_COMPILE = "-static-libgcc";

    # ── parallel builds ───────────────────────────────────────────────────────
    # Each compilation unit is an independent remote action; running them in
    # parallel exercises recc's concurrent submission path.
    enableParallelBuilding = true;

    # ── check ─────────────────────────────────────────────────────────────────
    # zlib's `make check` compiles and runs example / minigzip test programs.
    # nixception is still up at this point so the compilation of the test
    # programs also goes through recc.
    doCheck = true;

    # ── recc environment ──────────────────────────────────────────────────────
    # recc reads these to locate the remote-execution, CAS, and action-cache
    # endpoints served by nixception.
    RECC_VERBOSE = "1";
    RECC_LOG_PROGRESS = "1";
    RECC_INSTANCE = "main";
    RECC_SERVER = "127.0.0.1:50051";
    RECC_CAS_SERVER = "127.0.0.1:50051";
    RECC_ACTION_CACHE_SERVER = "127.0.0.1:50051";

    # ── build timing ─────────────────────────────────────────────────────────
    preBuild = "BUILD_START=$SECONDS";
    postBuild = ''
      BUILD_END=$SECONDS
      echo "buildPhase completed in $((BUILD_END - BUILD_START)) seconds"
    '';
  })
