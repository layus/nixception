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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Extension trait for [`Path`] that adds a `relative_to` method,
/// mirroring the unstable `std::path::Path::relative_to`.
trait PathExt {
    /// Compute a relative path from `base` to `self`.
    ///
    /// Both paths must be absolute.  If `self` is inside `base` the
    /// result is a simple relative path; otherwise appropriate `../`
    /// components are prepended.
    ///
    /// ```text
    /// Path::new("/a/b/c/foo.o").relative_to("/a/b")
    ///   => "c/foo.o"
    /// Path::new("/a/foo.o").relative_to("/a/b/c")
    ///   => "../../foo.o"
    /// ```
    ///
    /// # Panics
    ///
    /// This is only meaningful on Unix where all absolute paths share
    /// the root `/`.  It will produce nonsensical results if either
    /// path is relative.
    fn relative_to(&self, base: &Path) -> PathBuf;
}

impl PathExt for Path {
    fn relative_to(&self, base: &Path) -> PathBuf {
        debug_assert!(self.is_absolute(), "relative_to: self must be absolute, got {:?}", self);
        debug_assert!(base.is_absolute(), "relative_to: base must be absolute, got {:?}", base);
        let self_components: Vec<_> = self.components().collect();
        let base_components: Vec<_> = base.components().collect();
        let common = self_components
            .iter()
            .zip(base_components.iter())
            .take_while(|(a, b)| a == b)
            .count();
        let ups = base_components.len() - common;
        let mut result = PathBuf::new();
        for _ in 0..ups {
            result.push("..");
        }
        for comp in &self_components[common..] {
            result.push(comp);
        }
        result
    }
}

use parking_lot::RwLock;

use nix_compat::derivation::{Derivation, Output};
use nix_compat::nixhash::CAHash;
use nix_compat::store_path::{STORE_DIR_WITH_SLASH, StorePath};

use crate::nix_stats::{NixceptionStats, TimingGuard};
use crate::runner_info::{RunnerInfo, physical_store_path};

use bstr::BString;
use bytes::Bytes;
use nativelink_error::{Code, Error, ResultExt, make_err};
use nativelink_store::ac_utils::get_and_decode_digest;
use nativelink_store::nix_daemon_connection::NixDaemonConnectionPool;
use nativelink_store::nix_store::key_to_store_path;
use nativelink_util::action_messages::{
    ActionInfo, ActionStage, FileInfo, NameOrPath, OperationId, WorkerId,
};
use nativelink_util::common::DigestInfo;
use nativelink_util::digest_hasher::DigestHasher;
use nativelink_util::operation_state_manager::UpdateOperationType;
use nativelink_util::store_trait::{Store, StoreKey, StoreLike};
use serde_json::json;
use tokio::time;
use tracing::{Level, event};

use nativelink_store::nix_daemon_connection::BuildOutcome;

use nativelink_proto::build::bazel::remote::execution::v2::{
    Command as ProtoCommand, Directory as ProtoDirectory,
};

use crate::worker_scheduler::WorkerScheduler;

#[derive(Clone, Debug)]
struct PathEntry {
    path: PathBuf,
    store_path: StorePath<String>,
    digest: DigestInfo,
    is_executable: bool,
}

/// A single action's runner-side timing split, read back from
/// `$out/timing.json`.  All-zero when the runner produced no timing record.
#[derive(Clone, Copy, Debug, Default)]
struct RunnerTiming {
    /// Whether this action was served from the Nix cache (the runner did not
    /// run for *this* build).  Detected when `timing.json` is missing, or its
    /// recorded runner start predates the server's build request (a stale
    /// record from the original execution that the cache hit reused).
    cached: bool,
    /// Latency from the server's `build_derivation` request to the runner
    /// actually starting inside the sandbox (time spent in nix/the daemon).
    /// Zero for cached actions.
    daemon_to_runner: Duration,
    /// Runner setup: read manifest, copy inputs, prepare dirs/env.
    setup: Duration,
    /// The task itself: the command that ran (fork→waitpid).
    task: Duration,
    /// Runner wrap-up: collect declared outputs.
    wrapup: Duration,
}

impl RunnerTiming {
    /// The total time the runner reported for this action (setup + task +
    /// wrap-up).  For a cache hit, this is the *cached* record's runtime — i.e.
    /// what running the action would have cost.
    fn reported_total(&self) -> Duration {
        self.setup + self.task + self.wrapup
    }
}

/// Conservative lower bound on the time a real (non-cached) execution must
/// spend setting up the Nix build sandbox before the runner starts.  Added to
/// the runner's reported runtime when estimating the cost a cache hit avoided.
const SANDBOX_SETUP_FLOOR: Duration = Duration::from_millis(300);

/// Scan a byte slice for all `/nix/store/<hash>-<name>` references and
/// return them as a deduplicated set of [`StorePath`]s.
///
/// The scanner looks for the literal prefix `/nix/store/` and then
/// attempts to parse the longest valid store-path string starting there.
/// Invalid matches (e.g. truncated paths) are silently skipped.
fn scan_nix_store_paths(haystack: &[u8]) -> BTreeSet<StorePath<String>> {
    let prefix = STORE_DIR_WITH_SLASH.as_bytes(); // b"/nix/store/"
    let mut result = BTreeSet::new();
    let mut pos = 0;
    while pos + prefix.len() < haystack.len() {
        // Find the next occurrence of the prefix.
        let Some(start) = haystack[pos..]
            .windows(prefix.len())
            .position(|w| w == prefix)
            .map(|i| i + pos)
        else {
            break;
        };

        // After the prefix we expect 32 nixbase32 chars, a dash, then
        // one or more valid name characters.  Rather than hard-coding
        // the grammar we greedily collect characters that are valid in
        // a store-path name (alphanumeric plus - _ . + ? =) and let
        // `StorePath::from_absolute_path` decide if it's valid.
        let path_start = start;
        let mut end = start + prefix.len();
        while end < haystack.len() {
            let ch = haystack[end];
            if ch.is_ascii_alphanumeric() || matches!(ch, b'-' | b'_' | b'.' | b'+' | b'?' | b'=') {
                end += 1;
            } else {
                break;
            }
        }

        if let Ok(sp) = StorePath::<String>::from_absolute_path(&haystack[path_start..end]) {
            result.insert(sp);
        }

        // Advance past this match to avoid re-matching the same prefix.
        pos = start + prefix.len();
    }
    result
}

