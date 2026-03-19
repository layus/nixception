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
#     inherit nixception buildbox;
#     inherit (pkgs) gcc;
#   };
#
{
  nixception,
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

    # nixception carries nixception-hook as a propagatedNativeBuildInput, so
    # the setup hook is sourced automatically – no manual start/stop needed.
    nativeBuildInputs = [nixception];

    # ── custom runner ────────────────────────────────────────────────────
    # nixception.mkRunner builds a runner with the given extraRuntimeInputs
    # and returns a "outPath drvPath" string that the hook reads at build time.
    # gppSleeper is injected so remote compilations go through the sleep wrapper,
    # making uncached runs visibly slower than cached ones.
    nixceptionRunner = nixception.mkRunner [gppSleeper];

    # ── parallel builds ──────────────────────────────────────────────────
    # Lets stdenv pass -j${NIX_BUILD_CORES} to make automatically.
    enableParallelBuilding = true;

    # ── make variable assignments ────────────────────────────────────────
    # Equivalent to cmakeFlags for cmake-based builds.  Passed as arguments
    # to every make invocation (build, check, install).
    makeFlags = [
      "CC=${reccGpp}/bin/recc-gpp"
      "CXX=${reccGpp}/bin/recc-gpp"
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
