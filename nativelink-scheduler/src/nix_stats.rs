// Copyright 2024 The NativeLink Authors. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Cumulative timing statistics for nixception cost centers.
//!
//! [`NixceptionStats`] is a lock-free, atomically-updated struct that
//! aggregates wall-clock durations across all [`NixWorker`] instances.
//! Each worker records time spent in the various phases of action
//! execution, and the scheduler prints a summary on shutdown via
//! [`NixceptionStats::log_summary`].
//!
//! The individual cost centers are:
//!
//! | Field | What it measures |
//! |---|---|
//! | `upload_to_store_us` | Uploading CAS blobs to the Nix store |
//! | `store_path_scanning_us` | Scanning inputs/commands for `/nix/store/` references and resolving them |
//! | `derivation_prep_us` | Constructing the action derivation (manifest, environment, hashing) |
//! | `execute_us` | Executing the derivation via the Nix daemon (`build_derivation`) |
//! | `collect_outputs_us` | Reading exit code, uploading stdout/stderr/output files to CAS |
//! | `total_action_us` | Sum of every action's end-to-end `run_inner` span |
//!
//! Note that `total_action_us` is a **sum** across all (possibly parallel)
//! actions, so it is *not* wall-clock time — under concurrency it exceeds the
//! real elapsed time.  The true elapsed time is [`NixceptionStats::wall_clock`]
//! (server start → now); their ratio is the average parallelism.
//!
//! The difference `total_action_us − execute_us` gives the nixception
//! overhead (input setup + output collection) as opposed to the actual
//! command execution time.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nativelink_metric::MetricsComponent;

/// Cumulative timing statistics for nixception cost centers.
///
/// All durations are stored as microseconds in [`AtomicU64`] counters so
/// they can be updated from concurrent [`NixWorker`](crate::nix_worker::NixWorker)
/// instances without locking.
///
/// Create a single instance (wrapped in `Arc`) in the scheduler and share
/// it with every worker.
#[derive(Debug, Default, MetricsComponent)]
pub struct NixceptionStats {
    /// Number of actions that ran to completion (success or failure).
    #[metric(help = "Total number of completed actions")]
    pub actions_total: AtomicU64,

    /// Number of actions that completed successfully (exit code 0).
    #[metric(help = "Number of actions that succeeded")]
    pub actions_succeeded: AtomicU64,

    /// Number of actions that failed (non-zero exit code or error).
    #[metric(help = "Number of actions that failed")]
    pub actions_failed: AtomicU64,

    // ── Cost-center accumulators (microseconds) ─────────────────────
    /// Time spent uploading CAS blobs to the Nix store via
    /// `NixDaemonConnectionPool::upload_to_nix_daemon` / `add_to_store`.
    #[metric(help = "Cumulative time uploading to nix store (us)")]
    pub upload_to_store_us: AtomicU64,

    /// Time spent scanning inputs and commands for `/nix/store/`
    /// references (`scan_nix_store_paths_in_strings`) and resolving
    /// them via the daemon (`resolve_discovered_store_paths`).
    #[metric(help = "Cumulative time scanning for nix store paths (us)")]
    pub store_path_scanning_us: AtomicU64,

    /// Time spent constructing and uploading the action derivation
    /// (everything in `prepare_derivation` except the scanning and
    /// uploading portions).
    #[metric(help = "Cumulative time preparing derivations (us)")]
    pub derivation_prep_us: AtomicU64,

    /// Time spent executing the derivation via the Nix daemon
    /// (`build_derivation`).  This coarse span is broken down by the four
    /// runner-timing fields below.
    #[metric(help = "Cumulative time executing derivations (us)")]
    pub execute_us: AtomicU64,

    // ── Execution breakdown (from the runner's $out/timing.json) ─────
    //
    // These four split the opaque `execute_us` span.  They come from the
    // runner itself (via `$out/timing.json`), so they are only populated when
    // the built runner emits timing (best-effort; older runners contribute 0).

