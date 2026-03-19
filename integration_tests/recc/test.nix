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
#     inherit (pkgs) gcc coreutils;
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
  bashNonInteractive,
}: let
  # A small helper derivation whose `bin/g++` is a wrapper that sleeps for
  # 10 seconds and then execs the real g++ from the `gcc` input.  It is added
  # to `nativeBuildInputs` so local invocations (e.g. the linker step) also
  # use it, and it is injected into the runner's PATH so remote compilations
  # dispatched by nixception go through the same sleeper.
  gppSleeper = stdenv.mkDerivation {
    pname = "gpp-sleeper";
    version = "0";
    phases = ["installPhase"];
    installPhase = ''
            mkdir -p $out/bin
            cat > $out/bin/g++ <<'EOF'
      #!/bin/sh
      # Sleep for 10s to make non-cached runs noticeably slower than cached runs.
      sleep 10
      exec ${gcc}/bin/g++ "$@"
      EOF
            chmod +x $out/bin/g++
    '';
  };

  # Build the canonical runner from tools/runner.nix, passing gppSleeper as
  # part of runtimeInputs so the runner's PATH includes the sleeping g++.
  # coreutils, util-linux and bashNonInteractive provide the default tooling;
  # gppSleeper is listed last so its g++ shadows any earlier entry.
  runner = callPackage ../../tools/runner.nix {
    bash = bashNonInteractive;
    extraRuntimeInputs = [gppSleeper];
  };

  # Wrap nixception to export NIXCEPTION_RUNNER_* so RunnerInfo::from_env()
  # can discover the runner at startup.  PATH manipulation is no longer needed
  # here because the runner itself prepends gppSleeper.
  nixceptionWrapper = stdenv.mkDerivation {
    pname = "nixception-wrapper";
    version = "0";
    phases = ["installPhase"];
    installPhase = ''
            mkdir -p $out/bin
            cat > $out/bin/nixception <<EOF
      #!/bin/sh
      export NIXCEPTION_RUNNER_OUT=${runner}
      export NIXCEPTION_RUNNER_DRV=${runner.drvPath}
      exec ${nixception}/bin/nixception "\$@"
      EOF
            chmod +x $out/bin/nixception
    '';
  };
in
  stdenv.mkDerivation {
    name = "recc-recursive-nix-test";

    # ── recursive-nix ───────────────────────────────────────────────────
    # This is the key ingredient: it tells the Nix daemon to expose its
    # Unix socket inside the build sandbox so that nixception can talk to
    # the local Nix store.
    requiredSystemFeatures = ["recursive-nix"];

    # The only source we need is the tiny C++ test file.
    src = ./test;

    nativeBuildInputs = [
      nixceptionWrapper
      buildbox
      wait4x
      gcc
      coreutils
      moreutils
      gppSleeper
    ];

    # ── build ───────────────────────────────────────────────────────────
    buildPhase = ''
          runHook preBuild

          # The recursive-nix sandbox automatically sets
          #   NIX_REMOTE=unix:///build/.nix-socket
          # nixception's NixStore backend now reads NIX_REMOTE to discover
          # the daemon socket, so no extra setup is needed.

          # Sanity-check: the socket should be reachable.
          test -S /build/.nix-socket \
            || { echo "FAIL: recursive-nix daemon socket not found"; exit 1; }

          # Start nixception in the background, piping its output through `ts` so
          # every log line is timestamped and prefixed with the service name.
          # The output goes directly to the Nix build log (fd 2) rather than a file,
          # which keeps $out deterministic.
          echo "Starting nixception…"
          RUST_BACKTRACE=1 nixception > >(ts '[nixception] %H:%M:%.S' >&2) 2>&1 &
          NIXCEPTION_PID=$!

          # Wait until nixception is accepting TCP connections.
          wait4x tcp 127.0.0.1:50051 --timeout 30s

          echo "nixception is ready – running make (using recc as driver)"

          # Clean any previous artifacts from earlier runs
          ${coreutils}/bin/rm -f demo_app *.o make.log

          # Wrap `recc g++` in a small script so make doesn't choke on the space in
          # the CC/CXX value (make passes the value verbatim to the shell; a
          # two-word CC confuses some rules).
          cat > ./recc-gpp <<EOF
      #!/bin/sh
      exec ${buildbox}/bin/recc ${gppSleeper}/bin/g++ "\$@"
      EOF
          chmod +x ./recc-gpp

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
            CC="./recc-gpp" \
            CXX="./recc-gpp" \
            CPPFLAGS=-DBUILD_CONSTANT=42 \
            make test > >(ts '[make] %H:%M:%.S' >&2) 2>&1

          # ── verify ────────────────────────────────────────────────────────
          if [ ! -f demo_app ]; then
            echo "FAIL: demo_app was not created by make" >&2
            kill "$NIXCEPTION_PID" 2>/dev/null || true
            exit 1
          fi

          echo "SUCCESS: demo_app was created by recc via nixception and Makefile"

          kill "$NIXCEPTION_PID" 2>/dev/null || true
          wait "$NIXCEPTION_PID" 2>/dev/null || true

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
