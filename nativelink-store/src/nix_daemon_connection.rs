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

//! A pool of lazily-connected, idle-aware **async** connections to the Nix
//! daemon.
//!
//! [`NixDaemonConnectionPool`] manages up to `max_connections` Unix-socket
//! connections to the Nix daemon.  Concurrency is bounded by a
//! [`tokio::sync::Semaphore`]; idle connections are cached in a
//! [`tokio::sync::Mutex`]-protected `Vec` and reaped after 30 seconds of
//! inactivity.
//!
//! All daemon I/O now goes through [`AsyncNixConn`] which uses
//! `tokio::net::UnixStream` — reads and writes are truly async and never
//! block a runtime thread.

use std::sync::{Arc, Weak};
use std::time::Duration;

use bytes::Bytes;
use nativelink_error::{Code, Error, ResultExt, make_err};
use nativelink_util::buf_channel::{DropCloserReadHalf, make_buf_channel_pair};
use nix_compat::store_path::StorePath;
use nix_remote::stderr::Msg;
use nix_remote::worker_op::{
    AddToStore, BuildMode, BuildPaths, BuildResult, BuildStatus, Plain, Resp, WithFramedSource,
    WorkerOp,
};
use nix_remote::{DerivedPath, StorePathSet, ValidPathInfoWithPath};
use tokio::sync::{Mutex, Semaphore, SemaphorePermit};
use tokio::time::Instant;

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::async_nix_conn::AsyncNixConn;

/// The outcome of a `build_derivation` call, classifying success vs
/// failure and whether the failure is worth retrying.
#[derive(Debug)]
pub enum BuildOutcome {
    /// The build succeeded (status was Built, Substituted, AlreadyValid,
    /// or ResolvesToAlreadyValid).
    Success,
    /// The build failed with a status that might succeed on retry
    /// (TransientFailure, MiscFailure, DependencyFailed).  These
    /// correspond to OOM kills, signal-killed builders, and transient
    /// resource exhaustion.
    RetryableFailure {
        status: String,
        message: String,
        path: String,
    },
    /// The build failed permanently (PermanentFailure, InputRejected,
    /// CachedFailure, TimedOut, etc.).  Retrying will not help.
    PermanentFailure {
        status: String,
        message: String,
        path: String,
    },
}

impl fmt::Display for BuildOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Success => write!(f, "build succeeded"),
            Self::RetryableFailure { status, message, path } => {
                write!(f, "retryable build failure for {path}: [{status}] {message}")
            }
            Self::PermanentFailure { status, message, path } => {
                write!(f, "permanent build failure for {path}: [{status}] {message}")
            }
        }
    }
}

impl BuildOutcome {
    /// Returns `true` if this outcome represents a failure that might
    /// succeed on retry.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::RetryableFailure { .. })
    }

    /// Returns `true` if this outcome represents any kind of failure.
    pub fn is_failure(&self) -> bool {
        !matches!(self, Self::Success)
    }

    /// Convert a failure outcome into an [`Error`].  Panics on
    /// `Success`.
    pub fn into_error(self) -> Error {
        match self {
            Self::Success => panic!("BUG: into_error called on Success"),
            Self::RetryableFailure { status, message, path } => {
                make_err!(
                    Code::Unavailable,
                    "Build of {} failed (retryable) with status {}: {}",
                    path,
                    status,
                    message
                )
            }
            Self::PermanentFailure { status, message, path } => {
                make_err!(
                    Code::Internal,
                    "Build of {} failed with status {}: {}",
                    path,
                    status,
                    message
                )
            }
        }
    }
}

/// Classify a [`BuildStatus`] into a [`BuildOutcome`] variant.
fn classify_build_status(
    status: BuildStatus,
    error_msg: &[u8],
    path: &[u8],
) -> BuildOutcome {
    let status_str = format!("{status:?}");
    let message = String::from_utf8_lossy(error_msg).to_string();
    let path_str = String::from_utf8_lossy(path).to_string();

    match status {
        BuildStatus::Built
        | BuildStatus::Substituted
        | BuildStatus::AlreadyValid
        | BuildStatus::ResolvesToAlreadyValid => BuildOutcome::Success,

        // Retryable: transient resource exhaustion, signal kills,
        // dependency killed by OOM in a concurrent build.
        BuildStatus::TransientFailure
        | BuildStatus::MiscFailure
        | BuildStatus::DependencyFailed => BuildOutcome::RetryableFailure {
            status: status_str,
            message,
            path: path_str,
        },

        // Nix reports signal-killed builders (e.g. OOM killer
        // sending SIGKILL) as PermanentFailure, but the error
        // message reveals it was a signal kill.  Reclassify these
        // as retryable since the build may succeed when memory
        // pressure subsides.
        BuildStatus::PermanentFailure
            if message.contains("signal 9")
                || message.contains("Killed")
                || message.contains("signal 6")
                || (message.contains("Cannot build") && message.contains("signal")) =>
        {
            BuildOutcome::RetryableFailure {
                status: status_str,
                message,
                path: path_str,
            }
        }

        // Everything else is permanent.
        _ => BuildOutcome::PermanentFailure {
            status: status_str,
            message,
            path: path_str,
        },
    }
}

