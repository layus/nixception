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

# Helper: print a message only in verbose mode.
_nixception_log() {
    if [ "${NIXCEPTION_VERBOSE:-0}" = "1" ]; then
        echo "nixception-hook: $*" >&2
    fi
}

# ── Memory / OOM diagnostics ──────────────────────────────────────────────────
#
# The intermittent "builder failed due to signal 9 (Killed)" failures are most
# likely caused by the kernel OOM killer firing in the build's memory cgroup.
# The nixception server and every parallel `recc` action share the build's
# cgroup, so when memory is exhausted the cgroup is killed as a unit – the main
# builder dies with SIGKILL and nixception's connections to the recursive-nix
# daemon drop, producing the "(ignored) ... Broken pipe" lines from the daemon.
#
# Inside a cgroup-v2 Nix sandbox the build's own cgroup is mounted at
# /sys/fs/cgroup.  memory.events exposes oom / oom_kill counters and
# memory.peak the high-water mark.  These helpers surface that accounting.
#
# IMPORTANT: if Nix is configured with cgroup OOM-group semantics the bash that
# runs failureHook is itself killed, so an end-of-build dump may never execute.
# That is why the sampler below streams snapshots *while the build runs*: the
# last line before death stays in the build log even when the trap never fires.

# Root of the build's cgroup inside the sandbox (cgroup v2).
_nixception_cgroup_root=/sys/fs/cgroup

# Humanize a byte count (best-effort; passes through non-numeric values like
# the literal "max" used by memory.max).
_nixception_h() {
    numfmt --to=iec "$1" 2> /dev/null || printf '%s' "$1"
}

# Print a single compact memory line to stderr.
#   $1 = nixception server pid   $2 = start epoch (seconds)
_nixception_mem_line() {
    local _cg="$_nixception_cgroup_root" _srvpid="$1" _t0="$2"
    local _now _t _rss _availkb _avail
    _now=$(date +%s)
    _t=$((_now - _t0))

    # nixception server RSS (from /proc; works even when cgroups are off).
    _rss=$(awk '/^VmRSS:/{print $2 $3}' "/proc/$_srvpid/status" 2> /dev/null)

    # System-wide available memory (from /proc/meminfo; always present).
    _availkb=$(awk '/^MemAvailable:/{print $2}' /proc/meminfo 2> /dev/null)
    if [ -n "$_availkb" ]; then
        _avail=$(_nixception_h $((_availkb * 1024)))
    else
        _avail='?'
    fi

    # cgroup accounting if available; otherwise mark n/a (cgroups disabled).
    local _cgpart
    if [ -r "$_cg/memory.current" ]; then
        local _cur _peak _swap _oom _oomk
        _cur=$(cat "$_cg/memory.current" 2> /dev/null)
        _peak=$(cat "$_cg/memory.peak" 2> /dev/null || echo n/a)
        _swap=$(cat "$_cg/memory.swap.current" 2> /dev/null || echo 0)
        _oom=$(awk '/^oom /{print $2}' "$_cg/memory.events" 2> /dev/null)
        _oomk=$(awk '/^oom_kill /{print $2}' "$_cg/memory.events" 2> /dev/null)
        _cgpart=$(printf 'cgroup{cur=%s peak=%s swap=%s oom=%s oom_kill=%s}' \
            "$(_nixception_h "$_cur")" "$(_nixception_h "$_peak")" \
            "$(_nixception_h "$_swap")" "${_oom:-?}" "${_oomk:-?}")
    else
        _cgpart='cgroup{n/a}'
    fi

    printf 'nixception-hook: mem t=%ss nixception_rss=%s sys_avail=%s %s\n' \
        "$_t" "${_rss:-gone}" "$_avail" "$_cgpart" >&2
}

# Background sampler loop: emit a memory line every interval seconds.
#   $1 = nixception server pid
_nixception_mem_sampler() {
    local _srvpid="$1" _interval _t0
    _interval="${NIXCEPTION_DEBUG_MEM_INTERVAL:-2}"
    _t0=$(date +%s)
    while :; do
        _nixception_mem_line "$_srvpid" "$_t0"
        sleep "$_interval"
    done
}

