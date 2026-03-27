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

//! Fully async Nix daemon connection using [`tokio::net::UnixStream`].
//!
//! This module replaces the synchronous [`nix_remote::nix_client::NixDaemonClient`]
//! for use inside the [`NixDaemonConnectionPool`](super::nix_daemon_connection::NixDaemonConnectionPool).
//! All reads and writes go through tokio's async I/O, so they never block the
//! runtime thread pool — eliminating the thread-starvation deadlock that occurs
//! when many concurrent workers all perform blocking daemon I/O.
//!
//! # Design
//!
//! The Nix daemon wire protocol is built on two primitives:
//!
//! * **u64** — 8 bytes, little-endian
//! * **bytes** — a u64 length prefix, followed by the payload, zero-padded to
//!   an 8-byte boundary
//!
//! Structs are the concatenation of their fields. Sequences are a u64 count
//! followed by the elements.  Tagged enums are a u64 discriminant followed by
//! the variant body.
//!
//! For **writes** (sending operations to the daemon) we reuse the existing
//! synchronous [`nix_remote::to_vec`] to serialise into a `Vec<u8>` and then
//! `write_all().await` on the tokio stream.
//!
//! For **reads** (responses and stderr messages) we implement async parsers for
//! each concrete ide used by the pool, built on top of the two wire-format
//! primitives above.

use nativelink_error::{Code, Error, ResultExt, make_err};
use nix_remote::stderr::{
    LoggerField, LoggerFields, Msg, StderrError, StderrResult, StderrStartActivity, Trace,
};
use nix_remote::worker_op::{
    BuildResult, BuildStatus, DrvOutputs, QueryPathInfoResponse, ValidPathInfo, WorkerOp,
};
use nix_remote::{
    DerivedPath, NarHash, NixByteBuf, NixString, StorePath, StorePathSet, StringSet,
    ValidPathInfoWithPath,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

// -----------------------------------------------------------------------
// Constants (mirrored from nix-remote/src/lib.rs)
// -----------------------------------------------------------------------

const WORKER_MAGIC_1: u64 = 0x6e697863;
const WORKER_MAGIC_2: u64 = 0x6478696f;

/// Protocol version we advertise.  Must match what the `nix-remote` crate
/// uses (major=1, minor=35 → `(1 << 8) | 35`).
const PROTOCOL_VERSION: u64 = (1 << 8) | 35;

// -----------------------------------------------------------------------
// AsyncNixConn
// -----------------------------------------------------------------------

/// A fully-async connection to the Nix daemon.
///
/// Created via [`AsyncNixConn::connect`] which opens a Unix socket and
/// performs the protocol handshake.
pub struct AsyncNixConn {
    reader: BufReader<OwnedReadHalf>,
    writer: BufWriter<OwnedWriteHalf>,
}

impl std::fmt::Debug for AsyncNixConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncNixConn").finish_non_exhaustive()
    }
}

// -----------------------------------------------------------------------
// Wire-format primitives
// -----------------------------------------------------------------------

impl AsyncNixConn {
    // ── reading ────────────────────────────────────────────────────────

    /// Read a little-endian u64.
    #[inline]
    async fn read_u64(&mut self) -> Result<u64, Error> {
        let mut buf = [0u8; 8];
        self.reader
            .read_exact(&mut buf)
            .await
            .err_tip(|| "async_nix_conn: read_u64")?;
        Ok(u64::from_le_bytes(buf))
    }

    /// Read a bool (encoded as u64, 0 = false).
    #[inline]
    async fn read_bool(&mut self) -> Result<bool, Error> {
        Ok(self.read_u64().await? != 0)
    }

    /// Read a length-prefixed, 8-byte-padded byte buffer.
    async fn read_bytes(&mut self) -> Result<Vec<u8>, Error> {
        let len = self.read_u64().await? as usize;
        let padded = (len + 7) & !7;
        let mut buf = vec![0u8; padded];
        self.reader
            .read_exact(&mut buf)
            .await
            .err_tip(|| "async_nix_conn: read_bytes payload")?;
        buf.truncate(len);
        Ok(buf)
    }

