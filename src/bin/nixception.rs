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
use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

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
use nativelink_config::schedulers::NixProxySpec;
use nativelink_config::stores::NixSpec;
use nativelink_error::{Error, ResultExt};
use nativelink_scheduler::default_scheduler_factory::memory_awaited_action_db_factory;
use nativelink_scheduler::nix_scheduler::NixScheduler;
use nativelink_scheduler::runner_info::RunnerInfo;
use nativelink_service::ac_server::AcServer;
use nativelink_service::bytestream_server::ByteStreamServer;
use nativelink_service::capabilities_server::CapabilitiesServer;
use nativelink_service::cas_server::CasServer;
use nativelink_service::execution_server::ExecutionServer;
use nativelink_store::nix_store::NixStore;
use nativelink_store::noop_store::NoopStore;
use nativelink_store::store_manager::StoreManager;
use nativelink_util::digest_hasher::DigestHasherFunc;
use nativelink_util::shutdown_guard::ShutdownGuard;
use nativelink_util::store_trait::{DEFAULT_DIGEST_SIZE_HEALTH_CHECK_CFG, Store};
use nativelink_util::task::TaskExecutor;
use tokio::net::TcpListener;
use tokio::sync::{Notify, broadcast, oneshot};
use tonic::service::Routes;
use tracing::{info, warn};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const VOID: &str = "VOID";
const NIX_STORE: &str = "NIX_STORE";
const NIX_SCHEDULER: &str = "NIX_SCHEDULER";
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
    // ── Stores ──────────────────────────────────────────────────────────
    let store_manager = Arc::new(StoreManager::new());

    store_manager.add_store(VOID, Store::new(NoopStore::new()));

    let nix_store = NixStore::new(&NixSpec { socket_path: None })
        .await
        .err_tip(|| "Failed to create NIX_STORE")?;
    let nix_connection = nix_store.connection();
    store_manager.add_store(NIX_STORE, Store::new(nix_store));

    // ── Runner info ────────────────────────────────────────────────────
    let runner_info = Arc::new(
        RunnerInfo::from_env().err_tip(|| "Failed to initialise runner info from environment")?,
    );

    // ── Scheduler ──────────────────────────────────────────────────────
    let nix_proxy_spec = NixProxySpec {
        ac_store: VOID.to_string(),
        cas_store: NIX_STORE.to_string(),
    };

    let task_change_notify = Arc::new(Notify::new());
    let awaited_action_db =
        memory_awaited_action_db_factory(0, &task_change_notify, SystemTime::now);

    let ac_store = store_manager
        .get_store(VOID)
        .err_tip(|| "'VOID' store not found")?;
    let cas_store = store_manager
        .get_store(NIX_STORE)
        .err_tip(|| "'NIX_STORE' store not found")?;

    let (action_scheduler, _worker_scheduler) = NixScheduler::new(
        &nix_proxy_spec,
        awaited_action_db,
        task_change_notify,
        SystemTime::now,
        ac_store,
        cas_store,
        nix_connection,
        runner_info,
    );

    let action_scheduler: Arc<dyn nativelink_util::operation_state_manager::ClientStateManager> =
        action_scheduler;
    let action_schedulers = HashMap::from([(NIX_SCHEDULER.to_string(), action_scheduler)]);

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

    // Shutdown handler – no worker schedulers to tear down but we still
    // need to satisfy the protocol so the SIGTERM path works.
    let mut shutdown_rx = shutdown_tx.subscribe();
    root_futures.push(Box::pin(async move {
        if shutdown_rx.recv().await.is_ok() {
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
