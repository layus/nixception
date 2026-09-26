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

//! Server bootstrap helpers shared by the `nixception` binary.
//!
//! nixception constructs its store / scheduler / service topology directly
//! via the [`nativelink_topology::topology!`] macro (see
//! `src/bin/nixception.rs`), so the generic `CasConfig`-driven `store_factory`
//! / `scheduler_factory` dispatch that upstream `nativelink` uses is *not*
//! present here — dropping it lets the linker discard every storage backend
//! the binary never names (S3, GCS, Redis, MongoDB, …).
//!
//! What remains is the process-lifecycle scaffolding both would share:
//! [`run_server`] (tokio runtime, tracing, signal handlers, shutdown
//! plumbing) and [`tcp_accept_loop`] (the per-connection HTTP/2 accept loop).

use core::future::Future;
use core::net::SocketAddr;

use futures::future::{BoxFuture, Either};
use hyper_util::rt::tokio::TokioIo;
use hyper_util::server::conn::auto;
use hyper_util::service::TowerToHyperService;
use nativelink_error::{Error, ResultExt};
use nativelink_util::common::fs::set_open_file_limit;
use nativelink_util::digest_hasher::{DigestHasherFunc, set_default_digest_hasher_func};
#[cfg(target_family = "unix")]
use nativelink_util::shutdown_guard::Priority;
use nativelink_util::shutdown_guard::ShutdownGuard;
use nativelink_util::store_trait::set_default_digest_size_health_check;
use nativelink_util::task::TaskExecutor;
use nativelink_util::telemetry::init_tracing;
use nativelink_util::background_spawn;
use tokio::net::TcpListener;
use tokio::select;
#[cfg(target_family = "unix")]
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{broadcast, oneshot};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, error_span, info, trace_span};

/// Broadcast Channel Capacity
/// Note: The actual capacity may be greater than the provided capacity.
const BROADCAST_CAPACITY: usize = 1;

/// Create a future that runs a TCP accept loop, spawning a connection
/// handler for each incoming client.
///
/// If `maybe_tls_acceptor` is `Some`, connections are upgraded to TLS
/// before being served.  Pass `None` for plain-text HTTP/2 (e.g. in
/// nixception).
pub fn tcp_accept_loop(
    tcp_listener: TcpListener,
    http: auto::Builder<TaskExecutor>,
    svc: axum::Router,
    maybe_tls_acceptor: Option<TlsAcceptor>,
    socket_addr: SocketAddr,
) -> BoxFuture<'static, Result<(), Error>> {
    Box::pin(async move {
        loop {
            select! {
                accept_result = tcp_listener.accept() => {
                    match accept_result {
                        Ok((tcp_stream, remote_addr)) => {
                            debug!(
                                target: "nativelink::services",
                                ?remote_addr,
                                ?socket_addr,
                                "Client connected"
                            );

                            let (http, svc, maybe_tls_acceptor) =
                                (http.clone(), svc.clone(), maybe_tls_acceptor.clone());

                            background_spawn!(
                                name: "http_connection",
                                fut: error_span!(
                                    "http_connection",
                                    remote_addr = %remote_addr,
                                    socket_addr = %socket_addr,
                                ).in_scope(|| async move {
                                    let serve_connection = if let Some(tls_acceptor) = maybe_tls_acceptor {
                                        match tls_acceptor.accept(tcp_stream).await {
                                            Ok(tls_stream) => Either::Left(http.serve_connection(
                                                TokioIo::new(tls_stream),
                                                TowerToHyperService::new(svc),
                                            )),
                                            Err(err) => {
                                                error!(?err, "Failed to accept tls stream");
                                                return;
                                            }
                                        }
                                    } else {
                                        Either::Right(http.serve_connection(
                                            TokioIo::new(tcp_stream),
                                            TowerToHyperService::new(svc),
                                        ))
                                    };

                                    if let Err(err) = serve_connection.await {
                                        error!(
                                            target: "nativelink::services",
                                            ?err,
                                            "Failed running service"
                                        );
                                    }
                                }),
                                target: "nativelink::services",
                                ?remote_addr,
                                ?socket_addr,
                            );
                        },
                        Err(err) => {
                            error!(?err, "Failed to accept tcp connection");
                        }
                    }
                },
            }
        }
    })
}

/// Common server bootstrap: tokio runtime, tracing, global settings,
/// signal handlers, and shutdown plumbing.
///
/// `inner` receives the shutdown broadcast sender and the scheduler
/// shutdown oneshot sender, and should return a future that runs the
/// server logic.  `nixception` delegates to this so signal handling and
/// process lifecycle stay consistent.
///
/// # Errors
///
/// Returns an error if the runtime cannot be built, tracing fails to
/// initialize, or the server encounters a fatal error.
pub fn run_server<F, Fut>(
    name: &str,
    max_open_files: usize,
    digest_hasher: DigestHasherFunc,
    digest_size_health_check: usize,
    inner: F,
) -> Result<(), Box<dyn core::error::Error>>
where
    F: FnOnce(broadcast::Sender<ShutdownGuard>, oneshot::Sender<()>) -> Fut,
    Fut: Future<Output = Result<(), Error>>,
{
    #[expect(clippy::disallowed_methods, reason = "starting main runtime")]
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    // The OTLP exporters need to run in a Tokio context.
    // Do this first so all the other logging works.
    #[expect(clippy::disallowed_methods, reason = "tracing init on main runtime")]
    runtime.block_on(async { tokio::spawn(async { init_tracing() }).await? })?;

    set_open_file_limit(max_open_files);
    set_default_digest_hasher_func(digest_hasher)?;
    set_default_digest_size_health_check(digest_size_health_check)?;

    // Initiates the shutdown process by broadcasting the shutdown signal
    // via the `oneshot::Sender` to all listeners.  Each listener will
    // perform its cleanup and then drop its `oneshot::Sender`, signaling
    // completion.  Once all senders are dropped the process can exit.
    let (shutdown_tx, _) = broadcast::channel::<ShutdownGuard>(BROADCAST_CAPACITY);
    #[cfg(target_family = "unix")]
    let shutdown_tx_clone = shutdown_tx.clone();
    #[cfg(target_family = "unix")]
    let mut shutdown_guard = ShutdownGuard::default();

    #[expect(clippy::disallowed_methods, reason = "signal handler on main runtime")]
    runtime.spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen to SIGINT");
        eprintln!("User terminated process via SIGINT");
        std::process::exit(130);
    });

    #[allow(unused_variables)]
    let (scheduler_shutdown_tx, scheduler_shutdown_rx) = oneshot::channel();

    let shutdown_msg = format!("Successfully shut down {name}.");

    #[cfg(target_family = "unix")]
    #[expect(clippy::disallowed_methods, reason = "signal handler on main runtime")]
    runtime.spawn(async move {
        signal(SignalKind::terminate())
            .expect("Failed to listen to SIGTERM")
            .recv()
            .await;
        info!("Process terminated via SIGTERM");
        drop(shutdown_tx_clone.send(shutdown_guard.clone()));
        let () = shutdown_guard.wait_for(Priority::P0).await;
        info!("{}", shutdown_msg);
        std::process::exit(143);
    });

    let err_msg = format!("{name} main() failed");

    #[expect(clippy::disallowed_methods, reason = "waiting on everything to finish")]
    runtime
        .block_on(async {
            trace_span!("main")
                .in_scope(|| inner(shutdown_tx, scheduler_shutdown_tx))
                .await
        })
        .err_tip(|| err_msg)?;
    Ok(())
}