    /// Read a [`NixString`].
    #[inline]
    async fn read_nix_string(&mut self) -> Result<NixString, Error> {
        Ok(NixString(NixByteBuf::from(self.read_bytes().await?)))
    }

    /// Read a [`StorePath`] (wire-identical to a `NixString`).
    #[inline]
    async fn read_store_path(&mut self) -> Result<StorePath, Error> {
        Ok(StorePath(self.read_nix_string().await?))
    }

    /// Read a [`NarHash`] (wire-identical to a byte buffer).
    #[inline]
    async fn read_nar_hash(&mut self) -> Result<NarHash, Error> {
        Ok(NarHash {
            data: NixByteBuf::from(self.read_bytes().await?),
        })
    }

    /// Read a [`StorePathSet`] — `Vec<StorePath>` with a length prefix.
    async fn read_store_path_set(&mut self) -> Result<StorePathSet, Error> {
        let count = self.read_u64().await? as usize;
        let mut paths = Vec::with_capacity(count);
        for _ in 0..count {
            paths.push(self.read_store_path().await?);
        }
        Ok(StorePathSet { paths })
    }

    /// Read a [`StringSet`] — `Vec<NixString>` with a length prefix.
    async fn read_string_set(&mut self) -> Result<StringSet, Error> {
        let count = self.read_u64().await? as usize;
        let mut paths = Vec::with_capacity(count);
        for _ in 0..count {
            paths.push(self.read_nix_string().await?);
        }
        Ok(StringSet { paths })
    }

    // ── writing ────────────────────────────────────────────────────────

    /// Write a little-endian u64.
    #[inline]
    async fn write_u64(&mut self, v: u64) -> Result<(), Error> {
        self.writer
            .write_all(&v.to_le_bytes())
            .await
            .err_tip(|| "async_nix_conn: write_u64")
    }

    /// Write a length-prefixed, 8-byte-padded byte buffer.
    #[allow(dead_code)]
    async fn write_bytes(&mut self, data: &[u8]) -> Result<(), Error> {
        let len = data.len();
        self.write_u64(len as u64).await?;
        self.writer
            .write_all(data)
            .await
            .err_tip(|| "async_nix_conn: write_bytes payload")?;
        let pad = (8 - (len % 8)) % 8;
        if pad > 0 {
            self.writer
                .write_all(&[0u8; 8][..pad])
                .await
                .err_tip(|| "async_nix_conn: write_bytes padding")?;
        }
        Ok(())
    }

    /// Flush the write buffer.
    #[inline]
    pub async fn flush(&mut self) -> Result<(), Error> {
        self.writer
            .flush()
            .await
            .err_tip(|| "async_nix_conn: flush")
    }
}

// -----------------------------------------------------------------------
// Protocol-level operations
// -----------------------------------------------------------------------

impl AsyncNixConn {
    /// Open a connection and perform the protocol handshake.
    pub async fn connect(socket_path: &str) -> Result<Self, Error> {
        let stream = UnixStream::connect(socket_path)
            .await
            .err_tip(|| format!("async_nix_conn: connecting to {socket_path}"))?;

        let (read_half, write_half) = stream.into_split();
        let mut conn = Self {
            reader: BufReader::new(read_half),
            writer: BufWriter::new(write_half),
        };

        conn.handshake().await?;
        Ok(conn)
    }