/// Scan multiple strings and return the union of all discovered store
/// paths.
#[allow(single_use_lifetimes)] // lifetime required for `impl Trait` parameter
fn scan_nix_store_paths_in_strings<'a>(
    strings: impl IntoIterator<Item = &'a str>,
) -> BTreeSet<StorePath<String>> {
    let mut result = BTreeSet::new();
    for s in strings {
        result.extend(scan_nix_store_paths(s.as_bytes()));
    }
    result
}

/// A concurrent cache mapping Nix store paths (of input files) to the
/// set of `/nix/store/…` references discovered inside their contents.
///
/// Shared (via `Arc`) across all [`NixWorker`] instances spawned by a
/// single [`NixScheduler`](crate::nix_scheduler::NixScheduler).  Because
/// a file's store path is derived deterministically from its CAS digest,
/// the same content always maps to the same key — so results computed by
/// one action are reused by every subsequent action that shares the same
/// input file.
pub(crate) type ScanCache = Arc<RwLock<HashMap<String, BTreeSet<StorePath<String>>>>>;

/// Cached result of resolving a discovered store path via the Nix daemon.
///
/// Currently unconstructed: [`NixWorker::resolve_discovered_store_paths`] no
/// longer queries the daemon for deriver info at all (see its doc comment —
/// the daemon a real sandboxed build talks to always reports no deriver, so
/// resolving it when a *different* caller's daemon happens to know one made
/// otherwise-identical actions hash differently). Kept, still threaded
/// through [`NixWorker`] and `NixScheduler`, in case path-info caching is
/// reintroduced for something else later.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(crate) enum PathInfoCacheEntry {
    /// Path is valid but has no deriver — should be added as input source.
    Source,
    /// Path's deriver was resolved successfully.
    Resolved {
        drv_store_path: StorePath<String>,
        output_name: String,
        hash_derivation_modulo: [u8; 32],
    },
    /// Path was queried but is not valid in the store, or query failed.
    NotFound,
}

/// A concurrent cache mapping store path strings to their resolved
/// path-info results.  Shared across all [`NixWorker`] instances to
/// avoid redundant `query_path_info` + `.drv` read operations.
///
/// See [`PathInfoCacheEntry`]: currently unused for the same reason.
pub(crate) type PathInfoCache = Arc<RwLock<HashMap<String, PathInfoCacheEntry>>>;

/// RAII guard tracking a single live action in the shared in-flight
/// gauge.  Created at the start of [`NixWorker::run`] and dropped when
/// the worker future completes (including on panic/early return), so the
/// gauge always reflects the number of workers actually alive.
struct ActionInFlightGuard(Arc<NixceptionStats>);

impl ActionInFlightGuard {
    fn new(stats: Arc<NixceptionStats>) -> Self {
        stats.gauge_enter(&stats.actions_in_flight, &stats.actions_in_flight_peak);
        Self(stats)
    }
}

impl Drop for ActionInFlightGuard {
    fn drop(&mut self) {
        self.0.gauge_exit(&self.0.actions_in_flight);
    }
}

/// A worker that executes a single action end-to-end, reporting every state
/// transition back to the scheduler through the [`WorkerStateManager`].
///
/// Created by the scheduler in [`NixScheduler::create_running_action`] and
/// then spawned via [`NixWorker::run`].
pub(crate) struct NixWorker {
    /// The state manager shared with the scheduler, used to update action state.
    worker_scheduler: Arc<dyn WorkerScheduler>,
    /// A synthetic worker id assigned to this nix worker instance.
    worker_id: WorkerId,
    /// The operation this worker is executing (assigned by the db).
    operation_id: OperationId,
    /// CAS store used to fetch action inputs and upload derivations.
    cas_store: Store,
    /// Connection pool to the Nix daemon.
    connection: Arc<NixDaemonConnectionPool>,
    /// The action metadata describing what to execute.
    action_info: Arc<ActionInfo>,
    /// Runner metadata for constructing action derivations.
    runner_info: Arc<RunnerInfo>,
    /// Shared cumulative timing statistics.
    stats: Arc<NixceptionStats>,
    /// Shared cache of per-file store-path scan results, avoiding
    /// repeated CAS reads and scanning of identical input files across
    /// actions.
    scan_cache: ScanCache,
    /// Shared cache of resolved path-info results, avoiding redundant
    /// `query_path_info` daemon calls across actions.
    ///
    /// Currently unused — see [`PathInfoCacheEntry`]. Still threaded through
    /// from `NixScheduler` so it's ready if path-info caching is
    /// reintroduced for something else.
    #[allow(dead_code)]
    path_info_cache: PathInfoCache,
}

impl NixWorker {
    pub(crate) fn new(
        worker_scheduler: Arc<dyn WorkerScheduler>,
        worker_id: WorkerId,
        operation_id: OperationId,
        cas_store: Store,
        connection: Arc<NixDaemonConnectionPool>,
        action_info: Arc<ActionInfo>,
        runner_info: Arc<RunnerInfo>,
        stats: Arc<NixceptionStats>,
        scan_cache: ScanCache,
        path_info_cache: PathInfoCache,
    ) -> Self {
        Self {
            worker_scheduler,
            worker_id,
            operation_id,
            cas_store,
            connection,
            action_info,
            runner_info,
            stats,
            scan_cache,
            path_info_cache,
        }
    }

    /// Read a single input file from the CAS, scan its contents for
    /// `/nix/store/…` references, and return the discovered store paths.
    ///
    /// Results are cached in the shared [`ScanCache`] keyed by the
    /// file's Nix store path (which is derived deterministically from
    /// the CAS digest, so identical content always has the same key).
    async fn scan_file_store_paths(
        &self,
        entry: &PathEntry,
    ) -> Result<BTreeSet<StorePath<String>>, Error> {
        let cache_key = entry.store_path.to_absolute_path();

        // Fast path: check if we already scanned this file.
        if let Some(cached) = self.scan_cache.read().get(&cache_key) {
            self.stats.increment(&self.stats.scan_cache_hits);
            return Ok(cached.clone());
        }

        self.stats.increment(&self.stats.scan_cache_misses);

        // Slow path: read from CAS, scan, and cache.
        let cas_start = Instant::now();
        let content = self
            .cas_store
            .get_part_unchunked(StoreKey::Digest(entry.digest), 0, None)
            .await
            .err_tip(|| {
                format!(
                    "Reading input file {} from CAS for store-path scanning",
                    entry.path.display()
                )
            })?;
        self.stats
            .record(&self.stats.scan_cas_read_us, cas_start.elapsed());

        let paths = scan_nix_store_paths(&content);
        self.scan_cache.write().insert(cache_key, paths.clone());
        Ok(paths)
    }