# Verbose snapshot + best-effort kernel OOM log, printed on build failure.
_nixception_oom_report() {
    local _cg="$_nixception_cgroup_root"
    if [ -r "$_cg/memory.current" ]; then
        echo '' >&2
        echo 'nixception-hook: ── cgroup memory accounting (on failure) ──' >&2
        local _f
        for _f in memory.current memory.peak memory.max \
            memory.swap.current memory.swap.peak memory.swap.max; do
            [ -r "$_cg/$_f" ] && printf '  %-22s %s\n' \
                "$_f" "$(cat "$_cg/$_f" 2> /dev/null)" >&2
        done
        if [ -r "$_cg/memory.events" ]; then
            echo '  memory.events:' >&2
            sed 's/^/    /' "$_cg/memory.events" >&2
            # oom_kill > 0 is the smoking gun.
            if awk '/^oom_kill /{exit !($2 > 0)}' "$_cg/memory.events" 2> /dev/null; then
                echo 'nixception-hook: *** cgroup OOM kill detected (oom_kill > 0) ***' >&2
                echo '    => the build cgroup ran out of memory; lower build' >&2
                echo '       parallelism (enableParallelBuilding=false / -j) or' >&2
                echo '       raise the memory limit, then re-run.' >&2
            fi
        fi
    else
        echo 'nixception-hook: cgroup memory files not readable' \
            '(cgroups disabled in nix.conf? OOM evidence unavailable here).' >&2
        echo '    => If the server log shows the nixception process was' >&2
        echo '       "Killed", it was SIGKILL — almost certainly the kernel' >&2
        echo '       OOM killer.  Confirm on the host with:' >&2
        echo '         sudo journalctl -k | grep -iE "oom|killed process" | tail' >&2
        echo '       and watch the "nixception_rss" / "sys_avail" sampler lines' >&2
        echo '       above for the run-up to the kill.' >&2
    fi

    # Best-effort: kernel OOM messages.  dmesg usually needs host privileges
    # and is absent inside the sandbox, so this is silently skipped if it
    # produces nothing.
    if command -v dmesg > /dev/null 2>&1; then
        local _oom
        _oom=$(dmesg -T 2> /dev/null |
            grep -iE 'oom|killed process|out of memory|memory cgroup' |
            tail -n 20)
        if [ -n "$_oom" ]; then
            echo 'nixception-hook: ── kernel OOM messages (dmesg) ──' >&2
            printf '%s\n' "$_oom" >&2
        fi
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
    # NIXCEPTION_RUNNER_* are scoped to this one invocation via inline assignment.
    #
    # nixception reads NIXCEPTION_LOG for its log level.  In verbose mode the
    # default level is "info" and server output is timestamped and forwarded to
    # stderr.  In quiet mode the level drops to "warn" and output goes to a log
    # file that is only shown on failure.  If the caller already set
    # NIXCEPTION_LOG we never override it.
    local _nixception_log_level
    if [ -n "${NIXCEPTION_LOG:-}" ]; then
        _nixception_log_level="$NIXCEPTION_LOG"
    elif [ "$_verbose" = "1" ]; then
        _nixception_log_level="info"
    else
        _nixception_log_level="warn"
    fi

    _nixception_log "starting nixception server (runner: @runnerOut@)..."
    if [ "$_verbose" = "1" ]; then
        NIXCEPTION_RUNNER_OUT="@runnerOut@" \
            NIXCEPTION_RUNNER_DRV="@runnerDrv@" \
            NIXCEPTION_LOG="$_nixception_log_level" \
            RUST_BACKTRACE=1 \
            @nixception@/bin/nixception \
            > >(@moreutils@/bin/ts -s '[nixception] %H:%M:%.S' >&2) 2>&1 &
    else
        NIXCEPTION_RUNNER_OUT="@runnerOut@" \
            NIXCEPTION_RUNNER_DRV="@runnerDrv@" \
            NIXCEPTION_LOG="$_nixception_log_level" \
            RUST_BACKTRACE=1 \
            @nixception@/bin/nixception \
            > "$_logfile" 2>&1 &
    fi
    local _pid=$!

    # ── Start the background memory sampler ──────────────────────────────────
    # Streams a compact cgroup-memory line to stderr every couple of seconds so
    # the run-up to an OOM kill is visible in the build log even if the EXIT
    # trap is itself killed by the OOM-group.  Enabled by default while we are
    # debugging the intermittent SIGKILL; set NIXCEPTION_DEBUG_MEM=0 to disable
    # or NIXCEPTION_DEBUG_MEM_INTERVAL=<seconds> to change the cadence.
    local _mem_pid=""
    if [ "${NIXCEPTION_DEBUG_MEM:-1}" = "1" ]; then
        _nixception_mem_sampler "$_pid" &
        _mem_pid=$!
        _nixception_log "memory sampler started (pid $_mem_pid)"
    fi

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
        kill $_mem_pid 2>/dev/null || true
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

        # Surface cgroup memory accounting / OOM evidence on failure.  This is
        # best-effort and only useful when the EXIT trap survives (i.e. the
        # OOM-group did not also kill this shell); the background sampler covers
        # the case where it does not.
        _nixception_oom_report

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