    /// Perform the nix daemon handshake.
    async fn handshake(&mut self) -> Result<(), Error> {
        // Send WORKER_MAGIC_1
        self.write_u64(WORKER_MAGIC_1).await?;
        self.flush().await?;

        // Read WORKER_MAGIC_2
        let magic = self.read_u64().await?;
        if magic != WORKER_MAGIC_2 {
            return Err(make_err!(
                Code::Internal,
                "async_nix_conn: unexpected WORKER_MAGIC_2: got {magic:#x}"
            ));
        }

        // Read daemon protocol version
        let daemon_version = self.read_u64().await?;
        if daemon_version < PROTOCOL_VERSION {
            return Err(make_err!(
                Code::Internal,
                "async_nix_conn: protocol version too old: {daemon_version}"
            ));
        }

        // Send our protocol version + obsolete fields
        self.write_u64(PROTOCOL_VERSION).await?;
        self.write_u64(0).await?; // cpu affinity (obsolete)
        self.write_u64(0).await?; // reserve space (obsolete)
        self.flush().await?;

        // Read daemon version string
        let _version_string = self.read_nix_string().await?;
        tracing::info!(
            "Proxy daemon is: {}",
            String::from_utf8_lossy(_version_string.0.as_ref())
        );

        // Read trusted flag
        let _trusted = self.read_u64().await?;

        // Drain stderr messages until Last
        loop {
            let msg = self.read_stderr_msg().await?;
            if matches!(msg, Msg::Last(())) {
                break;
            }
        }

        Ok(())
    }

    // ── Sending operations ─────────────────────────────────────────────

    /// Serialise a [`WorkerOp`] and send it to the daemon.
    ///
    /// Uses the existing synchronous `nix_remote::to_vec` on in-memory
    /// data (no I/O), then writes asynchronously.
    pub async fn send_worker_op(&mut self, op: &WorkerOp) -> Result<(), Error> {
        let bytes = nix_remote::to_vec(op)
            .map_err(|e| make_err!(Code::Internal, "async_nix_conn: serialising WorkerOp: {e}"))?;
        self.writer
            .write_all(&bytes)
            .await
            .err_tip(|| "async_nix_conn: sending WorkerOp")?;
        self.flush().await?;
        Ok(())
    }

    /// Write a framed-source chunk length (used by `AddToStore`).
    #[inline]
    pub async fn streaming_write_len(&mut self, len: u64) -> Result<(), Error> {
        self.write_u64(len).await
    }

    /// Write raw framed-source data bytes.
    pub async fn streaming_write_buf(&mut self, data: &[u8]) -> Result<(), Error> {
        self.writer
            .write_all(data)
            .await
            .err_tip(|| "async_nix_conn: streaming_write_buf")
    }

    // ── Reading stderr messages ────────────────────────────────────────

    /// Read a single stderr [`Msg`] from the daemon.
    pub async fn read_stderr_msg(&mut self) -> Result<Msg, Error> {
        let tag = self.read_u64().await?;
        match tag {
            // STDERR_LAST
            0x616c7473 => Ok(Msg::Last(())),

            // STDERR_WRITE
            0x64617416 => {
                let data = self.read_nix_string().await?;
                Ok(Msg::Write(data))
            }

            // STDERR_NEXT
            0x6f6c6d67 => {
                let data = self.read_nix_string().await?;
                Ok(Msg::Next(data))
            }

            // STDERR_ERROR
            0x63787470 => {
                let id_bytes = self.read_bytes().await?;
                let level = self.read_u64().await?;
                let name_bytes = self.read_bytes().await?;
                let message_bytes = self.read_bytes().await?;
                let have_pos = self.read_u64().await?;
                let num_traces = self.read_u64().await? as usize;
                let mut traces = Vec::with_capacity(num_traces);
                for _ in 0..num_traces {
                    let t_have_pos = self.read_u64().await?;
                    let t_trace = self.read_bytes().await?;
                    traces.push(Trace {
                        have_pos: t_have_pos,
                        trace: t_trace.into(),
                    });
                }
                Ok(Msg::Error(StderrError {
                    id: id_bytes.into(),
                    level,
                    name: name_bytes.into(),
                    message: message_bytes.into(),
                    have_pos,
                    traces,
                }))
            }

            // STDERR_START_ACTIVITY
            0x53545254 => {
                let act = self.read_u64().await?;
                let lvl = self.read_u64().await?;
                let id = self.read_u64().await?;
                let s_bytes = self.read_bytes().await?;
                let fields = self.read_logger_fields().await?;
                let parent = self.read_u64().await?;
                Ok(Msg::StartActivity(StderrStartActivity {
                    act,
                    lvl,
                    id,
                    s: s_bytes.into(),
                    fields,
                    parent,
                }))
            }

            // STDERR_STOP_ACTIVITY
            0x53544f50 => {
                let id = self.read_u64().await?;
                Ok(Msg::StopActivity(id))
            }

            // STDERR_RESULT
            0x52534c54 => {
                let act = self.read_u64().await?;
                let id = self.read_u64().await?;
                let fields = self.read_logger_fields().await?;
                Ok(Msg::Result(StderrResult { act, id, fields }))
            }

            other => Err(make_err!(
                Code::Internal,
                "async_nix_conn: unknown stderr tag {other:#x}"
            )),
        }
    }