/// Default maximum number of pooled connections.
pub const DEFAULT_MAX_CONNECTIONS: usize = 10;

/// How long a connection may sit idle before we close it.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the background reaper wakes up to check for idleness.
const REAPER_INTERVAL: Duration = Duration::from_secs(5);

// -----------------------------------------------------------------------
// Internal types
// -----------------------------------------------------------------------

/// An idle connection together with the timestamp of its last use.
struct IdleConn {
    conn: AsyncNixConn,
    last_used: Instant,
}

/// A checked-out connection.  When the caller is done it should be
/// returned to the pool via [`NixDaemonConnectionPool::release`].
/// If an error occurred during use the connection should simply be
/// dropped (not returned) to avoid reusing corrupted state.
struct CheckedOutConn {
    conn: AsyncNixConn,
}

// -----------------------------------------------------------------------
// Public API
// -----------------------------------------------------------------------

/// A pool of async connections to the Nix daemon.
///
/// Created via [`NixDaemonConnectionPool::new`], which returns an
/// `Arc<Self>` and spawns a background reaper task.
///
/// Each public method transparently acquires a connection from the pool
/// (waiting if `max_connections` are already in use), performs the
/// daemon operation asynchronously, and returns the connection to the
/// pool on success (or discards it on error so that a fresh connection
/// is created next time).
pub struct NixDaemonConnectionPool {
    socket_path: String,
    /// Limits the total number of live connections (in-use + idle).
    semaphore: Semaphore,
    /// Idle, ready-to-use connections.
    idle: Mutex<Vec<IdleConn>>,
    /// Cumulative time (microseconds) spent waiting for a semaphore
    /// permit across all `acquire()` calls.  High values indicate
    /// connection pool contention.
    cumulative_sem_wait_us: AtomicU64,
}

impl fmt::Debug for NixDaemonConnectionPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NixDaemonConnectionPool")
            .field("socket_path", &self.socket_path)
            .field("max_connections", &self.semaphore.available_permits())
            .finish_non_exhaustive()
    }
}

