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

//! A shared, lazily-connected, idle-aware handle to the Nix daemon.
//!
//! [`NixDaemonConnection`] wraps a single Unix-socket connection to the
//! Nix daemon and serialises all operations through a
//! [`tokio::sync::Mutex`].  The connection is established on first use
//! and automatically closed after 30 seconds of inactivity.
//!
//! This module is the only place that talks the nix-remote wire
//! protocol — all `nix_remote::*` imports are confined here.

use std::os::unix::net::UnixStream;
use std::sync::{Arc, Weak};
use std::time::Duration;

use bytes::Bytes;
use nativelink_error::{Code, Error, ResultExt, make_err};
use nativelink_util::buf_channel::{DropCloserReadHalf, make_buf_channel_pair};
use nix_compat::store_path::StorePath;
use nix_remote::nix_client::NixDaemonClient;
use nix_remote::stderr::Msg;
use nix_remote::worker_op::{
    AddToStore, BuildMode, BuildPaths, BuildResult, BuildStatus, Plain, QueryPathInfoResponse,
    Resp, StreamingRecv, WithFramedSource, WorkerOp,
};
use nix_remote::{DerivedPath, StorePathSet, ValidPathInfoWithPath};
use tokio::sync::Mutex;
use tokio::time::Instant;

/// How long the connection may sit idle before we close it.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the background reaper wakes up to check for idleness.
const REAPER_INTERVAL: Duration = Duration::from_secs(5);

// -----------------------------------------------------------------------
// Connection state
// -----------------------------------------------------------------------

struct ConnectionState {
    client: Option<NixDaemonClient<UnixStream, UnixStream>>,
    last_used: Instant,
}

// -----------------------------------------------------------------------
// Public API
// -----------------------------------------------------------------------

/// A shared handle to a single Nix daemon connection.
///
/// Created via [`NixDaemonConnection::new`], which returns an `Arc` and
/// spawns a background task that closes the connection after
/// [`IDLE_TIMEOUT`] of inactivity.
///
/// All public methods acquire an internal mutex, so concurrent callers
/// are safely serialised.
pub struct NixDaemonConnection {
    socket_path: String,
    state: Mutex<ConnectionState>,
}

impl std::fmt::Debug for NixDaemonConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NixDaemonConnection")
            .field("socket_path", &self.socket_path)
            .finish_non_exhaustive()
    }
}

