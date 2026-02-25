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

use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use futures::Future;
use nativelink_config::schedulers::NixProxySpec;
use nativelink_error::{Code, Error, ResultExt, make_err};
use nativelink_metric::{MetricsComponent, RootMetricsComponent};
use nativelink_util::action_messages::{ActionInfo, OperationId, WorkerId};
use nativelink_util::instant_wrapper::InstantWrapper;
use nativelink_util::known_platform_property_provider::KnownPlatformPropertyProvider;
use nativelink_util::operation_state_manager::{
    ActionStateResult, ActionStateResultStream, ClientStateManager, OperationFilter,
    UpdateOperationType, WorkerStateManager,
};
use nativelink_util::shutdown_guard::ShutdownGuard;
use nativelink_util::store_trait::Store;

use crate::awaited_action_db::AwaitedActionDb;
use crate::nix_worker::NixWorker;
use crate::platform_property_manager::PlatformPropertyManager;
use crate::simple_scheduler_state_manager::SimpleSchedulerStateManager;
use crate::worker::{Worker, WorkerTimestamp};
use crate::worker_scheduler::WorkerScheduler;

/// Default timeout for no-event actions in seconds.
const DEFAULT_NO_EVENT_ACTION_TIMEOUT_S: u64 = 60;

/// Default client action timeout in seconds.
const DEFAULT_CLIENT_ACTION_TIMEOUT_S: u64 = 120;

// ---------------------------------------------------------------------------
// NixScheduler
// ---------------------------------------------------------------------------

/// A simplified Nix scheduler that executes actions by building Nix
/// derivations.
///
/// This struct implements both [`ClientStateManager`] (for accepting actions
/// from clients) and [`WorkerScheduler`] (for receiving execution-status
/// updates).  All action state is managed through a
/// [`SimpleSchedulerStateManager`] which owns the [`AwaitedActionDb`].
///
/// The state manager is the single source of truth for action state.  The
/// scheduler uses [`SimpleSchedulerStateManager::resolve_internal_operation_id`]
/// to obtain the DB-internal operation id after adding an action (the
/// client-facing [`ActionStateResult`] deliberately hides the internal id).
#[derive(MetricsComponent)]
pub struct NixScheduler<
    A: AwaitedActionDb,
    I: InstantWrapper,
    NowFn: Fn() -> I + Clone + Send + Unpin + Sync + 'static,
> {
    /// Platform property manager.
    #[metric(group = "platform_properties")]
    platform_property_manager: Arc<PlatformPropertyManager>,

    /// The state manager — single source of truth for action state.
    /// Implements [`ClientStateManager`] and [`WorkerStateManager`].
    #[metric(group = "state_manager")]
    state_manager: Arc<SimpleSchedulerStateManager<A, I, NowFn>>,

    /// Store manager for actions.
    #[allow(dead_code)]
    ac_store: Store,

    /// Store manager for data (CAS).
    cas_store: Store,

    /// Weak self-reference so we can pass `Arc<dyn WorkerScheduler>` to
    /// spawned [`NixWorker`] instances from `&self`.
    self_ref: OnceLock<Weak<dyn WorkerScheduler>>,
}

impl<
    A: AwaitedActionDb,
    I: InstantWrapper,
    NowFn: Fn() -> I + Clone + Send + Unpin + Sync + 'static,
> std::fmt::Debug for NixScheduler<A, I, NowFn>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NixScheduler")
            .field("platform_property_manager", &self.platform_property_manager)
            .finish_non_exhaustive()
    }
}

impl<
    A: AwaitedActionDb,
    I: InstantWrapper,
    NowFn: Fn() -> I + Clone + Send + Unpin + Sync + 'static,
> NixScheduler<A, I, NowFn>
{
    pub fn new(
        spec: &NixProxySpec,
        awaited_action_db: A,
        _task_change_notify: Arc<tokio::sync::Notify>,
        now_fn: NowFn,
        ac_store: Store,
        cas_store: Store,
    ) -> (Arc<Self>, Arc<dyn WorkerScheduler>) {
        Self::new_with_callback(
            spec,
            awaited_action_db,
            || async move {},
            _task_change_notify,
            now_fn,
            ac_store,
            cas_store,
        )
    }

    pub fn new_with_callback<
        Fut: Future<Output = ()> + Send,
        F: Fn() -> Fut + Send + Sync + 'static,
    >(
        _spec: &NixProxySpec,
        awaited_action_db: A,
        _on_matching_engine_run: F,
        _task_change_notify: Arc<tokio::sync::Notify>,
        now_fn: NowFn,
        ac_store: Store,
        cas_store: Store,
    ) -> (Arc<Self>, Arc<dyn WorkerScheduler>) {
        let platform_property_manager = Arc::new(PlatformPropertyManager::new(Default::default()));

        let state_manager = SimpleSchedulerStateManager::new(
            0, // max_job_retries — nix actions are not retried
            Duration::from_secs(DEFAULT_NO_EVENT_ACTION_TIMEOUT_S),
            Duration::from_secs(DEFAULT_CLIENT_ACTION_TIMEOUT_S),
            awaited_action_db,
            now_fn,
            None, // no worker registry for nix workers
        );

        let scheduler = Arc::new(Self {
            platform_property_manager,
            state_manager,
            ac_store,
            cas_store,
            self_ref: OnceLock::new(),
        });

        let worker_scheduler: Arc<dyn WorkerScheduler> = scheduler.clone();
        // Store a weak self-reference so create_running_action can obtain
        // an Arc<dyn WorkerScheduler> from &self.
        scheduler
            .self_ref
            .set(Arc::downgrade(&worker_scheduler))
            .ok();
        (scheduler, worker_scheduler)
    }

    /// Add an action via the state manager, spawn a [`NixWorker`] to execute
    /// it, and return the action state result the client can watch.
    async fn create_running_action(
        &self,
        client_operation_id: OperationId,
        action_info: Arc<ActionInfo>,
    ) -> Result<Box<dyn ActionStateResult>, Error> {
        // Register the action through the state manager (starts in Queued
        // state).  The returned `ActionStateResult` is already properly
        // wrapped with timeout handling by `SimpleSchedulerStateManager`.
        let action_state_result = self
            .state_manager
            .add_action(client_operation_id.clone(), action_info.clone())
            .await
            .err_tip(|| "In NixScheduler::create_running_action")?;

        // Retrieve the internal operation_id assigned by the db.
        //
        // The client-facing `ActionStateResult` deliberately masks the
        // internal operation id with the client operation id, so we ask
        // the state manager to resolve it for us.
        let operation_id = self
            .state_manager
            .resolve_internal_operation_id(&client_operation_id)
            .await
            .err_tip(|| "In NixScheduler::create_running_action resolving internal operation id")?;

        // Create a synthetic worker id for this nix worker instance.
        let worker_id = WorkerId(format!("nix-worker-{operation_id}"));

        // Spawn the worker — it will drive the action through all stages
        // and report state transitions via the state manager.
        let worker_scheduler_arc =
            self.self_ref.get().and_then(Weak::upgrade).ok_or_else(|| {
                make_err!(
                    Code::Internal,
                    "NixScheduler self-reference not initialised"
                )
            })?;
        let worker = NixWorker::new(
            worker_scheduler_arc,
            worker_id,
            operation_id,
            self.cas_store.clone(),
            action_info,
        );
        tokio::spawn(worker.run());

        Ok(action_state_result)
    }
}

