# Recursive Nix integration test for nixception + recc.
#
# This test uses the `recursive-nix` experimental feature so that the Nix
# daemon socket is available inside the build sandbox.  nixception connects
# to that socket (its NixStore backend) and serves the Remote Execution API,
# while recc acts as a client that submits a small C++ compilation.
#
# Requirements (nix.conf / NixOS config):
#   experimental-features = nix-command recursive-nix
#   system-features       = recursive-nix
#
# The derivation is meant to be called from the top-level flake, e.g.:
#
#   recc-recursive-nix-test = pkgs.callPackage integration_tests/recc/test.nix {
#     inherit nixception buildbox wait4x;
#     inherit (pkgs) gcc coreutils moreutils;
#   };
#
{
  nixception,
  buildbox,
  wait4x,
  gcc,
  coreutils,
  moreutils,
  stdenv,
  callPackage,
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

  # Build a custom runner that puts gppSleeper first on PATH so the sleeping
  # g++ is used for remote compilations dispatched by nixception.
  # extraRuntimeInputs are listed first in runtimeInputs, so gppSleeper's g++
  # takes precedence over anything in the built-in defaults.
  runner = callPackage ../../tools/runner.nix {
    extraRuntimeInputs = [gppSleeper];
  };

  # The nixception hook starts the server (with the custom runner) before
  # buildPhase and stops it after installPhase.  We override nixception's
  # default runner here by passing our custom one.
  nixceptionHook = callPackage ../../tools/nixception-hook.nix {
    inherit nixception wait4x moreutils runner;
  };

  # Wrap `recc <compiler>` in a single-word script so make doesn't choke on
  # the space in a two-word CC/CXX value.  The full store path to gppSleeper's
  # g++ is passed so the exact wrapper binary is used both locally and remotely.
  reccGpp = writeShellScriptBin "recc-gpp" ''
    exec ${buildbox}/bin/recc ${gppSleeper}/bin/g++ "$@"
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

    nativeBuildInputs = [
      nixceptionHook
      reccGpp
      buildbox
      gcc
      coreutils
      moreutils
      gppSleeper
    ];

    # ── build ───────────────────────────────────────────────────────────
    # nixceptionHook has already started the server via nixceptionStartPhase
    # (registered in preBuildPhases) and will stop it via nixceptionStopPhase
    # (registered in postPhases).  This phase only needs to drive make.
    buildPhase = ''
      runHook preBuild

      BUILD_START=$SECONDS

      echo "nixception is ready – running make (using recc as driver)"

      # Clean any previous artifacts from earlier runs.
      ${coreutils}/bin/rm -f demo_app *.o

      # Run the project's generic Makefile while pointing CC/CXX to the wrapper.
      # All three recc endpoints are pointed at nixception so recc doesn't fall
      # back to its built-in default of localhost:8085 for the CAS / action cache.
      env \
        RECC_VERBOSE=1 \
        RECC_LOG_PROGRESS=1 \
        RECC_INSTANCE=main \
        RECC_SERVER=127.0.0.1:50051 \
        RECC_CAS_SERVER=127.0.0.1:50051 \
        RECC_ACTION_CACHE_SERVER=127.0.0.1:50051 \
        CC="${reccGpp}/bin/recc-gpp" \
        CXX="${reccGpp}/bin/recc-gpp" \
        CPPFLAGS=-DBUILD_CONSTANT=42 \
        make -j4 test > >(${moreutils}/bin/ts '[make] %H:%M:%.S' >&2) 2>&1

      # ── verify ────────────────────────────────────────────────────────
      if [ ! -f demo_app ]; then
        echo "FAIL: demo_app was not created by make" >&2
        exit 1
      fi

      echo "SUCCESS: demo_app was created by recc via nixception and Makefile"

      BUILD_END=$SECONDS
      echo "buildPhase completed in $((BUILD_END - BUILD_START)) seconds"

      runHook postBuild
    '';

    # ── install ─────────────────────────────────────────────────────────
    # Only the compiled binary goes into $out; logs are streamed to the Nix
    # build log via ts so the output is fully deterministic.
    installPhase = ''
      runHook preInstall

      mkdir -p $out
      cp demo_app $out/
      echo "Test passed" > $out/result.txt

      runHook postInstall
    '';
  }
