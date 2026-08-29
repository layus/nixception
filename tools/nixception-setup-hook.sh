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
# ── Verbosity ─────────────────────────────────────────────────────────────────
#
# By default the hook runs in quiet mode: the nixception server's output is
# sent to a log file and lifecycle messages are suppressed.  Set
# NIXCEPTION_VERBOSE=1 in the build environment to get the previous behaviour
# where server output is timestamped and forwarded to stderr in real time.
#
# On build *failure* the server log is always dumped to stderr regardless of
# the verbosity setting, so you can still debug without re-running.
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
# The runner's store paths are baked into the nixception binary itself at
# *compile* time (NIXCEPTION_RUNNER_OUT/_DRV, set by flake.nix's
# nixceptionFor — see nativelink-scheduler/src/runner_info.rs). This hook has
# no runner-related configuration at all: it just starts the binary, which
# already knows which runner it was built against.
#
# ── Extra sandbox tools ──────────────────────────────────────────────────────
#
# To make extra tools available inside the reapi-action sandbox, export
# NIXCEPTION_EXTRA_SANDBOX_PATHS (colon-separated /nix/store/… paths) in the
# build environment *before* this phase runs — it is read directly by
# nixception at start-up (see nativelink-scheduler/src/runner_info.rs), so it
# needs no substitution here; being already exported, it is inherited by the
# background nixception invocation below like any other ambient variable.
#
# ── @…@ substitutions filled in at hook-install time by substituteAll ────────
#
#   @nixception@  – store path of the nixception package
#   @wait4x@      – store path of the wait4x package
#   @moreutils@   – store path of the moreutils package (provides `ts`)

# shellcheck shell=bash

# Helper: print a message only in verbose mode.
_nixception_log() {
    if [ "${NIXCEPTION_VERBOSE:-0}" = "1" ]; then
        echo "nixception-hook: $*" >&2
    fi
}

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

    local _verbose="${NIXCEPTION_VERBOSE:-0}"

    # ── Stats file ───────────────────────────────────────────────────────────
    # Tell the server where to write its timing summary on shutdown.  The
    # hook reads this file after the server exits and prints it to stderr.
    export NIXCEPTION_STATS_FILE
    NIXCEPTION_STATS_FILE="$(mktemp -p /build nixception-stats.XXXXXX)"

    # ── Server log file (used in quiet mode) ─────────────────────────────────
    local _logfile
    _logfile="$(mktemp -p /build nixception-server.log.XXXXXX)"

    # ── Start the server ─────────────────────────────────────────────────────
    # All tools are invoked via their full store paths baked in at hook-install
    # time – none of them need to be on PATH.
    #
    # In verbose mode the default RUST_LOG level is "info" and server output is
    # timestamped and forwarded to stderr.  In quiet mode the level drops to
    # "warn" and output goes to a log file that is only shown on failure.
    # If the caller already set RUST_LOG we never override it.
    local _rust_log
    if [ -n "${RUST_LOG:-}" ]; then
        _rust_log="$RUST_LOG"
    elif [ "$_verbose" = "1" ]; then
        _rust_log="info"
    else
        _rust_log="warn"
    fi

    _nixception_log "starting nixception server..."
    if [ "$_verbose" = "1" ]; then
        RUST_LOG="$_rust_log" \
            RUST_BACKTRACE=1 \
            @nixception@/bin/nixception \
            > >(@moreutils@/bin/ts -s '[nixception] %H:%M:%.S' >&2) 2>&1 &
    else
        RUST_LOG="$_rust_log" \
            RUST_BACKTRACE=1 \
            @nixception@/bin/nixception \
            > "$_logfile" 2>&1 &
    fi
    local _pid=$!

    # ── Register shutdown with stdenv's exit hooks ────────────────────────────
    # exitHandler (stdenv's EXIT trap) calls runHook failureHook on error and
    # runHook exitHook on success.  Appending to both ensures the server is
    # stopped in either case.  runHook-based hooks compose: multiple hooks
    # appended to the same variable are all executed in order.
    #
    # _nixceptionStop is the common tear-down; _nixceptionFailStop additionally
    # dumps the server log so failures are debuggable even in quiet mode.
    #
    # $_pid and $_logfile are expanded now (double-quote context) to bake
    # their actual values into the function bodies.
    # shellcheck disable=SC2064  # intentional: expand at definition-time
    eval "_nixceptionStop() {
        _nixception_log 'stopping server (pid $_pid)...'
        kill $_pid 2>/dev/null || true
        wait $_pid 2>/dev/null || true

        # ── Print timing summary ─────────────────────────────────────────
        if [ -s \"\$NIXCEPTION_STATS_FILE\" ]; then
            echo '' >&2
            echo 'nixception-hook: ── timing statistics ──' >&2
            cat \"\$NIXCEPTION_STATS_FILE\" >&2
            rm -f \"\$NIXCEPTION_STATS_FILE\"
        else
            _nixception_log 'no timing statistics available'
            rm -f \"\$NIXCEPTION_STATS_FILE\"
        fi
    }"

    # shellcheck disable=SC2064  # intentional: expand at definition-time
    eval "_nixceptionFailStop() {
        _nixceptionStop

        # In quiet mode, dump the server log on failure so the user can
        # debug without re-running in verbose mode.
        if [ '${_verbose}' != '1' ] && [ -s '$_logfile' ]; then
            echo '' >&2
            echo 'nixception-hook: ── server log (last 200 lines) ──' >&2
            tail -n 200 '$_logfile' >&2
            echo 'nixception-hook: ── end of server log ──' >&2
            echo '(set NIXCEPTION_VERBOSE=1 for full real-time output)' >&2
        fi
        rm -f '$_logfile'
    }"

    # Clean exit: just stop the server (and remove the log file).
    # shellcheck disable=SC2064
    eval "_nixceptionCleanStop() {
        _nixceptionStop
        rm -f '$_logfile'
    }"

    exitHook+=$'\n_nixceptionCleanStop\n'
    failureHook+=$'\n_nixceptionFailStop\n'

    # ── Wait for the server to be ready ──────────────────────────────────────
    @wait4x@/bin/wait4x tcp 127.0.0.1:50051 --timeout 30s --quiet
    _nixception_log "server is ready (pid $_pid)"
}

preConfigurePhases="${preConfigurePhases:-} nixceptionStartPhase"
