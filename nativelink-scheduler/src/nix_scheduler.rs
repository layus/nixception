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

use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use futures::{future, stream, Future, Stream, StreamExt};
use nativelink_config::schedulers::NixProxySpec;
use nativelink_error::{Error, ResultExt};
use nativelink_metric::{MetricsComponent, RootMetricsComponent};
use nativelink_util::action_messages::{
    ActionInfo, ActionResult, ActionStage, ActionState, OperationId, WorkerId,
};
use nativelink_util::instant_wrapper::InstantWrapper;
use nativelink_util::known_platform_property_provider::KnownPlatformPropertyProvider;
use nativelink_util::operation_state_manager::{
    ActionStateResult, ActionStateResultStream, ClientStateManager, OperationFilter,
    UpdateOperationType,
};
use nativelink_util::spawn;
use nativelink_util::task::JoinHandleDropGuard;
use tokio::sync::{mpsc, watch, Notify};
use tokio::time::Duration;
use tracing::{event, Level};

use crate::awaited_action_db::AwaitedActionDb;
use crate::platform_property_manager::PlatformPropertyManager;
use crate::worker::{Worker, WorkerTimestamp};
use crate::worker_scheduler::WorkerScheduler;

// Dummy struct to implement ActionStateResult
struct DummyActionStateResult {
    client_operation_id: OperationId,
    action_info: Arc<ActionInfo>,
    state_rx: watch::Receiver<Arc<ActionState>>,
}

#[async_trait]
impl ActionStateResult for DummyActionStateResult {
    async fn as_state(&self) -> Result<Arc<ActionState>, Error> {
        let mut state = self.state_rx.borrow().clone();
        Arc::make_mut(&mut state).client_operation_id = self.client_operation_id.clone();
        Ok(state)
    }

    async fn changed(&mut self) -> Result<Arc<ActionState>, Error> {
        // In a real implementation, we would wait for state to change
        // Here we just return the current state immediately
        let mut state = self.state_rx.borrow().clone();
        Arc::make_mut(&mut state).client_operation_id = self.client_operation_id.clone();
        Ok(state)
    }

    async fn as_action_info(&self) -> Result<Arc<ActionInfo>, Error> {
        Ok(self.action_info.clone())
    }
}

/// A simplified Nix scheduler that immediately returns dummy results
#[derive(MetricsComponent)]
pub struct NixScheduler {
    /// Platform property manager
    #[metric(group = "platform_properties")]
    platform_property_manager: Arc<PlatformPropertyManager>,

    /// Background task to make sure our scheduler is properly cleaned up
    _task_worker_matching_spawn: JoinHandleDropGuard<()>,
}

impl NixScheduler {
    pub fn new<A: AwaitedActionDb>(
        _spec: &NixProxySpec,
        _awaited_action_db: A,
        task_change_notify: Arc<Notify>,
    ) -> (Arc<Self>, Arc<dyn WorkerScheduler>) {
        Self::new_with_callback(
            _spec,
            _awaited_action_db,
            || async move {},
            task_change_notify,
            SystemTime::now,
        )
    }

    pub fn new_with_callback<
        Fut: Future<Output = ()> + Send,
        F: Fn() -> Fut + Send + Sync + 'static,
        A: AwaitedActionDb,
        I: InstantWrapper,
        NowFn: Fn() -> I + Clone + Send + Unpin + Sync + 'static,
    >(
        _spec: &NixProxySpec,
        _awaited_action_db: A,
        _on_matching_engine_run: F,
        task_change_notify: Arc<Notify>,
        _now_fn: NowFn,
    ) -> (Arc<Self>, Arc<dyn WorkerScheduler>) {
        let platform_property_manager = Arc::new(PlatformPropertyManager::new(Default::default()));

        // Create a dummy worker scheduler
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        let worker_scheduler = Arc::new(DummyWorkerScheduler {
            platform_property_manager: platform_property_manager.clone(),
        });

        let worker_scheduler_clone = worker_scheduler.clone();

        let action_scheduler = Arc::new_cyclic(move |_weak_self| -> Self {
            let task_worker_matching_spawn =
                spawn!("nix_scheduler_task_worker_matching", async move {
                    // Just wait for our task_change_notify to be dropped
                    loop {
                        match task_change_notify.notified().await {
                            () => {
                                // Do nothing, just loop
                                tokio::time::sleep(Duration::from_millis(10)).await;
                            }
                        }
                    }
                });

            event!(Level::INFO, "NixScheduler: Initialized");

            NixScheduler {
                platform_property_manager,
                _task_worker_matching_spawn: task_worker_matching_spawn,
            }
        });

        (action_scheduler, worker_scheduler_clone)
    }