// ---------------------------------------------------------------------------
// ClientStateManager
// ---------------------------------------------------------------------------

#[async_trait]
impl<
    A: AwaitedActionDb,
    I: InstantWrapper,
    NowFn: Fn() -> I + Clone + Send + Unpin + Sync + 'static,
> ClientStateManager for NixScheduler<A, I, NowFn>
{
    async fn add_action(
        &self,
        client_operation_id: OperationId,
        action_info: Arc<ActionInfo>,
    ) -> Result<Box<dyn ActionStateResult>, Error> {
        self.create_running_action(client_operation_id, action_info)
            .await
    }

    async fn filter_operations<'a>(
        &'a self,
        filter: OperationFilter,
    ) -> Result<ActionStateResultStream<'a>, Error> {
        ClientStateManager::filter_operations(&*self.state_manager, filter)
            .await
            .err_tip(|| "In NixScheduler::filter_operations")
    }

    fn as_known_platform_property_provider(&self) -> Option<&dyn KnownPlatformPropertyProvider> {
        Some(self)
    }
}

// ---------------------------------------------------------------------------
// KnownPlatformPropertyProvider
// ---------------------------------------------------------------------------

#[async_trait]
impl<
    A: AwaitedActionDb,
    I: InstantWrapper,
    NowFn: Fn() -> I + Clone + Send + Unpin + Sync + 'static,
> KnownPlatformPropertyProvider for NixScheduler<A, I, NowFn>
{
    async fn get_known_properties(&self, _instance_name: &str) -> Result<Vec<String>, Error> {
        Ok(self
            .platform_property_manager
            .get_known_properties()
            .keys()
            .cloned()
            .collect())
    }
}

// ---------------------------------------------------------------------------
// WorkerScheduler
// ---------------------------------------------------------------------------

#[async_trait]
impl<
    A: AwaitedActionDb,
    I: InstantWrapper,
    NowFn: Fn() -> I + Clone + Send + Unpin + Sync + 'static,
> WorkerScheduler for NixScheduler<A, I, NowFn>
{
    fn get_platform_property_manager(&self) -> &PlatformPropertyManager {
        &self.platform_property_manager
    }

    async fn add_worker(&self, _worker: Worker) -> Result<(), Error> {
        Err(make_err!(
            Code::Unimplemented,
            "Nix worker does not accept workers registration"
        ))
    }

    async fn update_action(
        &self,
        worker_id: &WorkerId,
        operation_id: &OperationId,
        update: UpdateOperationType,
    ) -> Result<(), Error> {
        self.state_manager
            .update_operation(operation_id, worker_id, update)
            .await
    }

    async fn worker_keep_alive_received(
        &self,
        _worker_id: &WorkerId,
        _timestamp: WorkerTimestamp,
    ) -> Result<(), Error> {
        Ok(())
    }

    async fn remove_worker(&self, _worker_id: &WorkerId) -> Result<(), Error> {
        Ok(())
    }

    async fn remove_timedout_workers(&self, _now_timestamp: WorkerTimestamp) -> Result<(), Error> {
        Ok(())
    }

    async fn set_drain_worker(
        &self,
        _worker_id: &WorkerId,
        _is_draining: bool,
    ) -> Result<(), Error> {
        Ok(())
    }

    async fn shutdown(&self, _shutdown_guard: ShutdownGuard) {}
}

// ---------------------------------------------------------------------------
// RootMetricsComponent
// ---------------------------------------------------------------------------

impl<
    A: AwaitedActionDb,
    I: InstantWrapper,
    NowFn: Fn() -> I + Clone + Send + Unpin + Sync + 'static,
> RootMetricsComponent for NixScheduler<A, I, NowFn>
{
}