impl NixDaemonConnection {
    /// Create a new connection handle and spawn the idle-reaper task.
    ///
    /// The actual Unix-socket connection is **not** opened until the
    /// first API call.
    pub fn new(socket_path: String) -> Arc<Self> {
        let conn = Arc::new(Self {
            socket_path,
            state: Mutex::new(ConnectionState {
                client: None,
                last_used: Instant::now(),
            }),
        });

        // Spawn a lightweight background task that drops idle
        // connections.  It holds only a `Weak` reference so it will
        // stop automatically when the last `Arc` is dropped.
        let weak: Weak<Self> = Arc::downgrade(&conn);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(REAPER_INTERVAL).await;
                let Some(conn) = weak.upgrade() else {
                    // The connection object has been dropped — exit.
                    break;
                };
                let mut state = conn.state.lock().await;
                if state.client.is_some() && state.last_used.elapsed() >= IDLE_TIMEOUT {
                    tracing::debug!(
                        socket_path = %conn.socket_path,
                        "Closing idle Nix daemon connection"
                    );
                    state.client = None;
                }
            }
        });

        conn
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
        let mut state = self.state.lock().await;
        let client = Self::get_or_connect(&self.socket_path, &mut state)?;

        let response_type: Resp<QueryPathInfoResponse> = Default::default();
        let query_op = &WorkerOp::QueryPathInfo(
            Plain(nix_remote::StorePath(store_path.to_owned().into())),
            response_type.clone(),
        );

        client
            .send_worker_op_to_daemon(query_op)
            .map_err(|e| make_err!(Code::Internal, "Sending QueryPathInfo op to daemon: {}", e))?;

        debug_assert!(!query_op.requires_streaming());

        // Read error messages from the daemon.
        loop {
            let msg = client
                .read_error_msg()
                .map_err(|e| make_err!(Code::Internal, "Reading error msg from daemon: {}", e))?;
            if msg == Msg::Last(()) {
                break;
            }
            tracing::debug!(?msg, "Nix daemon stderr during query_path_info");
        }

        let reply = client
            .read_build_response_from_daemon(&response_type)
            .map_err(|e| make_err!(Code::Internal, "{}", e))?;

        state.last_used = Instant::now();
        Ok(reply.path)
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
        let mut state = self.state.lock().await;
        let client = Self::get_or_connect(&self.socket_path, &mut state)?;

        // Build and send the AddToStore operation.
        let add_to_store_op = AddToStore {
            name: nix_remote::StorePath(name.to_string().into()),
            cam_str: nix_remote::StorePath(cam_str.to_owned().into()),
            refs,
            repair: false,
        };
        let response_type: Resp<ValidPathInfoWithPath> = Default::default();
        let worker_op =
            &WorkerOp::AddToStore(WithFramedSource(add_to_store_op), response_type.clone());

        client
            .send_worker_op_to_daemon(worker_op)
            .map_err(|e| make_err!(Code::Internal, "Sending AddToStore op to daemon: {}", e))?;

        debug_assert!(worker_op.requires_streaming());

        // Stream the data to the daemon.
        let mut remaining = upload_size;
        while remaining > 0 {
            let bytes = reader
                .recv()
                .await
                .err_tip(|| "Reading from channel during upload_to_nix_daemon")?;
            let chunk_len = bytes.len();
            client
                .streaming_write_len(chunk_len as u64)
                .map_err(|e| make_err!(Code::Internal, "Writing chunk length to daemon: {}", e))?;
            client
                .streaming_write_buff(bytes.as_ref(), chunk_len)
                .map_err(|e| make_err!(Code::Internal, "Writing chunk data to daemon: {}", e))?;
            remaining -= chunk_len;
        }
        // Signal end-of-stream and flush.
        client
            .streaming_write_len(0)
            .map_err(|e| make_err!(Code::Internal, "Writing end-of-stream to daemon: {}", e))?;
        client
            .flush()
            .map_err(|e| make_err!(Code::Internal, "Flushing daemon connection: {}", e))?;

        // Read error messages from the daemon.
        loop {
            let msg = client.read_error_msg().map_err(|e| {
                make_err!(
                    Code::Internal,
                    "Reading error msg from daemon during upload: {}",
                    e
                )
            })?;
            if msg == Msg::Last(()) {
                break;
            }
            tracing::debug!(?msg, "Nix daemon stderr during upload");
        }

        let reply = client
            .read_build_response_from_daemon(&response_type)
            .map_err(|e| make_err!(Code::Internal, "{}", e))?;

        state.last_used = Instant::now();
        Ok(reply)
    }

    /// Build a derivation by sending a `BuildPathsWithResults` operation
    /// to the nix daemon and waiting for completion.
    ///
    /// `drv_path` is the absolute store path of the `.drv` file
    /// (e.g. `/nix/store/...-reapi-action.drv`).
    pub async fn build_derivation(
        &self,
        drv_path: &str,
    ) -> Result<Vec<(DerivedPath, BuildResult)>, Error> {
        let mut state = self.state.lock().await;
        let client = Self::get_or_connect(&self.socket_path, &mut state)?;

        // Format the derivation path as a DerivedPath requesting all outputs.
        let derived_path = format!("{}!*", drv_path);

        let build_paths = BuildPaths {
            paths: vec![nix_remote::StorePath(derived_path.into())],
            build_mode: BuildMode::Normal,
        };

        let response_type: Resp<Vec<(DerivedPath, BuildResult)>> = Default::default();
        let build_op = &WorkerOp::BuildPathsWithResults(Plain(build_paths), response_type.clone());

        client.send_worker_op_to_daemon(build_op).map_err(|e| {
            make_err!(
                Code::Internal,
                "Sending BuildPathsWithResults op to daemon: {}",
                e
            )
        })?;

        debug_assert!(!build_op.requires_streaming());

        // Read stderr messages from the daemon until we receive the
        // final `Last(())` sentinel.
        loop {
            let msg = client.read_error_msg().map_err(|e| {
                make_err!(
                    Code::Internal,
                    "Reading stderr msg from daemon during build: {}",
                    e
                )
            })?;
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

        // Read the final response.
        let results: Vec<(DerivedPath, BuildResult)> = client
            .read_build_response_from_daemon(&response_type)
            .map_err(|e| {
                make_err!(
                    Code::Internal,
                    "Reading BuildPathsWithResults response: {}",
                    e
                )
            })?;

        // Check each result for failure.
        for (path, result) in &results {
            match result.status {
                BuildStatus::Built
                | BuildStatus::Substituted
                | BuildStatus::AlreadyValid
                | BuildStatus::ResolvesToAlreadyValid => {
                    // Success — continue.
                }
                _ => {
                    return Err(make_err!(
                        Code::Internal,
                        "Build of {:?} failed with status {:?}: {}",
                        String::from_utf8_lossy(path.as_ref()),
                        result.status,
                        String::from_utf8_lossy(&result.error_msg.0)
                    ));
                }
            }
        }

        state.last_used = Instant::now();
        Ok(results)
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

    /// Return the existing client or open a new connection to the daemon.
    ///
    /// Must be called while the `Mutex` is held (the caller passes the
    /// locked state).
    fn get_or_connect<'a>(
        socket_path: &str,
        state: &'a mut ConnectionState,
    ) -> Result<&'a mut NixDaemonClient<UnixStream, UnixStream>, Error> {
        if state.client.is_none() {
            tracing::debug!(%socket_path, "Opening new Nix daemon connection");
            state.client = Some(Self::connect(socket_path)?);
        }
        // SAFETY: we just ensured `client` is `Some`.
        Ok(state.client.as_mut().unwrap())
    }

    /// Open a fresh Unix-socket connection to the Nix daemon and perform
    /// the handshake.
    fn connect(socket_path: &str) -> Result<NixDaemonClient<UnixStream, UnixStream>, Error> {
        let read_socket = UnixStream::connect(socket_path)
            .err_tip(|| format!("While opening socket '{}'", socket_path))?;
        let write_socket = read_socket
            .try_clone()
            .err_tip(|| "While cloning nix daemon socket")?;
        NixDaemonClient::new(read_socket, write_socket)
            .map_err(|e| make_err!(Code::Internal, "While creating a new NixClient: {}", e))
    }
}
