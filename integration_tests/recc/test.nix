# Recursive Nix integration test for nixception + recc.
#
# This test uses the `recursive-nix` experimental feature so that the Nix
# daemon socket is available inside the build sandbox.  nixception connects
# to that socket (its NixStore backend) and serves the Remote Execution API,
# while recc acts as a client that submits a small C++ compilation.
#
# CC and CXX are exported in preConfigure so they take effect after stdenv's
# cc-wrapper setup hook (which unconditionally sets CC=gcc / CXX=g++).  The
# multi-word value "recc g++" follows the same pattern as "ccache g++": the
# shell word-splits it so recc is invoked with g++ as its first argument.
# The Makefile uses ?= for CC so the environment variable takes precedence.
# CXX is derived from CC by the Makefile when not explicitly set, but we set
# both for clarity.
#
# Requirements (nix.conf / NixOS config):
#   experimental-features = nix-command recursive-nix
#   system-features       = recursive-nix
#
# The derivation is meant to be called from the top-level flake, e.g.:
#
#   recc-recursive-nix-test = pkgs.callPackage integration_tests/recc/test.nix {
#     inherit nixceptionHook buildbox;
#     inherit (pkgs) gcc;
#   };
#
{
  nixceptionHook,
  buildbox,
  gcc,
  stdenv,
  writeShellScriptBin,
}: let
  # A wrapper whose bin/g++ sleeps for 10 s before calling the real compiler,
  # making non-cached remote runs noticeably slower than cached ones.
  # Dependency-scanning invocations (-M / -MM / -MF / -MD / -MMD) are exempted
  # because recc runs those locally and they don't exercise remote execution.
  gppSleeper = writeShellScriptBin "g++" ''
    case " $* " in
      *\ -M\ *|*\ -MM\ *|*\ -MF\ *|*\ -MD\ *|*\ -MMD\ *)
        ;;
      *)
        sleep 10
        ;;
    esac
    exec ${gcc}/bin/g++ "$@"
  '';
in
  stdenv.mkDerivation {
    name = "recc-recursive-nix-test";

    # ── recursive-nix ───────────────────────────────────────────────────
    # This is the key ingredient: it tells the Nix daemon to expose its
    # Unix socket inside the build sandbox so that nixception can talk to
    # the local Nix store.
    requiredSystemFeatures = ["recursive-nix"];

    # The only source we need is the tiny C++ test files.
    src = ./test;

    # nixceptionHook.withPackages injects gppSleeper into the runner sandbox so
    # remote compilations go through the sleep wrapper, making uncached runs
    # visibly slower than cached ones.
    nativeBuildInputs = [(nixceptionHook.withPackages [gppSleeper])];

    # ── parallel builds ──────────────────────────────────────────────────
    # Lets stdenv pass -j${NIX_BUILD_CORES} to make automatically.
    enableParallelBuilding = true;

    # ── compiler override ────────────────────────────────────────────────
    # stdenv's cc-wrapper setup hook unconditionally exports CC=gcc / CXX=g++,
    # so we must re-export after it.  preConfigure runs inside configurePhase,
    # after all setup hooks have been sourced.  The multi-word value causes
    # the shell to run `recc g++ …` — the same pattern as `ccache g++`.
    # The Makefile uses ?= for CC/CXX so these environment variables take
    # precedence over the defaults.
    preConfigure = ''
      export CC="${buildbox}/bin/recc ${gppSleeper}/bin/g++"
      export CXX="${buildbox}/bin/recc ${gppSleeper}/bin/g++"
    '';

    # ── preprocessor flags ───────────────────────────────────────────────
    # CPPFLAGS is not touched by any setup hook, so makeFlags is fine here.
    # make command-line variables override Makefile definitions, ensuring
    # the -D flag reaches every compilation unit.
    makeFlags = [
      "CPPFLAGS=-DBUILD_CONSTANT=42"
    ];

    # ── recc environment ─────────────────────────────────────────────────
    # These become environment variables in the build sandbox.  recc reads
    # them to locate the remote execution, CAS, and action-cache endpoints.
    # All three must point at nixception; without the explicit CAS / action-
    # cache overrides recc defaults to localhost:8085.
    RECC_VERBOSE = "1";
    RECC_LOG_PROGRESS = "1";
    RECC_INSTANCE = "main";
    RECC_SERVER = "127.0.0.1:50051";
    RECC_CAS_SERVER = "127.0.0.1:50051";
    RECC_ACTION_CACHE_SERVER = "127.0.0.1:50051";

    # ── build timing ─────────────────────────────────────────────────────
    # preBuild and postBuild run in the same shell as buildPhase, so
    # BUILD_START set here is visible in postBuild.
    preBuild = "BUILD_START=$SECONDS";
    postBuild = ''
      BUILD_END=$SECONDS
      echo "buildPhase completed in $((BUILD_END - BUILD_START)) seconds"
    '';

    # ── check ────────────────────────────────────────────────────────────
    # stdenv's checkPhase auto-discovers the `test` Makefile target and runs
    # it after buildPhase.  The nixception server is still up at this point
    # (it is stopped in postPhases, after installPhase).
    doCheck = true;

    # ── install ──────────────────────────────────────────────────────────
    # Copy the compiled binary into $out.  Logs are streamed to the Nix build
    # log via ts (inside the hook) so the output is fully deterministic.
    installPhase = ''
      runHook preInstall
      mkdir -p $out
      cp demo_app $out/
      echo "Test passed" > $out/result.txt
      runHook postInstall
    '';
  }