    /// Latency from the `build_derivation` request to the runner starting
    /// inside the sandbox — i.e. time spent in nix / the daemon (scheduling,
    /// sandbox setup) before any runner code runs.
    #[metric(help = "Cumulative nix→runner start latency (us)")]
    pub daemon_to_runner_us: AtomicU64,

    /// Runner setup phase: reading the manifest, copying inputs from the
    /// store, preparing directories and the child environment.
    #[metric(help = "Cumulative runner setup time (us)")]
    pub runner_setup_us: AtomicU64,

    /// The task itself: the action's command running in the sandbox
    /// (the runner's fork→waitpid span).
    #[metric(help = "Cumulative runner task time (us)")]
    pub runner_task_us: AtomicU64,

    /// Runner wrap-up phase: collecting declared outputs after the command
    /// finished.
    #[metric(help = "Cumulative runner wrap-up time (us)")]
    pub runner_wrapup_us: AtomicU64,

    /// Time spent collecting outputs: reading the exit code, uploading
    /// stdout/stderr, walking and uploading output files to the CAS.
    #[metric(help = "Cumulative time collecting outputs (us)")]
    pub collect_outputs_us: AtomicU64,

    /// Total wall-clock time of `run_inner` (end-to-end per action).
    #[metric(help = "Cumulative total action wall-clock time (us)")]
    pub total_action_us: AtomicU64,

    // ── Fine-grained sub-step counters ──────────────────────────────
    //
    // These break down the coarse cost centers above to pinpoint
    // contention and resource-wait time.

    /// Time spent waiting for a Nix daemon connection (semaphore acquire).
    /// High values indicate connection pool contention.
    #[metric(help = "Cumulative time waiting for daemon connection (us)")]
    pub daemon_acquire_wait_us: AtomicU64,

    /// Number of scan cache hits (file already scanned by another action).
    #[metric(help = "Number of scan cache hits")]
    pub scan_cache_hits: AtomicU64,

    /// Number of scan cache misses (file had to be read from CAS).
    #[metric(help = "Number of scan cache misses")]
    pub scan_cache_misses: AtomicU64,

    /// Time spent reading files from CAS during store-path scanning.
    #[metric(help = "Cumulative CAS read time during scanning (us)")]
    pub scan_cas_read_us: AtomicU64,

    /// Time spent fetching Command and Directory protos from CAS
    /// (at the start of prepare_derivation, before scanning).
    #[metric(help = "Cumulative CAS proto fetch time (us)")]
    pub cas_proto_fetch_us: AtomicU64,

    /// Time spent in unfold() walking the input directory tree.
    #[metric(help = "Cumulative input tree unfold time (us)")]
    pub unfold_us: AtomicU64,

    /// Time spent querying the Nix daemon for path info during
    /// resolve_discovered_store_paths.
    #[metric(help = "Cumulative query_path_info time (us)")]
    pub query_path_info_us: AtomicU64,

    /// Number of query_path_info calls made to the daemon.
    #[metric(help = "Number of query_path_info calls")]
    pub query_path_info_count: AtomicU64,

    /// Time spent building the JSON manifest and computing
    /// derivation hashes (after scanning, before upload).
    #[metric(help = "Cumulative manifest+hash computation time (us)")]
    pub manifest_and_hash_us: AtomicU64,

    /// Time spent reading exitcode/stdout/stderr files from the
    /// output directory during output collection.
    #[metric(help = "Cumulative output file read time (us)")]
    pub output_read_us: AtomicU64,

    /// Time spent uploading output files to CAS during output
    /// collection.
    #[metric(help = "Cumulative output CAS upload time (us)")]
    pub output_upload_us: AtomicU64,

    /// Number of path-info cache hits (store path already resolved by
    /// another action).
    #[metric(help = "Number of path-info cache hits")]
    pub path_info_cache_hits: AtomicU64,

