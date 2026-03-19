# nixception-setup-hook.sh
#
# Setup hook for the nixception package.  When nixception (or nixception-hook)
# is added to nativeBuildInputs this script is automatically sourced by stdenv
# before any build phases run.
#
# It registers two extra phases:
#
#   nixceptionStartPhase  – started via preBuildPhases, starts the nixception
#                           server and waits until it is ready to accept
#                           connections on 127.0.0.1:50051.
#
#   nixceptionStopPhase   – appended to postPhases, gracefully stops the
#                           server after the install phase completes.
#
# Store-path substitutions filled in by makeSetupHook:
#   @nixceptionBin@  – absolute path to the nixception wrapper binary
#   @wait4xBin@      – absolute path to wait4x
#   @tsBin@          – absolute path to ts (moreutils)

# shellcheck shell=bash

nixceptionStartPhase() {
    # The recursive-nix sandbox automatically exposes the Nix daemon socket at
    # /build/.nix-socket.  Fail fast when it is absent so the error is obvious.
    test -S /build/.nix-socket || {
        echo "nixception-hook: FAIL: recursive-nix daemon socket not found" \
            '– is requiredSystemFeatures = ["recursive-nix"] set?' >&2
        exit 1
    }

    echo "nixception-hook: starting nixception server..."
    RUST_BACKTRACE=1 @nixceptionBin@ \
        > >(@tsBin@ '[nixception] %H:%M:%.S' >&2) 2>&1 &
    export NIXCEPTION_PID=$!

    @wait4xBin@ tcp 127.0.0.1:50051 --timeout 30s
    echo "nixception-hook: server is ready (pid $NIXCEPTION_PID)"
}

nixceptionStopPhase() {
    if [ -n "${NIXCEPTION_PID:-}" ]; then
        echo "nixception-hook: stopping nixception server (pid $NIXCEPTION_PID)..."
        kill "$NIXCEPTION_PID" 2> /dev/null || true
        wait "$NIXCEPTION_PID" 2> /dev/null || true
        echo "nixception-hook: server stopped"
    fi
}

# Register the phases unless the caller has opted out.
if [ -z "${dontUseNixceptionHook:-}" ]; then
    preBuildPhases="${preBuildPhases:-} nixceptionStartPhase"
    postPhases="${postPhases:-} nixceptionStopPhase"
fi
