// Copyright 2024 The NativeLink Authors. All rights reserved.
//
// Licensed under the Functional Source License, Version 1.1, Apache 2.0 Future License (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    See LICENSE file for details
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use core::net::SocketAddr;

use axum::http::Uri;
use futures::future::{BoxFuture, try_join_all};
use hyper::StatusCode;
use hyper_util::server::conn::auto;
use mimalloc::MiMalloc;
use nativelink::{run_server, tcp_accept_loop};
use nativelink_config::cas_server::{
    AcStoreConfig, ByteStreamConfig, CapabilitiesConfig, CapabilitiesRemoteExecutionConfig,
    CasStoreConfig, ExecutionConfig, WithInstanceName,
};
use nativelink_error::{Error, ResultExt};
use nativelink_service::ac_server::AcServer;
use nativelink_service::bytestream_server::ByteStreamServer;
use nativelink_service::capabilities_server::CapabilitiesServer;
use nativelink_service::cas_server::CasServer;
use nativelink_service::execution_server::ExecutionServer;
use nativelink_topology::topology;
use nativelink_util::digest_hasher::DigestHasherFunc;
use nativelink_util::shutdown_guard::ShutdownGuard;
use nativelink_util::store_trait::DEFAULT_DIGEST_SIZE_HEALTH_CHECK_CFG;
use nativelink_util::task::TaskExecutor;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, oneshot};
use tonic::service::Routes;
use tracing::{info, warn};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const VOID: &str = "void";
const NIX_STORE: &str = "nix_store";
const NIX_SCHEDULER: &str = "nix_scheduler";
const INSTANCE: &str = "main";

/// Wrap a config value in a single-element `WithInstanceName` vec using
/// the default instance name.
fn with_instance<T>(config: T) -> Vec<WithInstanceName<T>> {
    vec![WithInstanceName {
        instance_name: INSTANCE.to_string(),
        config,
    }]
}

async fn inner_main(
    shutdown_tx: broadcast::Sender<ShutdownGuard>,
    scheduler_shutdown_tx: oneshot::Sender<()>,
) -> Result<(), Error> {
    // ── Stores & scheduler ──────────────────────────────────────────────
    // The `topology!` DSL expands to direct `NoopStore::new` / `NixStore::new`
    // / `NixScheduler::new` calls, so only these backends are referenced and
    // the linker can drop every unused store/scheduler under LTO.
    let (store_manager, action_schedulers, mut worker_schedulers) = topology! {
        stores {
            void = Noop,
            nix_store = Nix { socket_path: None },
        }
        schedulers {
            nix_scheduler = NixProxy { ac_store: void, cas_store: nix_store },
        }
    };

    let worker_scheduler = worker_schedulers
        .remove(NIX_SCHEDULER)
        .expect("nix_scheduler worker scheduler must exist");

    // ── Services ───────────────────────────────────────────────────────
    let cas_cfg = with_instance(CasStoreConfig {
        cas_store: NIX_STORE.to_string(),
    });
    let ac_cfg = with_instance(AcStoreConfig {
        ac_store: VOID.to_string(),
        read_only: false,
    });
    let exec_cfg = with_instance(ExecutionConfig {
        cas_store: NIX_STORE.to_string(),
        scheduler: NIX_SCHEDULER.to_string(),
    });
    let caps_cfg = with_instance(CapabilitiesConfig {
        remote_execution: Some(CapabilitiesRemoteExecutionConfig {
            scheduler: NIX_SCHEDULER.to_string(),
        }),
    });
    let bs_cfg = with_instance(ByteStreamConfig {
        cas_store: NIX_STORE.to_string(),
        max_bytes_per_stream: 0,
        persist_stream_on_disconnect_timeout: 0,
    });

    let cas_server = CasServer::new(&cas_cfg, &store_manager)
        .err_tip(|| "Could not create CAS service")?
        .into_service();
    let ac_server = AcServer::new(&ac_cfg, &store_manager)
        .err_tip(|| "Could not create AC service")?
        .into_service();
    let exec_server = ExecutionServer::new(&exec_cfg, &action_schedulers, &store_manager)
        .err_tip(|| "Could not create Execution service")?
        .into_service();
    let caps_server = CapabilitiesServer::new(&caps_cfg, &action_schedulers)
        .await
        .err_tip(|| "Could not create Capabilities service")?
        .into_service();
    let bs_server = ByteStreamServer::new(&bs_cfg, &store_manager)
        .err_tip(|| "Could not create ByteStream service")?
        .into_service();

    let tonic_services = Routes::builder()
        .routes()
        .add_service(cas_server)
        .add_service(ac_server)
        .add_service(exec_server)
        .add_service(caps_server)
        .add_service(bs_server);

    let svc = tonic_services
        .into_axum_router()
        .layer(nativelink_util::telemetry::OtlpLayer::new(false))
        .fallback(|uri: Uri| async move {
            warn!("No route for {uri}");
            (StatusCode::NOT_FOUND, format!("No route for {uri}"))
        });

    // ── TCP listener ───────────────────────────────────────────────────
    let socket_addr: SocketAddr = "0.0.0.0:50051"
        .parse()
        .expect("Invalid hardcoded socket address");
    let tcp_listener = TcpListener::bind(&socket_addr).await?;
    let http = auto::Builder::new(TaskExecutor::default());

    info!("nixception ready, listening on {socket_addr}");

    let mut root_futures: Vec<BoxFuture<Result<(), Error>>> = Vec::new();

    root_futures.push(tcp_accept_loop(tcp_listener, http, svc, None, socket_addr));

    // Shutdown handler – tear down the scheduler (which logs timing
    // stats and writes the summary file) then signal completion.
    let mut shutdown_rx = shutdown_tx.subscribe();
    root_futures.push(Box::pin(async move {
        if let Ok(shutdown_guard) = shutdown_rx.recv().await {
            worker_scheduler.shutdown(shutdown_guard).await;
            let _ = scheduler_shutdown_tx.send(());
        }
        Ok(())
    }));

    if let Err(e) = try_join_all(root_futures).await {
        panic!("{e:?}");
    }

    Ok(())
}

fn main() -> Result<(), Box<dyn core::error::Error>> {
    run_server(
        "nixception",
        512,
        DigestHasherFunc::Sha256,
        DEFAULT_DIGEST_SIZE_HEALTH_CHECK_CFG,
        |shutdown_tx, scheduler_shutdown_tx| inner_main(shutdown_tx, scheduler_shutdown_tx),
    )
}