    /// Number of path-info cache misses (had to query the daemon).
    #[metric(help = "Number of path-info cache misses")]
    pub path_info_cache_misses: AtomicU64,

    // ── Retry counters ───────────────────────────────────────────────

    /// Number of build retries attempted (each retry attempt counts once).
    #[metric(help = "Number of build retries attempted")]
    pub retries_attempted: AtomicU64,

    /// Number of retries that ultimately succeeded.
    #[metric(help = "Number of retries that succeeded")]
    pub retries_succeeded: AtomicU64,

    /// Number of actions that exhausted all retry attempts and still
    /// failed.
    #[metric(help = "Number of actions that exhausted retries")]
    pub retries_exhausted: AtomicU64,

    // ── In-flight gauges (debug instrumentation) ──────────────────
    /// Current number of in-flight actions (live workers).
    #[metric(help = "Current number of in-flight actions")]
    pub actions_in_flight: AtomicU64,

    /// Peak number of concurrently in-flight actions.
    #[metric(help = "Peak number of concurrently in-flight actions")]
    pub actions_in_flight_peak: AtomicU64,

    /// Wall-clock start (server boot).  Set when the stats are constructed;
    /// `wall_clock()` returns the elapsed time since — the *true* elapsed time
    /// in the summary, as opposed to `total_action_us`, which is the sum of
    /// per-action spans and overcounts under parallelism.
    ///
    /// Not annotated with `#[metric]`, so the `MetricsComponent` derive skips
    /// it (only `#[metric]` fields are published).
    pub start: StartInstant,
}

/// An [`Instant`] whose [`Default`] is the current time, so `NixceptionStats`
/// can keep deriving `Default` while stamping its construction moment as the
/// wall-clock start.
#[derive(Debug, Clone, Copy)]
pub struct StartInstant(pub Instant);

impl Default for StartInstant {
    fn default() -> Self {
        Self(Instant::now())
    }
}