    // ----- state-transition helpers -----

    /// Send a state update, returning an error if the operation is no longer
    /// in the db (e.g. it was already timed-out or cancelled).
    async fn try_update(&self, update: UpdateOperationType) -> Result<(), Error> {
        self.worker_scheduler
            .update_action(&self.worker_id, &self.operation_id, update)
            .await
    }

    /// Fire-and-forget update: sends the update and logs any error internally.
    async fn update(&self, update: UpdateOperationType) {
        if let Err(e) = self.try_update(update).await {
            event!(
                Level::ERROR,
                error = ?e,
                operation_id = ?self.operation_id,
                "Failed to update action state"
            );
        }
    }

    // ----- execution entry-point -----

    /// Execute the action from start to finish.
    ///
    ///  1. Fetch the command and input tree from the CAS.
    ///  2. Build a Nix derivation from the inputs.
    ///  3. Upload the derivation to the Nix store → transition to `Executing`.
    ///  4. TODO: build the derivation and collect outputs.
    ///  5. Mark the action as completed.
    ///
    /// The action's timeout (from [`ActionInfo::timeout`]) is enforced with
    /// [`tokio::time::timeout`].  On timeout the action is marked as
    /// completed with exit code 124 (the standard timeout exit code).
    ///
    /// All outcomes (success, error, timeout) are communicated exclusively
    /// via the [`WorkerStateManager`] — this method returns nothing.
    pub(crate) async fn run(self) {
        let _inflight = ActionInFlightGuard::new(self.stats.clone());
        let timeout_duration = self.action_info.timeout;
        let result = time::timeout(timeout_duration, self.run_inner()).await;

        let stage = match result {
            Ok(Ok(())) => return, // run_inner already sent the Completed update
            Ok(Err(e)) => {
                event!(
                    Level::ERROR,
                    error = ?e,
                    operation_id = ?self.operation_id,
                    "Action failed"
                );
                ActionStage::Completed(nativelink_util::action_messages::ActionResult {
                    error: Some(e),
                    ..Default::default()
                })
            }
            Err(_elapsed) => {
                event!(
                    Level::WARN,
                    operation_id = ?self.operation_id,
                    ?timeout_duration,
                    "Action timed out"
                );
                ActionStage::Completed(nativelink_util::action_messages::ActionResult {
                    exit_code: 124,
                    ..Default::default()
                })
            }
        };

        self.update(UpdateOperationType::UpdateWithActionStage(stage))
            .await;
    }

    /// The actual execution logic, called inside a timeout wrapper.
    ///
    /// On success the action is marked as [`ActionStage::Completed`] before
    /// returning `Ok(())`.  On error an `Err` is returned and the caller
    /// (`run`) is responsible for reporting the failure.
    async fn run_inner(&self) -> Result<(), Error> {
        let action_start = Instant::now();

        // Cost center: prepare_derivation (encompasses scanning, upload,
        // and derivation construction).  Scanning and upload are also
        // recorded individually inside prepare_derivation as sub-costs.
        let _prep_guard = TimingGuard::new(&self.stats, &self.stats.derivation_prep_us);
        let (drv_path, out_path, working_directory) = self.prepare_derivation().await?;
        let prep_elapsed = _prep_guard.elapsed();
        drop(_prep_guard);
        event!(
            Level::DEBUG,
            prep_ms = prep_elapsed.as_millis(),
            "prepare_derivation completed"
        );

        // The derivation has been uploaded — transition Queued → Executing.
        event!(
            Level::DEBUG,
            drv_path = ?drv_path.to_absolute_path(),
            out_path = ?out_path,
            "Derivation uploaded, marking action as Executing"
        );
        self.update(UpdateOperationType::UpdateWithActionStage(
            ActionStage::Executing,
        ))
        .await;

        // Cost center: execute_derivation
        // Capture an absolute wall-clock timestamp for the moment we ask the
        // daemon to build, so we can measure the nix→runner latency (the gap
        // until the runner actually starts inside the build sandbox).
        let build_call_wall = SystemTime::now();
        let exec_start = Instant::now();
        self.execute_derivation(&drv_path).await?;
        let exec_elapsed = exec_start.elapsed();
        event!(
            Level::DEBUG,
            elapsed_ms = exec_elapsed.as_millis(),
            "execute_derivation completed"
        );

        // Fold the runner's own timing record into the stats (best-effort),
        // classify the action as cached vs executed, and keep this action's
        // split for the per-action log below.
        //
        // The build's `out_path` is logical (`/nix/store/...-reapi-action`); the
        // server reads its contents (exitcode/stdout/stderr/outputs/timing.json)
        // from the local filesystem, so relocate it under the chroot store root
        // when one is configured (identity otherwise).
        let physical_out_path = physical_store_path(&out_path);
        let runner = self
            .record_runner_timing(Path::new(&physical_out_path), build_call_wall, exec_elapsed)
            .await;

        // `execute_us` is an "executed actions only" cost center: a cache hit's
        // exec span is tiny but non-zero and would dilute the executed average
        // (its cost is accounted separately in the cache section instead).
        if !runner.cached {
            self.stats.record(&self.stats.execute_us, exec_elapsed);
        }

        // Cost center: collect_outputs
        {
            let _guard = TimingGuard::new(&self.stats, &self.stats.collect_outputs_us);
            let action_result = self
                .collect_action_result(Path::new(&physical_out_path), &working_directory)
                .await?;

            // Record totals before sending the final update.
            let total_elapsed = action_start.elapsed();
            self.stats
                .record(&self.stats.total_action_us, total_elapsed);
            self.stats.increment(&self.stats.actions_total);
            if action_result.exit_code == 0 && action_result.error.is_none() {
                self.stats.increment(&self.stats.actions_succeeded);
            } else {
                self.stats.increment(&self.stats.actions_failed);
            }

            let overhead = total_elapsed.saturating_sub(exec_elapsed);
            event!(
                Level::DEBUG,
                cached = runner.cached,
                total_ms = total_elapsed.as_millis(),
                exec_ms = exec_elapsed.as_millis(),
                overhead_ms = overhead.as_millis(),
                nix_to_runner_ms = runner.daemon_to_runner.as_millis(),
                runner_setup_ms = runner.setup.as_millis(),
                task_ms = runner.task.as_millis(),
                runner_wrapup_ms = runner.wrapup.as_millis(),
                "Action timing breakdown"
            );

            self.try_update(UpdateOperationType::UpdateWithActionStage(
                ActionStage::Completed(action_result),
            ))
            .await?;
        }

        Ok(())
    }