    async fn create_dummy_result(
        &self,
        client_operation_id: OperationId,
        action_info: Arc<ActionInfo>,
    ) -> Box<dyn ActionStateResult> {
        // Create a completed action state
        let action_digest = action_info.digest();
        let completed_state = Arc::new(ActionState {
            client_operation_id: client_operation_id.clone(),
            stage: ActionStage::Completed(ActionResult::default()),
            action_digest,
        });

        // Create a watch channel with the completed state
        let (tx, rx) = watch::channel(completed_state);
        let _ = tx; // We won't use the sender

        event!(
            Level::INFO,
            ?client_operation_id,
            ?action_digest,
            "NixScheduler: Immediately completing action"
        );

        Box::new(DummyActionStateResult {
            client_operation_id,
            action_info,
            state_rx: rx,
        })
    }
}

#[async_trait]
impl ClientStateManager for NixScheduler {
    async fn add_action(
        &self,
        client_operation_id: OperationId,
        action_info: Arc<ActionInfo>,
    ) -> Result<Box<dyn ActionStateResult>, Error> {
        Ok(self
            .create_dummy_result(client_operation_id, action_info)
            .await)
    }

    async fn filter_operations<'a>(
        &'a self,
        filter: OperationFilter,
    ) -> Result<ActionStateResultStream<'a>, Error> {
        event!(
            Level::INFO,
            ?filter,
            "NixScheduler: Filter operations called"
        );
        // Return an empty stream - no operations to find
        Ok(Box::pin(stream::empty()))
    }

    fn as_known_platform_property_provider(&self) -> Option<&dyn KnownPlatformPropertyProvider> {
        Some(self)
    }
}

#[async_trait]
impl KnownPlatformPropertyProvider for NixScheduler {
    async fn get_known_properties(&self, _instance_name: &str) -> Result<Vec<String>, Error> {
        Ok(self
            .platform_property_manager
            .get_known_properties()
            .keys()
            .cloned()
            .collect())
    }
}

// Simple implementation of the WorkerScheduler trait that does nothing
#[derive(MetricsComponent)]
struct DummyWorkerScheduler {
    #[metric(group = "platform_property_manager")]
    platform_property_manager: Arc<PlatformPropertyManager>,
}

#[async_trait]
impl WorkerScheduler for DummyWorkerScheduler {
    fn get_platform_property_manager(&self) -> &PlatformPropertyManager {
        &self.platform_property_manager
    }

    async fn add_worker(&self, worker: Worker) -> Result<(), Error> {
        event!(Level::INFO, worker_id = ?worker.id, "DummyWorkerScheduler: Adding worker (no-op)");
        // Send initial connection response to worker
        worker.tx.send(nativelink_proto::com::github::trace_machina::nativelink::remote_execution::UpdateForWorker {
            update: Some(nativelink_proto::com::github::trace_machina::nativelink::remote_execution::update_for_worker::Update::ConnectionResult(
                nativelink_proto::com::github::trace_machina::nativelink::remote_execution::ConnectionResult {
                    worker_id: worker.id.to_string(),
                }
            )),
        }).map_err(|e| nativelink_error::make_err!(nativelink_error::Code::Internal, "Failed to send connection result: {}", e))?;
        Ok(())
    }

    async fn update_action(
        &self,
        worker_id: &WorkerId,
        operation_id: &OperationId,
        update: UpdateOperationType,
    ) -> Result<(), Error> {
        event!(
            Level::INFO,
            ?worker_id,
            ?operation_id,
            ?update,
            "DummyWorkerScheduler: Update action (no-op)"
        );
        Ok(())
    }

    async fn worker_keep_alive_received(
        &self,
        worker_id: &WorkerId,
        timestamp: WorkerTimestamp,
    ) -> Result<(), Error> {
        event!(
            Level::DEBUG,
            ?worker_id,
            ?timestamp,
            "DummyWorkerScheduler: Worker keep-alive received (no-op)"
        );
        Ok(())
    }

    async fn remove_worker(&self, worker_id: &WorkerId) -> Result<(), Error> {
        event!(
            Level::INFO,
            ?worker_id,
            "DummyWorkerScheduler: Removing worker (no-op)"
        );
        Ok(())
    }

    async fn remove_timedout_workers(&self, now_timestamp: WorkerTimestamp) -> Result<(), Error> {
        event!(
            Level::DEBUG,
            ?now_timestamp,
            "DummyWorkerScheduler: Removing timed-out workers (no-op)"
        );
        Ok(())
    }

    async fn set_drain_worker(&self, worker_id: &WorkerId, is_draining: bool) -> Result<(), Error> {
        event!(
            Level::INFO,
            ?worker_id,
            ?is_draining,
            "DummyWorkerScheduler: Setting worker drain status (no-op)"
        );
        Ok(())
    }
}

impl RootMetricsComponent for NixScheduler {}
impl RootMetricsComponent for DummyWorkerScheduler {}