impl NixceptionStats {
    /// Record `elapsed` into the given atomic counter.
    #[inline]
    pub fn record(&self, counter: &AtomicU64, elapsed: Duration) {
        counter.fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);
    }

    /// Increment the given counter by one.
    #[inline]
    pub fn increment(&self, counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// The true elapsed wall-clock time since the server started (i.e. since
    /// these stats were constructed).
    #[inline]
    pub fn wall_clock(&self) -> Duration {
        self.start.0.elapsed()
    }

    /// Enter a gauge scope: increment `current` and bump `peak` to the
    /// running maximum.
    #[inline]
    pub fn gauge_enter(&self, current: &AtomicU64, peak: &AtomicU64) {
        let now = current.fetch_add(1, Ordering::Relaxed) + 1;
        peak.fetch_max(now, Ordering::Relaxed);
    }

    /// Exit a gauge scope: decrement `current`.
    #[inline]
    pub fn gauge_exit(&self, current: &AtomicU64) {
        current.fetch_sub(1, Ordering::Relaxed);
    }

    /// Current in-flight action count.
    #[inline]
    pub fn actions_in_flight(&self) -> u64 {
        self.actions_in_flight.load(Ordering::Relaxed)
    }

    /// Peak concurrent in-flight action count.
    #[inline]
    pub fn actions_in_flight_peak(&self) -> u64 {
        self.actions_in_flight_peak.load(Ordering::Relaxed)
    }

    /// Print a human-readable summary of all cost centers to the
    /// `tracing` log at INFO level.
    ///
    /// Intended to be called from [`NixScheduler::shutdown`].
    pub fn log_summary(&self) {
        let actions = self.actions_total.load(Ordering::Relaxed);
        let succeeded = self.actions_succeeded.load(Ordering::Relaxed);
        let failed = self.actions_failed.load(Ordering::Relaxed);

        let cumulative = Duration::from_micros(self.total_action_us.load(Ordering::Relaxed));
        let wall = self.wall_clock();
        let upload = Duration::from_micros(self.upload_to_store_us.load(Ordering::Relaxed));
        let scan = Duration::from_micros(self.store_path_scanning_us.load(Ordering::Relaxed));
        let prep = Duration::from_micros(self.derivation_prep_us.load(Ordering::Relaxed));
        let exec = Duration::from_micros(self.execute_us.load(Ordering::Relaxed));
        let collect = Duration::from_micros(self.collect_outputs_us.load(Ordering::Relaxed));

        // "Overhead" is everything that isn't the actual nix build.
        let overhead = cumulative.saturating_sub(exec);

        if actions == 0 {
            tracing::debug!("Nixception shutting down — no actions were executed");
            return;
        }

        let parallelism = if wall.as_nanos() > 0 {
            cumulative.as_secs_f64() / wall.as_secs_f64()
        } else {
            0.0
        };
        let throughput = if wall.as_nanos() > 0 {
            actions as f64 / wall.as_secs_f64()
        } else {
            0.0
        };

        tracing::debug!(
            actions,
            succeeded,
            failed,
            wall_s = format_args!("{:.3}", wall.as_secs_f64()),
            cumulative_s = format_args!("{:.3}", cumulative.as_secs_f64()),
            parallelism = format_args!("{parallelism:.2}"),
            actions_per_s = format_args!("{throughput:.2}"),
            peak_parallelism = self.actions_in_flight_peak.load(Ordering::Relaxed),
            upload_s = format_args!("{:.3}", upload.as_secs_f64()),
            scan_s = format_args!("{:.3}", scan.as_secs_f64()),
            prep_s = format_args!("{:.3}", prep.as_secs_f64()),
            exec_s = format_args!("{:.3}", exec.as_secs_f64()),
            collect_s = format_args!("{:.3}", collect.as_secs_f64()),
            overhead_s = format_args!("{:.3}", overhead.as_secs_f64()),
            avg_exec_ms = format_args!("{:.1}", exec.as_secs_f64() * 1000.0 / actions as f64),
            "Nixception cumulative timing statistics"
        );
    }

    /// Build a human-readable, fixed-width summary string suitable for
    /// printing to a terminal or writing to a file.
    ///
    /// Returns `None` when no actions were executed.
    pub fn format_summary(&self) -> Option<String> {
        self.format_summary_with_daemon_wait(0)
    }

    /// Build a human-readable summary including sub-step breakdowns.
    ///
    /// `daemon_sem_wait_us` is the cumulative semaphore wait time from
    /// the [`NixDaemonConnectionPool`]; pass 0 if unavailable.
    pub fn format_summary_with_daemon_wait(
        &self,
        daemon_sem_wait_us: u64,
    ) -> Option<String> {
        let actions = self.actions_total.load(Ordering::Relaxed);
        if actions == 0 {
            return None;
        }

        let succeeded = self.actions_succeeded.load(Ordering::Relaxed);
        let failed = self.actions_failed.load(Ordering::Relaxed);

        // `cumulative` is the SUM of every action's end-to-end span; with
        // parallel workers it exceeds the real elapsed time.  `wall` is the
        // true elapsed wall-clock (server start → now/shutdown).
        let cumulative = Duration::from_micros(self.total_action_us.load(Ordering::Relaxed));
        let wall = self.wall_clock();
        let upload = Duration::from_micros(self.upload_to_store_us.load(Ordering::Relaxed));
        let scan = Duration::from_micros(self.store_path_scanning_us.load(Ordering::Relaxed));
        let prep = Duration::from_micros(self.derivation_prep_us.load(Ordering::Relaxed));
        let exec = Duration::from_micros(self.execute_us.load(Ordering::Relaxed));
        let collect = Duration::from_micros(self.collect_outputs_us.load(Ordering::Relaxed));
        let overhead = cumulative.saturating_sub(exec);
        let peak_parallelism = self.actions_in_flight_peak.load(Ordering::Relaxed);

        // Execution breakdown (from the runner's timing.json).
        let nix_latency = Duration::from_micros(self.daemon_to_runner_us.load(Ordering::Relaxed));
        let runner_setup = Duration::from_micros(self.runner_setup_us.load(Ordering::Relaxed));
        let runner_task = Duration::from_micros(self.runner_task_us.load(Ordering::Relaxed));
        let runner_wrapup = Duration::from_micros(self.runner_wrapup_us.load(Ordering::Relaxed));
        // Whatever the runner timing does not account for (server-side daemon
        // round-trip after the runner exits, unmeasured gaps, or actions whose
        // runner produced no timing record).
        let exec_unaccounted = exec
            .saturating_sub(nix_latency)
            .saturating_sub(runner_setup)
            .saturating_sub(runner_task)
            .saturating_sub(runner_wrapup);
        let have_runner_timing =
            !(nix_latency.is_zero() && runner_setup.is_zero() && runner_task.is_zero());

        // Sub-step durations
        let cas_proto = Duration::from_micros(self.cas_proto_fetch_us.load(Ordering::Relaxed));
        let unfold = Duration::from_micros(self.unfold_us.load(Ordering::Relaxed));
        let scan_cas = Duration::from_micros(self.scan_cas_read_us.load(Ordering::Relaxed));
        let qpi = Duration::from_micros(self.query_path_info_us.load(Ordering::Relaxed));
        let manifest_hash = Duration::from_micros(self.manifest_and_hash_us.load(Ordering::Relaxed));
        let out_read = Duration::from_micros(self.output_read_us.load(Ordering::Relaxed));
        let out_upload = Duration::from_micros(self.output_upload_us.load(Ordering::Relaxed));
        let daemon_wait = Duration::from_micros(daemon_sem_wait_us);

        let cache_hits = self.scan_cache_hits.load(Ordering::Relaxed);
        let cache_misses = self.scan_cache_misses.load(Ordering::Relaxed);
        let qpi_count = self.query_path_info_count.load(Ordering::Relaxed);
        let pi_cache_hits = self.path_info_cache_hits.load(Ordering::Relaxed);
        let pi_cache_misses = self.path_info_cache_misses.load(Ordering::Relaxed);

        // Percentages are of the CUMULATIVE action time (their correct
        // denominator — they describe how the per-action work splits, not the
        // wall clock).
        let exec_pct = if cumulative.as_nanos() > 0 {
            exec.as_secs_f64() / cumulative.as_secs_f64() * 100.0
        } else {
            0.0
        };
        let overhead_pct = 100.0 - exec_pct;

        // Average parallelism: how much work ran concurrently on average.
        // cumulative / wall (e.g. 1.9x means ~2 actions ran at once).
        let parallelism = if wall.as_nanos() > 0 {
            cumulative.as_secs_f64() / wall.as_secs_f64()
        } else {
            0.0
        };
        // Throughput: completed actions per second of wall-clock.
        let throughput = if wall.as_nanos() > 0 {
            actions as f64 / wall.as_secs_f64()
        } else {
            0.0
        };

        let avg = |d: Duration| -> f64 { d.as_secs_f64() * 1000.0 / actions as f64 };
        let fmt_dur = |d: Duration| -> String {
            if d.as_secs() >= 60 {
                format!("{}m {:05.2}s", d.as_secs() / 60, d.as_secs_f64() % 60.0)
            } else {
                format!("{:8.3}s", d.as_secs_f64())
            }
        };

        let mut s = String::with_capacity(1024);
        let _ = writeln!(s);
        let _ = writeln!(
            s,
            "  Nixception Timing Summary ({actions} actions: {succeeded} ok, {failed} failed)"
        );
        let _ = writeln!(s, "  {}", "─".repeat(60));
        let _ = writeln!(
            s,
            "  {:.<30} {} ({:>7.1} ms avg)",
            "Store-path scanning",
            fmt_dur(scan),
            avg(scan)
        );
        let _ = writeln!(
            s,
            "  {:.<30} {} ({:>7.1} ms avg)",
            "Derivation preparation",
            fmt_dur(prep),
            avg(prep)
        );
        let _ = writeln!(
            s,
            "  {:.<30} {} ({:>7.1} ms avg)",
            "Upload to nix store",
            fmt_dur(upload),
            avg(upload)
        );
        let _ = writeln!(
            s,
            "  {:.<30} {} ({:>7.1} ms avg)",
            "Command execution",
            fmt_dur(exec),
            avg(exec)
        );
        // Break the opaque execution span into nix→runner latency and the
        // runner's own setup / task / wrap-up phases, when the runner reported
        // timing.  These four (+ unaccounted) sum to "Command execution".
        if have_runner_timing {
            let _ = writeln!(
                s,
                "      {:.<26} {} ({:>7.1} ms avg)",
                "nix→runner latency",
                fmt_dur(nix_latency),
                avg(nix_latency)
            );
            let _ = writeln!(
                s,
                "      {:.<26} {} ({:>7.1} ms avg)",
                "runner setup",
                fmt_dur(runner_setup),
                avg(runner_setup)
            );
            let _ = writeln!(
                s,
                "      {:.<26} {} ({:>7.1} ms avg)",
                "task",
                fmt_dur(runner_task),
                avg(runner_task)
            );
            let _ = writeln!(
                s,
                "      {:.<26} {} ({:>7.1} ms avg)",
                "runner wrap-up",
                fmt_dur(runner_wrapup),
                avg(runner_wrapup)
            );
            let _ = writeln!(
                s,
                "      {:.<26} {} ({:>7.1} ms avg)",
                "unaccounted",
                fmt_dur(exec_unaccounted),
                avg(exec_unaccounted)
            );
        }
        let _ = writeln!(
            s,
            "  {:.<30} {} ({:>7.1} ms avg)",
            "Output collection",
            fmt_dur(collect),
            avg(collect)
        );
        let _ = writeln!(s, "  {}", "─".repeat(60));
        // Cumulative action time is the SUM of the per-action spans above; its
        // sub-lines describe how that per-action work splits.
        let _ = writeln!(
            s,
            "  {:.<30} {} ({:>7.1} ms avg)",
            "Cumulative action time",
            fmt_dur(cumulative),
            avg(cumulative)
        );
        let _ = writeln!(
            s,
            "    execution................ {} ({:>5.1}% of cumulative)",
            fmt_dur(exec),
            exec_pct
        );
        let _ = writeln!(
            s,
            "    overhead (setup+collect). {} ({:>5.1}% of cumulative)",
            fmt_dur(overhead),
            overhead_pct
        );
        // True elapsed time and how much parallelism it reflects.
        let _ = writeln!(s, "  {}", "─".repeat(60));
        let _ = writeln!(s, "  {:.<30} {}", "Wall-clock (elapsed)", fmt_dur(wall));
        let _ = writeln!(
            s,
            "  {:.<30} {:.2}x avg, {} peak",
            "Parallelism (cum / wall)", parallelism, peak_parallelism
        );
        let _ = writeln!(
            s,
            "  {:.<30} {:.2} actions/s",
            "Throughput", throughput
        );

        // Sub-step breakdown
        let _ = writeln!(s);
        let _ = writeln!(s, "  Sub-step breakdown:");
        let _ = writeln!(s, "  {}", "─".repeat(60));
        let _ = writeln!(
            s,
            "    {:.<28} {} ({:>7.1} ms avg)",
            "CAS proto fetch",
            fmt_dur(cas_proto),
            avg(cas_proto)
        );
        let _ = writeln!(
            s,
            "    {:.<28} {} ({:>7.1} ms avg)",
            "Input tree unfold",
            fmt_dur(unfold),
            avg(unfold)
        );
        let _ = writeln!(
            s,
            "    {:.<28} {} ({:>7.1} ms avg)",
            "Scan CAS reads",
            fmt_dur(scan_cas),
            avg(scan_cas)
        );
        let _ = writeln!(
            s,
            "    {:.<28} {} hits, {} misses",
            "Scan cache",
            cache_hits,
            cache_misses,
        );
        let _ = writeln!(
            s,
            "    {:.<28} {} ({:>7.1} ms avg, {} calls)",
            "query_path_info",
            fmt_dur(qpi),
            avg(qpi),
            qpi_count,
        );
        let _ = writeln!(
            s,
            "    {:.<28} {} hits, {} misses",
            "Path-info cache",
            pi_cache_hits,
            pi_cache_misses,
        );
        let _ = writeln!(
            s,
            "    {:.<28} {} ({:>7.1} ms avg)",
            "Manifest + hash",
            fmt_dur(manifest_hash),
            avg(manifest_hash)
        );
        let _ = writeln!(
            s,
            "    {:.<28} {} ({:>7.1} ms avg)",
            "Output read+stdout/stderr",
            fmt_dur(out_read),
            avg(out_read)
        );
        let _ = writeln!(
            s,
            "    {:.<28} {} ({:>7.1} ms avg)",
            "Output CAS upload",
            fmt_dur(out_upload),
            avg(out_upload)
        );
        let _ = writeln!(
            s,
            "    {:.<28} {}",
            "Daemon semaphore wait",
            fmt_dur(daemon_wait),
        );

        // Retry stats
        let retries_attempted = self.retries_attempted.load(Ordering::Relaxed);
        let retries_succeeded = self.retries_succeeded.load(Ordering::Relaxed);
        let retries_exhausted = self.retries_exhausted.load(Ordering::Relaxed);
        if retries_attempted > 0 {
            let _ = writeln!(s);
            let _ = writeln!(s, "  Retry statistics:");
            let _ = writeln!(s, "  {}", "─".repeat(60));
            let _ = writeln!(
                s,
                "    {:.<28} {}",
                "Retries attempted",
                retries_attempted,
            );
            let _ = writeln!(
                s,
                "    {:.<28} {}",
                "Retries succeeded",
                retries_succeeded,
            );
            let _ = writeln!(
                s,
                "    {:.<28} {}",
                "Retries exhausted",
                retries_exhausted,
            );
        }
        let _ = writeln!(s);

        Some(s)
    }

    /// Write the formatted summary to the path given by the
    /// `NIXCEPTION_STATS_FILE` environment variable.
    ///
    /// Does nothing (and logs a debug message) when the variable is
    /// unset.  Errors are logged but not propagated — stats output
    /// must never fail the build.
    pub fn write_summary_file(&self) {
        self.write_summary_file_with_daemon_wait(0);
    }

    /// Like [`write_summary_file`] but includes daemon semaphore wait
    /// time in the sub-step breakdown.
    pub fn write_summary_file_with_daemon_wait(&self, daemon_sem_wait_us: u64) {
        let path = match std::env::var("NIXCEPTION_STATS_FILE") {
            Ok(p) if !p.is_empty() => p,
            _ => {
                tracing::debug!("NIXCEPTION_STATS_FILE not set, skipping file output");
                return;
            }
        };

        let content = match self.format_summary_with_daemon_wait(daemon_sem_wait_us) {
            Some(s) => s,
            None => "Nixception: no actions were executed.\n".to_string(),
        };

        if let Err(e) = std::fs::write(&path, &content) {
            tracing::warn!(
                path,
                error = %e,
                "Failed to write nixception stats file"
            );
        } else {
            tracing::debug!(path, "Wrote nixception stats file");
        }
    }
}