    /// Maximum number of retry attempts for transient build failures
    /// (e.g. OOM kills, signal-killed builders).
    const MAX_BUILD_RETRIES: u32 = 3;

    /// Initial backoff delay between retries (doubles each attempt).
    const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(2);

    /// Build the derivation by sending it to the nix daemon and waiting
    /// for completion.
    ///
    /// Transient failures (OOM kills, signal-killed builders) are
    /// automatically retried up to [`MAX_BUILD_RETRIES`] times with
    /// exponential backoff.  Permanent failures are returned immediately.
    async fn execute_derivation(&self, drv_path: &StorePath<String>) -> Result<(), Error> {
        let drv_abs_path = drv_path.to_absolute_path();
        let mut last_outcome: Option<BuildOutcome> = None;

        for attempt in 0..=Self::MAX_BUILD_RETRIES {
            if attempt > 0 {
                self.stats.increment(&self.stats.retries_attempted);
                let delay = Self::INITIAL_RETRY_DELAY * 2u32.pow(attempt - 1);
                event!(
                    Level::WARN,
                    drv_path = ?drv_abs_path,
                    attempt,
                    delay_ms = delay.as_millis() as u64,
                    previous_failure = %last_outcome.as_ref().unwrap(),
                    "Retrying build after transient failure"
                );
                time::sleep(delay).await;
            }

            // Run the build alongside a periodic keepalive so the state
            // manager doesn't time out long-running compilations.
            let mut keepalive = time::interval(Duration::from_secs(30));
            keepalive.tick().await; // consume the immediate first tick

            let outcome = tokio::select! {
                res = self.connection.build_derivation(&drv_abs_path) => {
                    res.err_tip(|| format!("Building derivation {}", drv_abs_path))?
                }
                _ = async {
                    loop {
                        keepalive.tick().await;
                        event!(
                            Level::DEBUG,
                            drv_path = ?drv_abs_path,
                            "Sending keepalive update during build"
                        );
                        self.update(UpdateOperationType::UpdateWithActionStage(
                            ActionStage::Executing,
                        ))
                        .await;
                    }
                } => {
                    unreachable!("keepalive loop never terminates")
                }
            };

            match &outcome {
                BuildOutcome::Success => {
                    if attempt > 0 {
                        self.stats.increment(&self.stats.retries_succeeded);
                        event!(
                            Level::INFO,
                            drv_path = ?drv_abs_path,
                            attempt,
                            "Build succeeded after retry"
                        );
                    } else {
                        event!(
                            Level::DEBUG,
                            drv_path = ?drv_abs_path,
                            "Build completed successfully"
                        );
                    }
                    return Ok(());
                }
                BuildOutcome::RetryableFailure { status, message, path } => {
                    event!(
                        Level::WARN,
                        drv_path = ?drv_abs_path,
                        status,
                        message,
                        failed_path = path,
                        attempt,
                        max_retries = Self::MAX_BUILD_RETRIES,
                        "Build failed with retryable status"
                    );
                }
                BuildOutcome::PermanentFailure { status, message, path } => {
                    event!(
                        Level::ERROR,
                        drv_path = ?drv_abs_path,
                        status,
                        message,
                        failed_path = path,
                        "Build failed permanently (not retrying)"
                    );
                    return Err(outcome.into_error());
                }
            }

            last_outcome = Some(outcome);
        }

        // Exhausted all retries.
        self.stats.increment(&self.stats.retries_exhausted);
        let outcome = last_outcome.expect("BUG: loop ran at least once");
        event!(
            Level::ERROR,
            drv_path = ?drv_abs_path,
            max_retries = Self::MAX_BUILD_RETRIES,
            last_failure = %outcome,
            "Build failed after exhausting all retries"
        );
        Err(outcome.into_error())
    }

    /// Read the exit code, upload stdout/stderr, walk the outputs
    /// sub-directory, and return a populated [`ActionResult`].
    ///
    /// `out_dir` is the absolute path of the derivation's "out" output
    /// (e.g. `/nix/store/xxx-reapi-action`), which is expected to
    /// contain `exitcode`, `stdout`, `stderr` files and an `outputs/`
    /// sub-directory.
    async fn collect_action_result(
        &self,
        out_dir: &Path,
        working_directory: &str,
    ) -> Result<nativelink_util::action_messages::ActionResult, Error> {
        // Read the exit code produced by the script.
        let read_start = Instant::now();
        let exit_code: i32 = tokio::fs::read_to_string(out_dir.join("exitcode"))
            .await
            .map_err(|e| make_err!(Code::Internal, "Failed to read exitcode: {}", e))?
            .trim()
            .parse::<i32>()
            .map_err(|e| make_err!(Code::Internal, "Failed to parse exitcode: {}", e))?;

        // Upload stdout and stderr to the CAS and obtain their digests.
        let stdout_digest = self
            .upload_file_to_cas(&out_dir.join("stdout"))
            .await
            .err_tip(|| "Uploading stdout to CAS")?;
        let stderr_digest = self
            .upload_file_to_cas(&out_dir.join("stderr"))
            .await
            .err_tip(|| "Uploading stderr to CAS")?;
        self.stats
            .record(&self.stats.output_read_us, read_start.elapsed());

        // Walk $out/outputs/ and collect FileInfo entries.
        // The runner stores outputs under $out/outputs/<working_directory>/…
        // but REAPI requires output file paths relative to the working
        // directory.  We walk from $out/outputs/ but relativize the
        // resulting paths against $out/outputs/<working_directory> so that
        // paths inside the working directory lose the prefix and paths
        // outside it get appropriate ../../… prefixes.
        let outputs_dir = out_dir.join("outputs");
        let relative_to = outputs_dir.join(working_directory);
        let upload_start = Instant::now();
        let output_files = self
            .collect_output_files(&outputs_dir, &relative_to)
            .await
            .err_tip(|| format!("Collecting output files from {}", outputs_dir.display()))?;
        self.stats
            .record(&self.stats.output_upload_us, upload_start.elapsed());

        event!(
            Level::DEBUG,
            num_output_files = output_files.len(),
            exit_code,
            "Output files collected"
        );

        Ok(nativelink_util::action_messages::ActionResult {
            output_files,
            exit_code,
            stdout_digest,
            stderr_digest,
            message: out_dir.to_string_lossy().to_string(),
            ..Default::default()
        })
    }