    /// Read logger fields (used inside StartActivity and Result stderr msgs).
    async fn read_logger_fields(&mut self) -> Result<LoggerFields, Error> {
        let count = self.read_u64().await? as usize;
        let mut fields = Vec::with_capacity(count);
        for _ in 0..count {
            let field_tag = self.read_u64().await?;
            let field = match field_tag {
                0 => {
                    let v = self.read_u64().await?;
                    LoggerField::Int(v)
                }
                1 => {
                    let v = self.read_bytes().await?;
                    LoggerField::String(v.into())
                }
                other => {
                    return Err(make_err!(
                        Code::Internal,
                        "async_nix_conn: unknown LoggerField tag {other}"
                    ));
                }
            };
            fields.push(field);
        }
        Ok(LoggerFields { fields })
    }

    /// Drain the stderr stream until [`Msg::Last`], returning an error if
    /// the daemon reports one.
    pub async fn drain_stderr(&mut self) -> Result<(), Error> {
        loop {
            let msg = self.read_stderr_msg().await?;
            match msg {
                Msg::Last(()) => return Ok(()),
                Msg::Error(err) => {
                    return Err(make_err!(
                        Code::Internal,
                        "Nix daemon reported error: {}",
                        String::from_utf8_lossy(&err.message)
                    ));
                }
                _ => {
                    tracing::debug!(stderr_msg = ?msg, "Nix daemon stderr");
                }
            }
        }
    }

    /// Drain stderr, but only until [`Msg::Last`] — does **not** treat
    /// [`Msg::Error`] as fatal.  Returns the first error message, if any.
    ///
    /// This is used by `build_derivation` where we want to log all
    /// stderr but only fail based on the build result status, not on
    /// individual stderr error messages (which may be informational).
    pub async fn drain_stderr_collecting_errors(&mut self) -> Result<Option<StderrError>, Error> {
        let mut first_error: Option<StderrError> = None;
        loop {
            let msg = self.read_stderr_msg().await?;
            match msg {
                Msg::Last(()) => return Ok(first_error),
                Msg::Error(err) => {
                    if first_error.is_none() {
                        first_error = Some(err);
                    }
                }
                _ => {
                    tracing::debug!(stderr_msg = ?msg, "Nix daemon stderr during build");
                }
            }
        }
    }

    // ── Reading ided responses ────────────────────────────────────────

    /// Read a [`QueryPathInfoResponse`].
    ///
    /// Wire format: `u64` flag (0 = path not valid → `None`), then if
    /// non-zero a [`ValidPathInfo`].
    pub async fn read_query_path_info_response(&mut self) -> Result<QueryPathInfoResponse, Error> {
        let valid = self.read_bool().await?;
        if !valid {
            return Ok(QueryPathInfoResponse { path: None });
        }
        let info = self.read_valid_path_info().await?;
        Ok(QueryPathInfoResponse { path: Some(info) })
    }

