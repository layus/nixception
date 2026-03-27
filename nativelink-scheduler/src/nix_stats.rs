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
//! | `total_action_us` | End-to-end wall-clock time of `run_inner` |
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
    /// (`build_derivation`).
    #[metric(help = "Cumulative time executing derivations (us)")]
    pub execute_us: AtomicU64,

    /// Time spent collecting outputs: reading the exit code, uploading
    /// stdout/stderr, walking and uploading output files to the CAS.
    #[metric(help = "Cumulative time collecting outputs (us)")]
    pub collect_outputs_us: AtomicU64,

    /// Total wall-clock time of `run_inner` (end-to-end per action).
    #[metric(help = "Cumulative total action wall-clock time (us)")]
    pub total_action_us: AtomicU64,
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

    /// Print a human-readable summary of all cost centers to the
    /// `tracing` log at INFO level.
    ///
    /// Intended to be called from [`NixScheduler::shutdown`].
    pub fn log_summary(&self) {
        let actions = self.actions_total.load(Ordering::Relaxed);
        let succeeded = self.actions_succeeded.load(Ordering::Relaxed);
        let failed = self.actions_failed.load(Ordering::Relaxed);

        let total = Duration::from_micros(self.total_action_us.load(Ordering::Relaxed));
        let upload = Duration::from_micros(self.upload_to_store_us.load(Ordering::Relaxed));
        let scan = Duration::from_micros(self.store_path_scanning_us.load(Ordering::Relaxed));
        let prep = Duration::from_micros(self.derivation_prep_us.load(Ordering::Relaxed));
        let exec = Duration::from_micros(self.execute_us.load(Ordering::Relaxed));
        let collect = Duration::from_micros(self.collect_outputs_us.load(Ordering::Relaxed));

        // "Overhead" is everything that isn't the actual nix build.
        let overhead = total.saturating_sub(exec);

        if actions == 0 {
            tracing::info!("Nixception shutting down — no actions were executed");
            return;
        }

        tracing::info!(
            actions,
            succeeded,
            failed,
            total_s = format_args!("{:.3}", total.as_secs_f64()),
            upload_s = format_args!("{:.3}", upload.as_secs_f64()),
            scan_s = format_args!("{:.3}", scan.as_secs_f64()),
            prep_s = format_args!("{:.3}", prep.as_secs_f64()),
            exec_s = format_args!("{:.3}", exec.as_secs_f64()),
            collect_s = format_args!("{:.3}", collect.as_secs_f64()),
            overhead_s = format_args!("{:.3}", overhead.as_secs_f64()),
            avg_total_ms = format_args!("{:.1}", total.as_secs_f64() * 1000.0 / actions as f64),
            avg_exec_ms = format_args!("{:.1}", exec.as_secs_f64() * 1000.0 / actions as f64),
            avg_overhead_ms =
                format_args!("{:.1}", overhead.as_secs_f64() * 1000.0 / actions as f64),
            "Nixception cumulative timing statistics"
        );
    }

    /// Build a human-readable, fixed-width summary string suitable for
    /// printing to a terminal or writing to a file.
    ///
    /// Returns `None` when no actions were executed.
    pub fn format_summary(&self) -> Option<String> {
        let actions = self.actions_total.load(Ordering::Relaxed);
        if actions == 0 {
            return None;
        }

        let succeeded = self.actions_succeeded.load(Ordering::Relaxed);
        let failed = self.actions_failed.load(Ordering::Relaxed);

        let total = Duration::from_micros(self.total_action_us.load(Ordering::Relaxed));
        let upload = Duration::from_micros(self.upload_to_store_us.load(Ordering::Relaxed));
        let scan = Duration::from_micros(self.store_path_scanning_us.load(Ordering::Relaxed));
        let prep = Duration::from_micros(self.derivation_prep_us.load(Ordering::Relaxed));
        let exec = Duration::from_micros(self.execute_us.load(Ordering::Relaxed));
        let collect = Duration::from_micros(self.collect_outputs_us.load(Ordering::Relaxed));
        let overhead = total.saturating_sub(exec);

        let exec_pct = if total.as_nanos() > 0 {
            exec.as_secs_f64() / total.as_secs_f64() * 100.0
        } else {
            0.0
        };
        let overhead_pct = 100.0 - exec_pct;

        let avg = |d: Duration| -> f64 { d.as_secs_f64() * 1000.0 / actions as f64 };
        let fmt_dur = |d: Duration| -> String {
            if d.as_secs() >= 60 {
                format!("{}m {:05.2}s", d.as_secs() / 60, d.as_secs_f64() % 60.0)
            } else {
                format!("{:8.3}s", d.as_secs_f64())
            }
        };

        let mut s = String::with_capacity(512);
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
        let _ = writeln!(
            s,
            "  {:.<30} {} ({:>7.1} ms avg)",
            "Output collection",
            fmt_dur(collect),
            avg(collect)
        );
        let _ = writeln!(s, "  {}", "─".repeat(60));
        let _ = writeln!(
            s,
            "  {:.<30} {} ({:>7.1} ms avg)",
            "Total wall-clock",
            fmt_dur(total),
            avg(total)
        );
        let _ = writeln!(
            s,
            "    execution................ {} ({:>5.1}%)",
            fmt_dur(exec),
            exec_pct
        );
        let _ = writeln!(
            s,
            "    overhead (setup+collect). {} ({:>5.1}%)",
            fmt_dur(overhead),
            overhead_pct
        );
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
        let path = match std::env::var("NIXCEPTION_STATS_FILE") {
            Ok(p) if !p.is_empty() => p,
            _ => {
                tracing::debug!("NIXCEPTION_STATS_FILE not set, skipping file output");
                return;
            }
        };

        let content = match self.format_summary() {
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