    /// Read `$out/timing.json` (written by the runner), classify the action as
    /// cached or executed, fold the appropriate measurements into the shared
    /// stats, and return this action's split for the per-action log.
    ///
    /// `exec_elapsed` is the server's own wall-clock for the `build_derivation`
    /// call, used as the "actual" time a cache hit took.
    ///
    /// **Cache detection:** a cache hit means the runner did not run for *this*
    /// build, so either there is no `timing.json`, or the one present is stale
    /// — its `runner_wall_start_ns` predates the moment we requested the build.
    /// Executed actions have a fresh record whose start is at/after the request.
    ///
    /// For executed actions we fold the runner setup / task / wrap-up and the
    /// nix→runner latency into the execution-breakdown cost centers.  For cache
    /// hits we instead accumulate the cache accounting: the actual (tiny) time
    /// it took versus the cost it avoided (the cached record's reported runtime
    /// plus a conservative sandbox-setup floor).
    ///
    /// Best-effort: a missing or malformed file is treated as a cache hit with
    /// zero avoided cost, so builds keep working regardless.
    async fn record_runner_timing(
        &self,
        out_dir: &Path,
        build_call_wall: SystemTime,
        exec_elapsed: Duration,
    ) -> RunnerTiming {
        let build_call_ns = build_call_wall
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);

        let parsed = tokio::fs::read_to_string(out_dir.join("timing.json"))
            .await
            .ok()
            .and_then(|contents| serde_json::from_str::<serde_json::Value>(&contents).ok());

        let split = match parsed {
            None => {
                // No (readable) timing record → treat as a cache hit.
                RunnerTiming {
                    cached: true,
                    ..Default::default()
                }
            }
            Some(timing) => {
                let ns = |key: &str| timing.get(key).and_then(serde_json::Value::as_u64);
                let runner_wall_start = ns("runner_wall_start_ns").unwrap_or(0);
                // Fresh record ⇒ executed; stale (older than our request) or
                // missing start ⇒ the runner did not run for this build.
                let cached = runner_wall_start < build_call_ns;

                RunnerTiming {
                    cached,
                    daemon_to_runner: Duration::from_nanos(
                        runner_wall_start.saturating_sub(build_call_ns),
                    ),
                    setup: Duration::from_nanos(ns("setup_ns").unwrap_or(0)),
                    task: Duration::from_nanos(ns("task_ns").unwrap_or(0)),
                    wrapup: Duration::from_nanos(ns("wrapup_ns").unwrap_or(0)),
                }
            }
        };

        if split.cached {
            // Cache hit: record what it cost (actual exec) vs what it saved
            // (the cached record's runtime + the sandbox-setup we skipped).
            self.stats.increment(&self.stats.actions_cached);
            self.stats.record(&self.stats.cached_actual_us, exec_elapsed);
            self.stats.record(
                &self.stats.cached_would_have_us,
                split.reported_total() + SANDBOX_SETUP_FLOOR,
            );
        } else {
            // Executed: fold the runner-side breakdown into the execution stats.
            self.stats
                .record(&self.stats.daemon_to_runner_us, split.daemon_to_runner);
            self.stats.record(&self.stats.runner_setup_us, split.setup);
            self.stats.record(&self.stats.runner_task_us, split.task);
            self.stats.record(&self.stats.runner_wrapup_us, split.wrapup);
        }

