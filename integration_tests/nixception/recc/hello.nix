# integration_tests/nixception/recc/hello.nix
#
# Integration test: build GNU Hello via recc + nixception.
#
# This is a more ambitious integration test than recc-recursive-nix-test.
# Instead of a tiny hand-rolled C++ project, it builds the real GNU Hello
# package (fetched from upstream), exercising the full autoconf/automake build
# pipeline with all compilations dispatched as remote actions through recc and
# nixception.
#
# Build flow
# ──────────
#   1. unpackPhase    – untar hello-2.12.2.tar.gz
#   2. patchPhase     – (none on Linux)
#   3. configurePhase – ./configure --prefix=$out   ← uses stdenv's gcc
#   4. nixceptionStartPhase (preBuildPhase)         ← server comes UP here
#   5. buildPhase     – make CC=recc-gcc            ← all .c → .o via recc
#   6. checkPhase     – make check                  ← runs hello binary
#   7. installPhase   – make install                ← copies to $out
#   8. exitHook / failureHook                       ← server comes DOWN here
#
# Important: CC is passed via makeFlags, NOT as a derivation env-var.
# ───────────────────────────────────────────────────────────────────
# nixceptionStartPhase is a preBuildPhase, meaning it runs *after*
# configurePhase.  If we exported CC=recc-gcc as an environment variable,
# autoconf's ./configure would try to compile its small feature-detection
# programs through recc before the nixception server is up – causing
# connection-refused errors.  Putting CC only in makeFlags confines the recc
# interception to buildPhase (and later), when nixception is already
# listening.
#
# Requirements (nix.conf / NixOS config):
#   experimental-features = nix-command recursive-nix
#   system-features       = recursive-nix
#
# Called from the top-level flake, e.g.:
#
#   hello-nixception-recc-test = pkgs.callPackage
#     integration_tests/nixception/recc/hello.nix {
#       inherit nixceptionHook buildbox;
#       inherit (pkgs) gcc fetchurl;
#     };
#
{
  nixceptionHook,
  buildbox, # provides the `recc` binary
  gcc,
  stdenv,
  fetchurl,
  writeShellScriptBin,
}: let
  # ── recc wrapper ───────────────────────────────────────────────────────────
  # Wraps `recc <compiler>` in a single-word executable so make does not choke
  # on a two-word CC value (make splits unquoted whitespace in variables).
  reccGcc = writeShellScriptBin "recc-gcc" ''
    exec ${buildbox}/bin/recc ${gcc}/bin/gcc "$@"
  '';
in
  stdenv.mkDerivation {
    pname = "hello";
    version = "2.12.2";

    # GNU Hello upstream tarball – same source as nixpkgs' hello package.
    src = fetchurl {
      url = "mirror://gnu/hello/hello-2.12.2.tar.gz";
      hash = "sha256-WpqZbcKSzCTc9BHO6H6S9qrluNE72caBm0x6nc4IGKs=";
    };

    # ── recursive-nix ────────────────────────────────────────────────────────
    # Exposes the Nix daemon socket inside the build sandbox at
    # /build/.nix-socket so nixception can use the local Nix store as its
    # remote-execution backend.
    requiredSystemFeatures = ["recursive-nix"];

    # ── nixception setup hook ────────────────────────────────────────────────
    # gcc is injected into the runner sandbox so remote actions can find it
    # on PATH.  The hook registers nixceptionStartPhase as a preBuildPhase
    # and tears the server down via exitHook / failureHook.
    nativeBuildInputs = [(nixceptionHook.withPackages [gcc])];

    # ── parallel builds ───────────────────────────────────────────────────────
    # Each compilation is an independent remote action; running them in
    # parallel exercises recc's concurrent submission path.
    enableParallelBuilding = true;

    # ── compiler override (build phase only) ─────────────────────────────────
    # Passed to every `make` invocation by stdenv (buildPhase, checkPhase,
    # installPhase).  make command-line variables override Makefile definitions,
    # so the autoconf-generated $(CC) is replaced by our recc wrapper for all
    # compilation steps.  This does NOT affect configurePhase, which runs
    # before nixceptionStartPhase and must use the default stdenv gcc.
    makeFlags = [
      "CC=${reccGcc}/bin/recc-gcc"
    ];

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
    # preBuild / postBuild run in the same shell as buildPhase, so
    # BUILD_START set here is visible in postBuild.
    preBuild = "BUILD_START=$SECONDS";
    postBuild = ''
      BUILD_END=$SECONDS
      echo "buildPhase completed in $((BUILD_END - BUILD_START)) seconds"
    '';

    # ── check ─────────────────────────────────────────────────────────────────
    # hello's `make check` runs the compiled binary and validates its output.
    # nixception is still up at this point (torn down in exitHook, after
    # installPhase), so any linking or minor checks that go through make also
    # work.
    doCheck = true;
  }
