# nixception-setup-hook.sh
#
# Setup hook for the nixceptionHook package.  When nixceptionHook is added to
# nativeBuildInputs this script is automatically sourced by stdenv because
# nixceptionHook installs it at $out/nix-support/setup-hook during its own
# fixupPhase (via the makeSetupHook mechanism, the same pattern cmake uses).
#
# It registers two extra phases:
#
#   nixceptionStartPhase  – registered via preBuildPhases; resolves the runner,
#                           starts the nixception server, and waits until it is
#                           ready to accept connections on 127.0.0.1:50051.
#
#   nixceptionStopPhase   – registered via postPhases; gracefully stops the
#                           server after the install phase completes.
#
# ── Runner ──────────────────────────────────────────────────────────────────
#
# The nixception server needs a "runner" derivation to execute remote actions.
# The runner is built from the extraRuntimeInputs passed to nixceptionHook (or
# nixceptionHook.withPackages) and its store paths are baked in at hook-install
# time via @runnerOut@ / @runnerDrv@.  Falls back to a minimal runner with only
# coreutils, util-linux, and bashNonInteractive when no extraRuntimeInputs are
# given.  The paths are passed directly to nixception without leaking into the
# surrounding environment.
#
# ── @…@ substitutions filled in at hook-install time by substituteAll ────────
#
#   @nixception@        – store path of the nixception package
#   @wait4x@            – store path of the wait4x package
#   @moreutils@         – store path of the moreutils package (provides `ts`)
#   @runnerOut@         – store path of the runner derivation output
#   @runnerDrv@         – store path of the runner .drv file

# shellcheck shell=bash

nixceptionStartPhase() {
    # ── Sanity-check: recursive-nix socket ───────────────────────────────────
    # The recursive-nix sandbox automatically exposes the Nix daemon socket at
    # /build/.nix-socket.  Fail fast when it is absent so the error is obvious.
    test -S /build/.nix-socket || {
        echo "nixception-hook: FAIL: recursive-nix daemon socket not found" \
            '– is requiredSystemFeatures = ["recursive-nix"] set?' >&2
        exit 1
    }

    # ── Start the server ─────────────────────────────────────────────────────
    # All three tools are invoked via their full store paths, baked in at
    # hook-install time by substituteAll – none of them need to be on PATH.
    echo "nixception-hook: starting nixception server" \
        "(runner: @runnerOut@)..."
    NIXCEPTION_RUNNER_OUT="@runnerOut@" \
        NIXCEPTION_RUNNER_DRV="@runnerDrv@" \
        RUST_BACKTRACE=1 \
        @nixception@/bin/nixception \
        > >(@moreutils@/bin/ts '[nixception] %H:%M:%.S' >&2) 2>&1 &
    NIXCEPTION_PID=$!

    @wait4x@/bin/wait4x tcp 127.0.0.1:50051 --timeout 30s
    echo "nixception-hook: server is ready (pid $NIXCEPTION_PID)"
}

nixceptionStopPhase() {
    if [ -n "${NIXCEPTION_PID+x}" ]; then
        echo "nixception-hook: stopping nixception server (pid $NIXCEPTION_PID)..."
        kill "$NIXCEPTION_PID" 2> /dev/null || true
        wait "$NIXCEPTION_PID" 2> /dev/null || true
        echo "nixception-hook: server stopped"
    fi
}

preBuildPhases="${preBuildPhases:-} nixceptionStartPhase"
postPhases="${postPhases:-} nixceptionStopPhase"