    /// Read a [`ValidPathInfo`].
    async fn read_valid_path_info(&mut self) -> Result<ValidPathInfo, Error> {
        let deriver = self.read_store_path().await?;
        let hash = self.read_nar_hash().await?;
        let references = self.read_store_path_set().await?;
        let registration_time = self.read_u64().await?;
        let nar_size = self.read_u64().await?;
        let ultimate = self.read_bool().await?;
        let sigs = self.read_string_set().await?;
        let content_address = self.read_nix_string().await?;

        Ok(ValidPathInfo {
            deriver,
            hash,
            references,
            registration_time,
            nar_size,
            ultimate,
            sigs,
            content_address,
        })
    }

    /// Read a [`ValidPathInfoWithPath`].
    pub async fn read_valid_path_info_with_path(&mut self) -> Result<ValidPathInfoWithPath, Error> {
        let path = self.read_store_path().await?;
        let info = self.read_valid_path_info().await?;
        Ok(ValidPathInfoWithPath { path, info })
    }

    /// Read `Vec<(DerivedPath, BuildResult)>` — the response to
    /// `BuildPathsWithResults`.
    pub async fn read_build_paths_with_results_response(
        &mut self,
    ) -> Result<Vec<(DerivedPath, BuildResult)>, Error> {
        let count = self.read_u64().await? as usize;
        let mut results = Vec::with_capacity(count);
        for _ in 0..count {
            let derived_path = self.read_derived_path().await?;
            let build_result = self.read_build_result().await?;
            results.push((derived_path, build_result));
        }
        Ok(results)
    }

    /// Read a [`DerivedPath`] (wire-identical to a `NixString`).
    #[inline]
    async fn read_derived_path(&mut self) -> Result<DerivedPath, Error> {
        Ok(DerivedPath(self.read_nix_string().await?))
    }

    /// Read a [`BuildResult`].
    async fn read_build_result(&mut self) -> Result<BuildResult, Error> {
        let status = self.read_build_status().await?;
        let error_msg = self.read_nix_string().await?;
        let times_built = self.read_u64().await?;
        let is_non_deterministic = self.read_bool().await?;
        let start_time = self.read_u64().await?;
        let stop_time = self.read_u64().await?;
        let built_outputs = self.read_drv_outputs().await?;

        Ok(BuildResult {
            status,
            error_msg,
            times_built,
            is_non_deterministic,
            start_time,
            stop_time,
            built_outputs,
        })
    }

    /// Read a [`BuildStatus`] (tagged u64).
    async fn read_build_status(&mut self) -> Result<BuildStatus, Error> {
        let tag = self.read_u64().await?;
        match tag {
            0 => Ok(BuildStatus::Built),
            1 => Ok(BuildStatus::Substituted),
            2 => Ok(BuildStatus::AlreadyValid),
            3 => Ok(BuildStatus::PermanentFailure),
            4 => Ok(BuildStatus::InputRejected),
            5 => Ok(BuildStatus::OutputRejected),
            6 => Ok(BuildStatus::TransientFailure),
            7 => Ok(BuildStatus::CachedFailure),
            8 => Ok(BuildStatus::TimedOut),
            9 => Ok(BuildStatus::MiscFailure),
            10 => Ok(BuildStatus::DependencyFailed),
            11 => Ok(BuildStatus::LogLimitExceeded),
            12 => Ok(BuildStatus::NotDeterministic),
            13 => Ok(BuildStatus::ResolvesToAlreadyValid),
            14 => Ok(BuildStatus::NoSubstituters),
            other => Err(make_err!(
                Code::Internal,
                "async_nix_conn: unknown BuildStatus tag {other}"
            )),
        }
    }

    /// Read [`DrvOutputs`] — `Vec<(NixString, Realisation)>`.
    async fn read_drv_outputs(&mut self) -> Result<DrvOutputs, Error> {
        let count = self.read_u64().await? as usize;
        let mut outputs = Vec::with_capacity(count);
        for _ in 0..count {
            let key = self.read_nix_string().await?;
            let realisation = self.read_nix_string().await?;
            outputs.push((key, nix_remote::Realisation(realisation)));
        }
        Ok(DrvOutputs(outputs))
    }
}
