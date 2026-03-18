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
  stdenv,
}:
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
    nixception
    buildbox
    wait4x
    gcc
    coreutils
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

    # Start nixception in the background.
    echo "Starting nixception…"
    RUST_BACKTRACE=1 nixception 2>&1 | tee -i nixception.log &
    NIXCEPTION_PID=$!

    # Wait until nixception is accepting TCP connections.
    wait4x tcp 127.0.0.1:50051 --timeout 30s

    echo "nixception is ready – running recc"

    # Ask recc to compile the test file remotely via nixception.
    ${coreutils}/bin/rm -f main.o

    env \
      RECC_VERBOSE=1 \
      RECC_LOG_PROGRESS=1 \
      RECC_INSTANCE=main \
      RECC_SERVER=127.0.0.1:50051 \
      ${buildbox}/bin/recc \
        ${gcc}/bin/g++ -DBUILD_CONSTANT=42 -c main.cpp -o main.o \
      2>&1 | tee -i recc.log

    # ── verify ────────────────────────────────────────────────────────
    if [ ! -f main.o ]; then
      echo "FAIL: main.o was not created"
      echo "--- nixception log ---"
      cat nixception.log
      echo "--- recc log ---"
      cat recc.log
      echo "---"
      kill "$NIXCEPTION_PID" 2>/dev/null || true
      exit 1
    fi

    echo "SUCCESS: main.o was created by recc via nixception"

    # Check the nixception log for obvious errors.
    if grep -qi "ERROR" nixception.log; then
      echo "FAIL: nixception log contains errors"
      cat nixception.log
      kill "$NIXCEPTION_PID" 2>/dev/null || true
      exit 1
    fi

    echo "nixception log looks clean"

    kill "$NIXCEPTION_PID" 2>/dev/null || true
    wait "$NIXCEPTION_PID" 2>/dev/null || true

    runHook postBuild
  '';

  # ── install ─────────────────────────────────────────────────────────
  installPhase = ''
    runHook preInstall

    mkdir -p $out
    cp main.o          $out/
    cp nixception.log  $out/
    cp recc.log        $out/
    echo "Test passed" > $out/result.txt

    runHook postInstall
  '';
}