impl NixDaemonConnectionPool {
    /// Create a new connection pool and spawn the idle-reaper task.
    ///
    /// No connections are opened until the first API call.
    pub fn new(socket_path: String, max_connections: usize) -> Arc<Self> {
        let pool = Arc::new(Self {
            socket_path,
            semaphore: Semaphore::new(max_connections),
            idle: Mutex::new(Vec::with_capacity(max_connections)),
            cumulative_sem_wait_us: AtomicU64::new(0),
        });

        // Spawn a lightweight background task that drops idle
        // connections.  It holds only a `Weak` reference so it will
        // stop automatically when the last `Arc` is dropped.
        let weak: Weak<Self> = Arc::downgrade(&pool);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(REAPER_INTERVAL).await;
                let Some(pool) = weak.upgrade() else {
                    break;
                };
                let mut idle = pool.idle.lock().await;
                let before = idle.len();
                idle.retain(|entry| entry.last_used.elapsed() < IDLE_TIMEOUT);
                let removed = before - idle.len();
                if removed > 0 {
                    tracing::debug!(
                        socket_path = %pool.socket_path,
                        removed,
                        remaining = idle.len(),
                        "Reaped idle Nix daemon connections"
                    );
                }
            }
        });

        pool
    }

    /// Convenience constructor using [`DEFAULT_MAX_CONNECTIONS`].
    pub fn new_default(socket_path: String) -> Arc<Self> {
        Self::new(socket_path, DEFAULT_MAX_CONNECTIONS)
    }

    /// Return the socket path this pool connects to.
    pub fn socket_path(&self) -> &str {
        &self.socket_path
    }

    // -------------------------------------------------------------------
    // Public daemon operations
    // -------------------------------------------------------------------

    /// Query the nix daemon for path info about the given store path.
    ///
    /// Returns `Some(ValidPathInfo)` if the path is valid, `None`
    /// otherwise.
    pub async fn query_path_info(
        &self,
        store_path: &str,
    ) -> Result<Option<nix_remote::worker_op::ValidPathInfo>, Error> {
        let (_permit, mut checked_out) = self.acquire().await?;
        let conn = &mut checked_out.conn;

        let result = async {
            let response_type: Resp<nix_remote::worker_op::QueryPathInfoResponse> =
                Default::default();
            let query_op = WorkerOp::QueryPathInfo(
                Plain(nix_remote::StorePath(store_path.to_owned().into())),
                response_type,
            );

            conn.send_worker_op(&query_op).await?;
            conn.drain_stderr().await?;

            let reply = conn.read_query_path_info_response().await?;
            Ok(reply.path)
        }
        .await;

        match result {
            Ok(val) => {
                self.release(checked_out).await;
                Ok(val)
            }
            Err(e) => Err(e),
        }
    }

    /// Upload data to the Nix daemon via the `AddToStore` operation.
    ///
    /// Streams bytes from `reader` and returns the
    /// `ValidPathInfoWithPath` reply on success.
    pub async fn upload_to_nix_daemon(
        &self,
        name: &str,
        cam_str: &str,
        refs: StorePathSet,
        mut reader: DropCloserReadHalf,
        upload_size: usize,
    ) -> Result<ValidPathInfoWithPath, Error> {
        let (_permit, mut checked_out) = self.acquire().await?;
        let conn = &mut checked_out.conn;

        tracing::debug!(
            name = %name,
            ca = %cam_str,
            size = upload_size,
            "upload_to_nix_daemon – starting AddToStore"
        );

        let result = async {
            let add_to_store_op = AddToStore {
                name: nix_remote::StorePath(name.to_string().into()),
                cam_str: nix_remote::StorePath(cam_str.to_owned().into()),
                refs,
                repair: false,
            };
            let response_type: Resp<ValidPathInfoWithPath> = Default::default();
            let worker_op = WorkerOp::AddToStore(WithFramedSource(add_to_store_op), response_type);

            conn.send_worker_op(&worker_op).await?;

            // Stream the framed data to the daemon.
            let mut remaining = upload_size;
            while remaining > 0 {
                let bytes = reader
                    .recv()
                    .await
                    .err_tip(|| "Reading from channel during upload_to_nix_daemon")?;
                let chunk_len = bytes.len();
                conn.streaming_write_len(chunk_len as u64).await?;
                conn.streaming_write_buf(bytes.as_ref()).await?;
                remaining -= chunk_len;
            }
            // End-of-stream marker.
            conn.streaming_write_len(0).await?;
            conn.flush().await?;

            conn.drain_stderr().await?;

            let reply = conn.read_valid_path_info_with_path().await?;
            Ok(reply)
        }
        .await;

        match result {
            Ok(val) => {
                tracing::debug!(
                    name = %name,
                    ca = %cam_str,
                    result_path = ?val.path.0,
                    "upload_to_nix_daemon – success"
                );
                self.release(checked_out).await;
                Ok(val)
            }
            Err(e) => {
                tracing::error!(
                    name = %name,
                    ca = %cam_str,
                    size = upload_size,
                    error = %e,
                    "upload_to_nix_daemon – failed"
                );
                Err(e)
            }
        }
    }

    /// Build a derivation by sending a `BuildPathsWithResults` operation
    /// to the nix daemon and waiting for completion.
    ///
    /// `drv_path` is the absolute store path of the `.drv` file
    /// (e.g. `/nix/store/...-reapi-action.drv`).
    ///
    /// Returns a [`BuildOutcome`] that classifies the result as success,
    /// retryable failure, or permanent failure.  Transport-level errors
    /// (broken daemon connection, protocol errors) are returned as `Err`.
    pub async fn build_derivation(
        &self,
        drv_path: &str,
    ) -> Result<BuildOutcome, Error> {
        let (_permit, mut checked_out) = self.acquire().await?;
        let conn = &mut checked_out.conn;

        let result = async {
            let derived_path = format!("{}!*", drv_path);

            let build_paths = BuildPaths {
                paths: vec![nix_remote::StorePath(derived_path.into())],
                build_mode: BuildMode::Normal,
            };

            let response_type: Resp<Vec<(DerivedPath, BuildResult)>> = Default::default();
            let build_op = WorkerOp::BuildPathsWithResults(Plain(build_paths), response_type);

            conn.send_worker_op(&build_op).await?;

            // Drain stderr.  Build errors are reported via the build
            // result status, not via stderr error messages, so we
            // collect rather than fail on Msg::Error.
            loop {
                let msg = conn.read_stderr_msg().await?;
                match msg {
                    Msg::Last(()) => break,
                    Msg::Error(err) => {
                        return Err(make_err!(
                            Code::Internal,
                            "Nix daemon reported error during build: {}",
                            String::from_utf8_lossy(&err.message)
                        ));
                    }
                    _ => {
                        tracing::debug!(stderr_msg = ?msg, "Nix daemon stderr during build");
                    }
                }
            }

            let results = conn.read_build_paths_with_results_response().await?;

            // Classify the first failure (if any) as the outcome.
            for (path, result) in &results {
                let outcome = classify_build_status(
                    result.status,
                    &result.error_msg.0,
                    path.as_ref(),
                );
                if outcome.is_failure() {
                    return Ok(outcome);
                }
            }

            Ok(BuildOutcome::Success)
        }
        .await;

        match result {
            Ok(val) => {
                self.release(checked_out).await;
                Ok(val)
            }
            Err(e) => Err(e),
        }
    }

    /// Upload an item to the Nix store with explicit content, name, and
    /// references.
    ///
    /// This is a convenience wrapper around [`upload_to_nix_daemon`] that
    /// creates the channel pair internally.
    pub async fn add_to_store(
        &self,
        digest: String,
        content: &[u8],
        name: &str,
        inputs: &[StorePath<String>],
    ) -> Result<ValidPathInfoWithPath, Error> {
        let refs = StorePathSet {
            paths: {
                let mut paths: Vec<nix_remote::StorePath> = inputs
                    .iter()
                    .map(|sp| nix_remote::StorePath(sp.to_absolute_path().into()))
                    .collect();
                paths.sort();
                paths
            },
        };

        let (mut tx, rx) = make_buf_channel_pair();
        tx.send(Bytes::copy_from_slice(content))
            .await
            .err_tip(|| "Failed to send buffer into channel")?;
        tx.send_eof()
            .err_tip(|| "Failed to send EOF into channel")?;

        self.upload_to_nix_daemon(name, &digest, refs, rx, content.len())
            .await
    }

    // -------------------------------------------------------------------
    // Internal helpers
    // -------------------------------------------------------------------

    /// Return the cumulative time (microseconds) spent waiting for
    /// semaphore permits.  Useful for diagnosing connection pool
    /// contention.
    pub fn cumulative_sem_wait_us(&self) -> u64 {
        self.cumulative_sem_wait_us.load(Ordering::Relaxed)
    }

    /// Acquire a connection from the pool.
    ///
    /// Waits for a semaphore permit (which bounds the total number of
    /// live connections), then either reuses an idle connection or
    /// opens a new one.
    ///
    /// The returned [`SemaphorePermit`] **must** be kept alive until the
    /// caller is done with the connection.  Dropping the permit signals
    /// that the connection slot is free (whether the connection was
    /// returned to the pool or discarded).
    async fn acquire(&self) -> Result<(SemaphorePermit<'_>, CheckedOutConn), Error> {
        let sem_start = std::time::Instant::now();
        let permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| make_err!(Code::Internal, "Connection pool semaphore closed"))?;
        self.cumulative_sem_wait_us
            .fetch_add(sem_start.elapsed().as_micros() as u64, Ordering::Relaxed);

        let mut idle = self.idle.lock().await;
        let conn = if let Some(entry) = idle.pop() {
            tracing::trace!(
                socket_path = %self.socket_path,
                idle_remaining = idle.len(),
                "Reusing idle Nix daemon connection"
            );
            entry.conn
        } else {
            drop(idle); // Release the lock before doing async IO.
            tracing::debug!(
                socket_path = %self.socket_path,
                "Opening new Nix daemon connection"
            );
            AsyncNixConn::connect(&self.socket_path).await?
        };

        Ok((permit, CheckedOutConn { conn }))
    }

    /// Return a connection to the idle pool after successful use.
    ///
    /// The semaphore permit (held by the caller via `_permit`) is
    /// released when it goes out of scope — which happens immediately
    /// after this method returns.  The flow is:
    ///
    ///   1. `acquire()` → takes permit, pops/creates connection
    ///   2. caller uses the connection
    ///   3. `release()` → pushes connection back to idle vec
    ///   4. `_permit` drops → permit returned to semaphore
    ///
    /// Because the semaphore tracks concurrent *operations* (not live
    /// connections), idle connections in the vec do not consume permits.
    async fn release(&self, checked_out: CheckedOutConn) {
        let mut idle = self.idle.lock().await;
        idle.push(IdleConn {
            conn: checked_out.conn,
            last_used: Instant::now(),
        });
    }
}
