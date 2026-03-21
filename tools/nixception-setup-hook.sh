# nixception-setup-hook.sh
#
# Setup hook for the nixceptionHook package.  When nixceptionHook is added to
# nativeBuildInputs this script is automatically sourced by stdenv because
# nixceptionHook installs it at $out/nix-support/setup-hook during its own
# fixupPhase (via the makeSetupHook mechanism, the same pattern cmake uses).
#
# It registers one extra phase:
#
#   nixceptionStartPhase  – registered via preConfigurePhases; starts the
#                           nixception server and waits until it is ready to
#                           accept connections on 127.0.0.1:50051.  The server
#                           is stopped by hooking into stdenv's failureHook and
#                           exitHook, which are called by exitHandler (the EXIT
#                           trap) on failure and success respectively.
#
# The phase is registered before configurePhase (rather than before buildPhase)
# because some build systems (e.g. CMake) probe the compiler during configure.
# When the compiler is wrapped by recc, the nixception server must already be
# listening or those probes will fail with connection-refused errors.
#
# ── Shutdown ──────────────────────────────────────────────────────────────────
#
# stdenv's exitHandler (set as the EXIT trap before any setup hook runs) calls:
#   runHook failureHook   – on non-zero exit
#   runHook exitHook      – on clean exit
#
# We append our stop command to both hooks so the server is always torn down,
# whether the build succeeds or fails, without touching the EXIT trap at all.
# runHook appends to the named hook variable, so multiple hooks compose safely.
#
# ── Runner ────────────────────────────────────────────────────────────────────
#
# The runner is built from the extraRuntimeInputs passed to nixceptionHook (or
# nixceptionHook.withPackages) and its store paths are baked in at hook-install
# time via @runnerOut@ / @runnerDrv@.  They are passed as inline variables
# scoped to the nixception invocation and do not leak into the build environment.
#
# ── @…@ substitutions filled in at hook-install time by substituteAll ────────
#
#   @nixception@  – store path of the nixception package
#   @wait4x@      – store path of the wait4x package
#   @moreutils@   – store path of the moreutils package (provides `ts`)
#   @runnerOut@   – store path of the runner derivation output
#   @runnerDrv@   – store path of the runner .drv file

# shellcheck shell=bash

nixceptionStartPhase() {
    # ── Sanity-check: recursive-nix socket ───────────────────────────────────
    # The Nix daemon socket is exposed at /build/.nix-socket when the sandbox
    # is started with the recursive-nix system feature.  Fail fast so the
    # error is obvious rather than a cryptic connection-refused from nixception.
    test -S /build/.nix-socket || {
        echo "nixception-hook: FAIL: recursive-nix daemon socket not found" \
            '– is requiredSystemFeatures = ["recursive-nix"] set?' >&2
        exit 1
    }

    # ── Start the server ─────────────────────────────────────────────────────
    # All tools are invoked via their full store paths baked in at hook-install
    # time – none of them need to be on PATH.
    # NIXCEPTION_RUNNER_* are scoped to this one invocation via inline assignment.
    echo "nixception-hook: starting nixception server (runner: @runnerOut@)..."
    NIXCEPTION_RUNNER_OUT="@runnerOut@" \
        NIXCEPTION_RUNNER_DRV="@runnerDrv@" \
        RUST_BACKTRACE=1 \
        @nixception@/bin/nixception \
        > >(@moreutils@/bin/ts -s '[nixception] %H:%M:%.S' >&2) 2>&1 &
    local _pid=$!

    # ── Register shutdown with stdenv's exit hooks ────────────────────────────
    # exitHandler (stdenv's EXIT trap) calls runHook failureHook on error and
    # runHook exitHook on success.  Appending to both ensures the server is
    # stopped in either case.  runHook-based hooks compose: multiple hooks
    # appended to the same variable are all executed in order.
    # The stop snippet is factored into a named function so the kill/wait lines
    # are not duplicated; $_pid is expanded now (double-quote context) to bake
    # the actual PID into the function body.
    # shellcheck disable=SC2064  # intentional: expand _pid at definition-time
    eval "_nixceptionStop() {
        echo 'nixception-hook: stopping server (pid $_pid)...' >&2
        kill $_pid 2>/dev/null || true
        wait $_pid 2>/dev/null || true
    }"
    exitHook+=$'\n_nixceptionStop\n'
    failureHook+=$'\n_nixceptionStop\n'

    # ── Wait for the server to be ready ──────────────────────────────────────
    @wait4x@/bin/wait4x tcp 127.0.0.1:50051 --timeout 30s
    echo "nixception-hook: server is ready (pid $_pid)"
}

preConfigurePhases="${preConfigurePhases:-} nixceptionStartPhase"