/// A guard that records the elapsed time since its creation into a
/// specific cost-center counter when dropped.
///
/// This is useful for timing blocks that may exit via `?` or other
/// early-return paths — the duration is always recorded.
///
/// # Example
///
/// ```ignore
/// let _guard = TimingGuard::new(&stats, &stats.execute_us);
/// self.execute_derivation(&drv_path).await?;
/// // guard dropped here → elapsed time recorded
/// ```
#[derive(Debug)]
pub struct TimingGuard<'a> {
    stats: &'a NixceptionStats,
    counter: &'a AtomicU64,
    start: Instant,
}

impl<'a> TimingGuard<'a> {
    /// Start timing.  The elapsed duration will be recorded into
    /// `counter` when this guard is dropped.
    #[inline]
    pub fn new(stats: &'a NixceptionStats, counter: &'a AtomicU64) -> Self {
        Self {
            stats,
            counter,
            start: Instant::now(),
        }
    }

    /// Return the time elapsed since this guard was created, without
    /// stopping or recording anything.
    #[inline]
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }
}

impl Drop for TimingGuard<'_> {
    fn drop(&mut self) {
        self.stats.record(self.counter, self.start.elapsed());
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_adds_to_counter() {
        let stats = NixceptionStats::default();
        stats.record(&stats.upload_to_store_us, Duration::from_millis(42));
        stats.record(&stats.upload_to_store_us, Duration::from_millis(8));
        assert_eq!(
            stats.upload_to_store_us.load(Ordering::Relaxed),
            50_000 // 50 ms in microseconds
        );
    }

    #[test]
    fn increment_adds_one() {
        let stats = NixceptionStats::default();
        stats.increment(&stats.actions_total);
        stats.increment(&stats.actions_total);
        stats.increment(&stats.actions_total);
        assert_eq!(stats.actions_total.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn timing_guard_records_on_drop() {
        let stats = NixceptionStats::default();
        {
            let _guard = TimingGuard::new(&stats, &stats.execute_us);
            // Simulate some work.
            std::thread::sleep(Duration::from_millis(5));
        }
        // The guard has been dropped, so something was recorded.
        let recorded = stats.execute_us.load(Ordering::Relaxed);
        assert!(
            recorded >= 4_000, // at least ~4ms in microseconds
            "Expected at least 4000 us, got {recorded}"
        );
    }

    #[test]
    fn log_summary_does_not_panic_with_zero_actions() {
        let stats = NixceptionStats::default();
        // Should not divide by zero or panic.
        stats.log_summary();
    }

    #[test]
    fn log_summary_does_not_panic_with_data() {
        let stats = NixceptionStats::default();
        stats.increment(&stats.actions_total);
        stats.increment(&stats.actions_succeeded);
        stats.record(&stats.total_action_us, Duration::from_secs(1));
        stats.record(&stats.execute_us, Duration::from_millis(800));
        stats.record(&stats.upload_to_store_us, Duration::from_millis(50));
        stats.record(&stats.store_path_scanning_us, Duration::from_millis(10));
        stats.record(&stats.derivation_prep_us, Duration::from_millis(100));
        stats.record(&stats.collect_outputs_us, Duration::from_millis(40));
        stats.log_summary();
    }

    #[test]
    fn default_is_all_zeros() {
        let stats = NixceptionStats::default();
        assert_eq!(stats.actions_total.load(Ordering::Relaxed), 0);
        assert_eq!(stats.actions_succeeded.load(Ordering::Relaxed), 0);
        assert_eq!(stats.actions_failed.load(Ordering::Relaxed), 0);
        assert_eq!(stats.upload_to_store_us.load(Ordering::Relaxed), 0);
        assert_eq!(stats.store_path_scanning_us.load(Ordering::Relaxed), 0);
        assert_eq!(stats.derivation_prep_us.load(Ordering::Relaxed), 0);
        assert_eq!(stats.execute_us.load(Ordering::Relaxed), 0);
        assert_eq!(stats.collect_outputs_us.load(Ordering::Relaxed), 0);
        assert_eq!(stats.total_action_us.load(Ordering::Relaxed), 0);
    }
}