        split
    }

    // ----- derivation helpers -----

    /// Given a set of discovered store paths (from command args, env vars,
    /// etc.), classify each as an action input source.
    ///
    /// This used to query the Nix daemon for each path's deriver and, when
    /// one was found, add the *deriver* (as an `input_derivation`, with a
    /// computed `hash_derivation_modulo`) instead of the path itself — a
    /// more precise, input-addressed reference. That precision turned out to
    /// be unreliable rather than merely unavailable: inside a `recursive-nix`
    /// sandboxed build (i.e. every real `nix build` action nixception
    /// prepares), the daemon it talks to is Nix's own `RestrictedStore`,
    /// which *unconditionally* strips the `deriver` field from every
    /// `queryPathInfo` reply as "impure information"
    /// (`src/libstore/restricted-store.cc`, `queryPathInfoUncached`) — so a
    /// real build's actions always fell into the "no deriver, use as
    /// input_source" branch. Anything resolved from *outside* that sandbox
    /// (e.g. `nix develop`, which talks to the host daemon directly and does
    /// see real deriver info) could instead resolve some of the very same
    /// store paths to `input_derivations` — producing a derivation with a
    /// different structural shape (and hash) for what is otherwise a
    /// byte-identical action, defeating the whole point of caching across
    /// that boundary.
    ///
    /// Every discovered path is now added directly as an input source,
    /// unconditionally. This is less precise than resolving to a deriver
    /// when one happens to be available — Nix can no longer distinguish
    /// "this exact build of gcc" from "any content-identical path already
    /// named /nix/store/<hash>-gcc-..." — but it is deterministic: the same
    /// scan of the same command/environment always classifies every path
    /// the same way, from any calling context, with no daemon round-trip
    /// (and its host-dependent answer) involved at all.
    fn resolve_discovered_store_paths(
        discovered: &BTreeSet<StorePath<String>>,
        already_known_sources: &BTreeSet<StorePath<String>>,
    ) -> Vec<StorePath<String>> {
        discovered
            .iter()
            .filter(|sp| !already_known_sources.contains(*sp))
            .cloned()
            .collect()
    }

    /// Prepare and upload a Nix derivation for this action:
    ///  1. Fetch the command and input tree from the CAS
    ///  2. Build a Nix derivation from the inputs
    ///  3. Upload the derivation to the Nix store
    ///
    /// Returns a tuple of (derivation store path, output store path,
    /// working directory) on success.
    async fn prepare_derivation(&self) -> Result<(StorePath<String>, String, String), Error> {
        let cas_fetch_start = Instant::now();
        let command = get_and_decode_digest::<ProtoCommand>(
            &self.cas_store,
            self.action_info.command_digest.into(),
        )
        .await
        .err_tip(|| "Converting command_digest to Command")?;
        self.stats
            .record(&self.stats.cas_proto_fetch_us, cas_fetch_start.elapsed());

        let unfold_start = Instant::now();
        let mut entries: Vec<PathEntry> = Vec::new();
        self.unfold(
            "./".into(),
            &self.action_info.input_root_digest,
            &mut entries,
        )
        .await
        .err_tip(|| "Converting digest to Directory")?;
        self.stats
            .record(&self.stats.unfold_us, unfold_start.elapsed());

        // ── Discover Nix store paths referenced by the action ───────────
        //
        // Scan the command arguments and environment variable values for
        // /nix/store/… references.  These may come from e.g.
        // rules_nixpkgs resolving a CC toolchain to a concrete store
        // path.  Without adding them to the derivation's inputs the Nix
        // sandbox would not contain them and the action would fail.
        // Cost center: store-path scanning and resolution.
        let scan_start = Instant::now();

        let mut discovered_store_paths =
            scan_nix_store_paths_in_strings(command.arguments.iter().map(String::as_str));
        discovered_store_paths.extend(scan_nix_store_paths_in_strings(
            command
                .environment_variables
                .iter()
                .flat_map(|var| [var.name.as_str(), var.value.as_str()]),
        ));

        // Also scan the contents of every input file for /nix/store/…
        // references.  Input files such as shell scripts, wrapper
        // binaries, or pkg-config files may embed store paths that must
        // be available in the Nix sandbox.  Results are cached per file
        // (keyed by the file's Nix store path) so that files shared
        // across actions are only read and scanned once.
        for entry in &entries {
            let file_paths = self.scan_file_store_paths(entry).await.err_tip(|| {
                format!(
                    "Scanning input file {} for store-path references",
                    entry.path.display()
                )
            })?;
            discovered_store_paths.extend(file_paths);
        }

        // Extra paths configured via NIXCEPTION_EXTRA_SANDBOX_PATHS (e.g. a
        // toolchain the executed command needs but that isn't otherwise
        // referenced anywhere the scanner above looks). Resolved identically
        // to any other discovered path below.
        discovered_store_paths.extend(self.runner_info.extra_sandbox_paths.iter().cloned());

        // The CAS entries are already going into input_sources; exclude
        // them from the "discovered" set so we don't query the daemon
        // for paths we already own.
        let cas_input_sources: BTreeSet<StorePath<String>> =
            entries.iter().map(|e| e.store_path.to_owned()).collect();

        // Every discovered store path becomes an input source directly — see
        // resolve_discovered_store_paths for why we no longer try to resolve
        // any of them to a deriver.
        //
        // The hash cache still only needs to carry the runner's own
        // pre-computed hash; nothing else populates it now that no deriver
        // is ever resolved.
        let ri = &self.runner_info;
        let hash_cache: HashMap<String, [u8; 32]> = HashMap::from([(
            ri.drv_store_path.to_absolute_path(),
            ri.hash_derivation_modulo,
        )]);

        let resolve_start = Instant::now();
        let extra_sources =
            Self::resolve_discovered_store_paths(&discovered_store_paths, &cas_input_sources);
        self.stats
            .record(&self.stats.query_path_info_us, resolve_start.elapsed());

        let scan_elapsed = scan_start.elapsed();
        self.stats
            .record(&self.stats.store_path_scanning_us, scan_elapsed);
        event!(
            Level::DEBUG,
            elapsed_ms = scan_elapsed.as_millis(),
            num_discovered = discovered_store_paths.len(),
            "Store-path scanning completed"
        );

        if !extra_sources.is_empty() {
            event!(
                Level::DEBUG,
                num_extra_sources = extra_sources.len(),
                "Discovered Nix store path dependencies in action"
            );
        }

        // ── Build the JSON manifest ────────────────────────────────────
        //
        // The manifest is passed to the builder via Nix's passAsFile
        // mechanism: the derivation environment contains
        //   passAsFile = "manifest"
        //   manifest   = <JSON>
        // and the Nix daemon writes the JSON to a temporary file, setting
        // $manifestPath in the builder's environment.
        let command_working_directory = command.working_directory.clone();

        let manifest_start = Instant::now();
        let manifest = json!({
            "inputs": entries.iter().map(|e| {
                json!({
                    "store_path": e.store_path.to_absolute_path(),
                    "path": e.path.to_string_lossy(),
                    "executable": e.is_executable,
                })
            }).collect::<Vec<_>>(),
            "working_directory": command.working_directory,
            "output_directories": command.output_directories,
            "output_files": command.output_files,
            "output_paths": command.output_paths,
            "environment": command.environment_variables.into_iter()
                .map(|var| (var.name, serde_json::Value::String(var.value)))
                .collect::<serde_json::Map<String, serde_json::Value>>(),
            "command": command.arguments,
        });

        let manifest_str = manifest.to_string();

        let outputs = BTreeMap::from([("out".to_string(), Output::default())]);

        // The derivation environment uses passAsFile so that the (potentially
        // large) manifest is written to a file by the Nix daemon rather than
        // passed as an environment variable.  The C++ runner reads
        // $manifestPath at startup.
        let environment = BTreeMap::from([
            (
                "manifest".into(),
                BString::new(manifest_str.as_bytes().to_vec()),
            ),
            ("passAsFile".into(), BString::from("manifest")),
            ("out".into(), BString::default()),
        ]);

        // ── Build input_derivations ────────────────────────────────────
        //
        // Just the runner — no discovered store path is ever resolved to a
        // deriver now (see resolve_discovered_store_paths).
        let input_derivations: BTreeMap<StorePath<String>, BTreeSet<String>> =
            BTreeMap::from([(ri.drv_store_path.clone(), BTreeSet::from(["out".into()]))]);

        // ── Build input_sources ────────────────────────────────────────
        //
        // CAS entries plus every other discovered store path.
        let mut input_sources = cas_input_sources;
        input_sources.extend(extra_sources);

        let mut derivation: Derivation = Derivation {
            // No arguments needed — the C++ runner reads $manifestPath.
            arguments: vec![],
            builder: ri.builder_path.clone().into(),
            environment,
            input_derivations,
            input_sources,
            outputs,
            system: ri.system.clone().into(),
        };

        // ── Compute hash_derivation_modulo ─────────────────────────────
        //
        // The closure must return the hash for every input derivation.
        // input_derivations now only ever contains the runner (see
        // resolve_discovered_store_paths), whose hash we pre-populated
        // above, so lookup is infallible.
        let hash_modulo = derivation.hash_derivation_modulo(|input_drv_path| {
            let abs = input_drv_path.to_absolute_path();
            *hash_cache
                .get(&abs)
                .unwrap_or_else(|| panic!("BUG: hash_derivation_modulo not pre-computed for {abs}"))
        });
        derivation
            .calculate_output_paths("reapi-action", &hash_modulo)
            .map_err(|e| make_err!(Code::Internal, "Failed to calculate output paths: {}", e))?;

        let out_path = derivation
            .outputs
            .get("out")
            .and_then(|o| o.path.as_ref())
            .map(|sp| sp.to_absolute_path())
            .ok_or_else(|| {
                make_err!(
                    Code::Internal,
                    "Derivation has no 'out' output path after calculate_output_paths"
                )
            })?;

        let drv_path = derivation
            .calculate_derivation_path("reapi-action")
            .map_err(|e| make_err!(Code::Internal, "Failed to calculate derivation path: {}", e))?;

        let serialized_derivation = derivation.to_aterm_bytes();
        let references: Vec<StorePath<String>> = derivation
            .input_sources
            .into_iter()
            .chain(derivation.input_derivations.into_keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        self.stats
            .record(&self.stats.manifest_and_hash_us, manifest_start.elapsed());

        // Cost center: upload derivation to the Nix store.
        let upload_start = Instant::now();
        let drv_abs_path = drv_path.to_absolute_path();
        match self
            .connection
            .add_to_store(
                CAHash::Text([0; 32]).to_nix_nixbase32_string(),
                &serialized_derivation,
                "reapi-action.drv",
                &references,
            )
            .await
        {
            Ok(_) => {}
            Err(e) => {
                // The drv may already exist in the sandbox from a concurrent
                // action (same content → same store path).  If it's valid,
                // we can proceed.
                if self
                    .connection
                    .query_path_info(&drv_abs_path)
                    .await?
                    .is_some()
                {
                    event!(
                        Level::DEBUG,
                        drv_path = %drv_abs_path,
                        "Derivation already exists in store, reusing"
                    );
                } else {
                    return Err(e);
                }
            }
        }
        let upload_elapsed = upload_start.elapsed();
        self.stats
            .record(&self.stats.upload_to_store_us, upload_elapsed);
        event!(
            Level::DEBUG,
            elapsed_ms = upload_elapsed.as_millis(),
            "Derivation upload to nix store completed"
        );

        // The remaining prepare_derivation time (fetching command,
        // unfolding inputs, building manifest, hashing) is "derivation
        // prep" overhead.  We approximate it as total prepare time minus
        // the scan and upload portions that were already recorded.
        // This is done by the caller (run_inner records the top-level
        // prepare_derivation span and subtracts).

        Ok((drv_path, out_path, command_working_directory))
    }

    /// Read a file from disk, upload it to the CAS store, and return its
    /// digest.
    async fn upload_file_to_cas(&self, path: &Path) -> Result<DigestInfo, Error> {
        let content = tokio::fs::read(path)
            .await
            .map_err(|e| make_err!(Code::Internal, "Failed to read {}: {}", path.display(), e))?;

        let digest_function = self.action_info.unique_qualifier.digest_function();
        let mut hasher = digest_function.hasher();
        hasher.update(&content);
        let digest = hasher.finalize_digest();

        self.cas_store
            .update_oneshot(StoreKey::Digest(digest), Bytes::from(content))
            .await
            .err_tip(|| format!("Uploading {} to CAS", path.display()))?;

        Ok(digest)
    }

    /// Walk the Nix output directory, upload every regular file to the CAS
    /// store, and return a [`FileInfo`] for each one.
    ///
    /// `out_dir` is the absolute path of the derivation's outputs
    /// sub-directory (e.g. `/nix/store/xxx-reapi-action/outputs`).
    ///
    /// `relative_to` is the base path against which output file paths are
    /// relativized.  Typically this is `out_dir` joined with the REAPI
    /// working directory, so that paths inside the working directory lose
    /// the prefix while paths outside get `../../…` prefixes.
    async fn collect_output_files(
        &self,
        out_dir: &Path,
        relative_to: &Path,
    ) -> Result<Vec<FileInfo>, Error> {
        let mut files: Vec<FileInfo> = Vec::new();
        self.walk_and_upload(relative_to, out_dir, &mut files)
            .await?;
        Ok(files)
    }

    /// Recursively walk `current_dir`, uploading each regular file to the
    /// CAS and appending a [`FileInfo`] to `files`.  Paths in the
    /// resulting `FileInfo` entries are relative to `relative_to`.
    ///
    /// `relative_to` may differ from the walk root when the REAPI command
    /// uses a working directory: files inside the working directory get
    /// simple relative paths while files outside get `../../…` prefixes.
    async fn walk_and_upload(
        &self,
        relative_to: &Path,
        current_dir: &Path,
        files: &mut Vec<FileInfo>,
    ) -> Result<(), Error> {
        let mut read_dir = tokio::fs::read_dir(current_dir).await.map_err(|e| {
            make_err!(
                Code::Internal,
                "Failed to read directory {}: {}",
                current_dir.display(),
                e
            )
        })?;

        while let Some(entry) = read_dir.next_entry().await.map_err(|e| {
            make_err!(
                Code::Internal,
                "Failed to read dir entry in {}: {}",
                current_dir.display(),
                e
            )
        })? {
            let file_type = entry.file_type().await.map_err(|e| {
                make_err!(
                    Code::Internal,
                    "Failed to get file type for {:?}: {}",
                    entry.path(),
                    e
                )
            })?;

            let path = entry.path();

            if file_type.is_dir() {
                Box::pin(self.walk_and_upload(relative_to, &path, files)).await?;
            } else if file_type.is_file() {
                let relative_path = path
                    .relative_to(relative_to)
                    .to_string_lossy()
                    .into_owned();

                let digest = self
                    .upload_file_to_cas(&path)
                    .await
                    .err_tip(|| format!("Uploading output file {} to CAS", relative_path))?;

                let metadata = entry.metadata().await.map_err(|e| {
                    make_err!(
                        Code::Internal,
                        "Failed to read metadata for {}: {}",
                        path.display(),
                        e
                    )
                })?;
                let is_executable = metadata.permissions().mode() & 0o111 != 0;

                files.push(FileInfo {
                    name_or_path: NameOrPath::Path(relative_path),
                    digest,
                    is_executable,
                });
            }
            // Symlinks and other special files are silently skipped.
        }

        Ok(())
    }

    /// Recursively walk a CAS directory tree and collect every file as a
    /// [`PathEntry`] (relative path + Nix store path).
    async fn unfold<'a>(
        &self,
        root: PathBuf,
        digest: &DigestInfo,
        entries: &'a mut Vec<PathEntry>,
    ) -> Result<&'a mut Vec<PathEntry>, Error> {
        let directory =
            get_and_decode_digest::<ProtoDirectory>(&self.cas_store, digest.into()).await?;

        for file in directory.files {
            let digest: DigestInfo = file
                .digest
                .err_tip(|| "Expected Digest to exist in Directory::directories::digest")?
                .try_into()
                .err_tip(|| "In Directory::file::digest")?;
            entries.push(PathEntry {
                path: root.join(file.name),
                store_path: key_to_store_path(&StoreKey::Digest(digest))?,
                digest,
                is_executable: file.is_executable,
            });
        }

        for dir in directory.directories {
            let digest: DigestInfo = dir
                .digest
                .err_tip(|| "Expected Digest to exist in Directory::directories::digest")?
                .try_into()
                .err_tip(|| "In Directory::file::digest")?;

            Box::pin(self.unfold(root.join(dir.name), &digest, entries)).await?;
        }

        Ok(entries)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid 32-char nixbase32 hash for use in test store paths.
    const HASH_A: &str = "00000000000000000000000000000000";
    const HASH_B: &str = "1111111111111111111111111111111a";
    const HASH_C: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz0";

    #[test]
    fn scan_basic_store_path() {
        let input = format!("/nix/store/{HASH_A}-hello-1.0");
        let paths = scan_nix_store_paths(input.as_bytes());
        assert_eq!(paths.len(), 1);
        let sp = paths.into_iter().next().unwrap();
        assert_eq!(sp.to_absolute_path(), input);
    }

    #[test]
    fn scan_store_path_with_trailing_slash() {
        // The scanner should extract just the store path root, not the
        // trailing /bin/gcc part (slashes are not valid name characters).
        let input = format!("/nix/store/{HASH_A}-gcc-13.2.0/bin/gcc");
        let paths = scan_nix_store_paths(input.as_bytes());
        assert_eq!(paths.len(), 1);
        let sp = paths.into_iter().next().unwrap();
        assert_eq!(
            sp.to_absolute_path(),
            format!("/nix/store/{HASH_A}-gcc-13.2.0")
        );
    }

    #[test]
    fn scan_multiple_paths_in_one_string() {
        let input = format!(
            "PATH=/nix/store/{HASH_A}-coreutils-9.4/bin:/nix/store/{HASH_B}-gcc-13.2.0/bin"
        );
        let paths = scan_nix_store_paths(input.as_bytes());
        assert_eq!(paths.len(), 2);
        let abs: Vec<String> = paths.into_iter().map(|p| p.to_absolute_path()).collect();
        assert!(abs.contains(&format!("/nix/store/{HASH_A}-coreutils-9.4")));
        assert!(abs.contains(&format!("/nix/store/{HASH_B}-gcc-13.2.0")));
    }

    #[test]
    fn scan_deduplicates_identical_paths() {
        let path = format!("/nix/store/{HASH_A}-hello-1.0");
        let input = format!("{path} {path} {path}");
        let paths = scan_nix_store_paths(input.as_bytes());
        assert_eq!(paths.len(), 1);
    }

    #[test]
    fn scan_ignores_truncated_path() {
        // Only the prefix — no hash or name follows.
        let paths = scan_nix_store_paths(b"/nix/store/");
        assert!(paths.is_empty());
    }

    #[test]
    fn scan_ignores_invalid_hash() {
        // Spaces are not valid nixbase32 characters.
        let paths = scan_nix_store_paths(b"/nix/store/not a valid hash-name");
        assert!(paths.is_empty());
    }

    #[test]
    fn scan_empty_input() {
        let paths = scan_nix_store_paths(b"");
        assert!(paths.is_empty());
    }

    #[test]
    fn scan_no_store_paths() {
        let paths = scan_nix_store_paths(b"gcc -O2 -Wall -o hello hello.c");
        assert!(paths.is_empty());
    }

    #[test]
    fn scan_env_var_style_strings() {
        let strings = vec![
            format!("CC=/nix/store/{HASH_A}-gcc-wrapper-13/bin/gcc"),
            format!("CXX=/nix/store/{HASH_A}-gcc-wrapper-13/bin/g++"),
            "LANG=C.UTF-8".to_string(),
            format!("PATH=/nix/store/{HASH_B}-coreutils-9.4/bin:/nix/store/{HASH_C}-bash-5.2/bin"),
        ];
        let paths = scan_nix_store_paths_in_strings(strings.iter().map(String::as_str));
        // gcc-wrapper appears in two vars but is the same store path → 1
        // coreutils → 1, bash → 1
        assert_eq!(paths.len(), 3);
    }

    #[test]
    fn scan_path_at_end_of_buffer() {
        // Ensure we don't panic when the store path reaches the very end
        // of the buffer without any trailing character.
        let input = format!("/nix/store/{HASH_A}-pkg");
        let paths = scan_nix_store_paths(input.as_bytes());
        assert_eq!(paths.len(), 1);
    }

    #[test]
    fn scan_adjacent_paths_separated_by_colon() {
        // Colons are not valid store-path name characters, so they act
        // as separators.
        let input = format!("/nix/store/{HASH_A}-aaa:/nix/store/{HASH_B}-bbb");
        let paths = scan_nix_store_paths(input.as_bytes());
        assert_eq!(paths.len(), 2);
    }
}
