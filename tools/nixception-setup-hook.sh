# nixception-setup-hook.sh
#
# Setup hook for the nixception package.  When nixception is added to
# nativeBuildInputs this script is automatically sourced by stdenv because
# nixception installs it at $out/nix-support/setup-hook during its own
# fixupPhase (via the setupHooks mechanism, the same pattern cmake uses).
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
# ── Runner selection ────────────────────────────────────────────────────────
#
# The nixception server needs a "runner" derivation to execute remote actions.
# Two sources are checked in order:
#
#   1. nixceptionRunner (shell variable set in the consumer derivation)
#      Must be the value returned by nixception.mkRunner, which is a
#      space-separated string of the runner output path and its .drv path:
#
#        nixceptionRunner = nixception.mkRunner [ myTool otherTool ];
#
#   2. Built-in default runner (substituted at hook-install time)
#      Contains only the minimal set of tools required by the runner script:
#      coreutils, util-linux, bashNonInteractive.
#      The paths are baked in via @defaultRunnerOut@ / @defaultRunnerDrv@.
#
# ── Tools used by this hook ──────────────────────────────────────────────────
#
#   nixception  – on PATH because the package itself is in nativeBuildInputs
#   wait4x      – invoked as @wait4x@/bin/wait4x (baked in at hook-install time)
#   ts          – invoked as @moreutils@/bin/ts (baked in at hook-install time)
#
# ── @…@ substitutions filled in at hook-install time by substituteAll ────────
#
#   @defaultRunnerOut@  – store path of the default runner derivation output
#   @defaultRunnerDrv@  – store path of the default runner .drv file
#   @wait4x@            – store path of the wait4x package
#   @moreutils@         – store path of the moreutils package (provides `ts`)

# shellcheck shell=bash

nixceptionStartPhase() {
    # ── Resolve the runner ───────────────────────────────────────────────────
    # If the consumer set nixceptionRunner (via nixception.mkRunner), parse the
    # two space-separated paths from it.  Otherwise fall back to the default
    # runner whose paths were baked in at hook-install time.
    if [ -n "${nixceptionRunner:-}" ]; then
        export NIXCEPTION_RUNNER_OUT="${nixceptionRunner%% *}"
        export NIXCEPTION_RUNNER_DRV="${nixceptionRunner##* }"
    else
        export NIXCEPTION_RUNNER_OUT="@defaultRunnerOut@"
        export NIXCEPTION_RUNNER_DRV="@defaultRunnerDrv@"
    fi

    # ── Sanity-check: recursive-nix socket ───────────────────────────────────
    # The recursive-nix sandbox automatically exposes the Nix daemon socket at
    # /build/.nix-socket.  Fail fast when it is absent so the error is obvious.
    test -S /build/.nix-socket || {
        echo "nixception-hook: FAIL: recursive-nix daemon socket not found" \
            '– is requiredSystemFeatures = ["recursive-nix"] set?' >&2
        exit 1
    }

    # ── Start the server ─────────────────────────────────────────────────────
    # nixception is on PATH via nativeBuildInputs.  wait4x and ts are invoked
    # via their full store paths, baked in at hook-install time by substituteAll.
    echo "nixception-hook: starting nixception server" \
        "(runner: $NIXCEPTION_RUNNER_OUT)..."
    RUST_BACKTRACE=1 nixception \
        > >(@moreutils@/bin/ts '[nixception] %H:%M:%.S' >&2) 2>&1 &
    export NIXCEPTION_PID=$!

    @wait4x@/bin/wait4x tcp 127.0.0.1:50051 --timeout 30s
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
# nixception itself sets dontUseNixceptionHook = "1" so the hook does not
# try to start a server during nixception's own Rust build.
if [ -z "${dontUseNixceptionHook:-}" ]; then
    preBuildPhases="${preBuildPhases:-} nixceptionStartPhase"
    postPhases="${postPhases:-} nixceptionStopPhase"
fi
