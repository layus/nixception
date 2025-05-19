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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use futures::{stream, Future};
use nativelink_config::schedulers::NixProxySpec;
use nativelink_error::Error;
use nativelink_metric::{MetricsComponent, RootMetricsComponent};
use nativelink_util::action_messages::{
    ActionInfo, ActionStage, ActionState, OperationId, WorkerId,
};
use nativelink_util::instant_wrapper::InstantWrapper;
use nativelink_util::known_platform_property_provider::KnownPlatformPropertyProvider;
use nativelink_util::operation_state_manager::{
    ActionStateResult, ActionStateResultStream, ClientStateManager, OperationFilter,
    OperationStageFlags, UpdateOperationType,
};
use nativelink_util::store_trait::Store;
use tokio::sync::{watch, Mutex as TokioMutex, Notify};
use tracing::{event, Level};

use crate::awaited_action_db::AwaitedActionDb;
use crate::nix_worker::NixExecutor;
use crate::platform_property_manager::PlatformPropertyManager;
use crate::worker::{Worker, WorkerTimestamp};
use crate::worker_scheduler::WorkerScheduler;

// Struct to implement ActionStateResult for Nix scheduler
struct NixActionStateResult {
    client_operation_id: OperationId,
    action_info: Arc<ActionInfo>,
    state_rx: watch::Receiver<Arc<ActionState>>,
}

#[async_trait]
impl ActionStateResult for NixActionStateResult {
    async fn as_state(&self) -> Result<Arc<ActionState>, Error> {
        let mut state = self.state_rx.borrow().clone();
        Arc::make_mut(&mut state).client_operation_id = self.client_operation_id.clone();
        Ok(state)
    }

    async fn changed(&mut self) -> Result<Arc<ActionState>, Error> {
        // Wait for the state to change
        if self.state_rx.changed().await.is_ok() {
            let mut state = self.state_rx.borrow_and_update().clone();
            Arc::make_mut(&mut state).client_operation_id = self.client_operation_id.clone();
            Ok(state)
        } else {
            // Channel closed
            let mut state = self.state_rx.borrow_and_update().clone();
            Arc::make_mut(&mut state).client_operation_id = self.client_operation_id.clone();
            Ok(state)
        }
    }

    async fn as_action_info(&self) -> Result<Arc<ActionInfo>, Error> {
        Ok(self.action_info.clone())
    }
}

/// A simplified Nix scheduler that simulates running actions until timeout
#[derive(MetricsComponent)]
pub struct NixScheduler {
    /// Platform property manager
    #[metric(group = "platform_properties")]
    platform_property_manager: Arc<PlatformPropertyManager>,

    /// All active actions (operation_id -> action state channel sender)
    active_actions:
        Arc<TokioMutex<HashMap<OperationId, (Arc<ActionInfo>, watch::Sender<Arc<ActionState>>)>>>,

    /// Store manager for accessing content
    #[allow(dead_code)]
    ac_store: Store,

    /// Nix executor for handling tasks
    #[allow(dead_code)]
    nix_executor: Arc<NixExecutor>,
}

impl NixScheduler {
    pub fn new<A: AwaitedActionDb>(
        _spec: &NixProxySpec,
        _awaited_action_db: A,
        task_change_notify: Arc<Notify>,
        ac_store: Store,
    ) -> (Arc<Self>, Arc<dyn WorkerScheduler>) {
        Self::new_with_callback(
            _spec,
            _awaited_action_db,
            || async move {},
            task_change_notify,
            SystemTime::now,
            ac_store,
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
        ac_store: Store,
    ) -> (Arc<Self>, Arc<dyn WorkerScheduler>) {
        let platform_property_manager = Arc::new(PlatformPropertyManager::new(Default::default()));

        // Create a Nix worker scheduler
        let worker_scheduler = Arc::new(NixWorkerScheduler {
            platform_property_manager: platform_property_manager.clone(),
        });

        let worker_scheduler_clone = worker_scheduler.clone();

        let action_scheduler = Arc::new_cyclic(move |_weak_self| -> Self {
            let active_actions = Arc::new(TokioMutex::new(HashMap::<
                OperationId,
                (Arc<ActionInfo>, watch::Sender<Arc<ActionState>>),
            >::new()));

            // Create the Nix executor which will monitor tasks
            let nix_executor = Arc::new(NixExecutor::new(
                active_actions.clone(),
                task_change_notify.clone(),
                ac_store.clone(),
            ));

            event!(Level::INFO, "NixScheduler: Initialized");

            NixScheduler {
                platform_property_manager,
                active_actions,
                ac_store,
                nix_executor,
            }
        });

        (action_scheduler, worker_scheduler_clone)
    }

    async fn create_running_action(
        &self,
        client_operation_id: OperationId,
        action_info: Arc<ActionInfo>,
    ) -> Box<dyn ActionStateResult> {
        // Create a running action state
        let action_digest = action_info.digest();
        let running_state = Arc::new(ActionState {
            client_operation_id: client_operation_id.clone(),
            stage: ActionStage::Executing,
            action_digest,
        });

        // Create a watch channel with the running state
        let (tx, rx) = watch::channel(running_state);

        // Store the action and sender in our active actions map
        {
            let mut actions = self.active_actions.lock().await;
            actions.insert(client_operation_id.clone(), (action_info.clone(), tx));
        }

        // Log detailed information about the received action
        // Note: In a production environment, we could use the ac_store (currently just storing the name: {})
        // to fetch and log the actual command details using the command_digest
        event!(
            Level::INFO,
            ?client_operation_id,
            ?action_digest,
            command_digest = ?action_info.command_digest,
            input_root_digest = ?action_info.input_root_digest,
            timeout_secs = ?action_info.timeout.as_secs(),
            platform_props = ?action_info.platform_properties,
            priority = ?action_info.priority,
            qualifier = ?action_info.unique_qualifier,
            "NixScheduler: Received action with detailed info (will timeout)"
        );

        Box::new(NixActionStateResult {
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
            .create_running_action(client_operation_id, action_info)
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

        // Get a snapshot of current actions
        let actions = self.active_actions.lock().await;

        // Apply filters
        let matches: Vec<_> = actions
            .iter()
            .filter_map(|(op_id, (action_info, state_tx))| {
                // Filter by operation_id if specified
                if let Some(filter_op_id) = &filter.operation_id {
                    if filter_op_id != op_id {
                        return None;
                    }
                }

                // Filter by stage if specified
                let state = state_tx.borrow();
                let current_stage_flag = match &state.stage {
                    ActionStage::Queued => OperationStageFlags::Queued,
                    ActionStage::Executing => OperationStageFlags::Executing,
                    ActionStage::Completed(_) => OperationStageFlags::Completed,
                    ActionStage::Unknown => OperationStageFlags::Any,
                    ActionStage::CacheCheck => OperationStageFlags::CacheCheck,
                    ActionStage::CompletedFromCache(_) => OperationStageFlags::Completed,
                };

                if !filter.stages.contains(current_stage_flag) {
                    return None;
                }

                // Create a new ActionStateResult for the matched operation
                let rx = state_tx.subscribe();
                Some(Box::new(NixActionStateResult {
                    client_operation_id: op_id.clone(),
                    action_info: action_info.clone(),
                    state_rx: rx,
                }) as Box<dyn ActionStateResult>)
            })
            .collect();

        // Return a stream that yields all matches
        Ok(Box::pin(stream::iter(matches)))
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

// Implementation of the WorkerScheduler trait for Nix scheduler
#[derive(MetricsComponent)]
struct NixWorkerScheduler {
    #[metric(group = "platform_property_manager")]
    platform_property_manager: Arc<PlatformPropertyManager>,
}

#[async_trait]
impl WorkerScheduler for NixWorkerScheduler {
    fn get_platform_property_manager(&self) -> &PlatformPropertyManager {
        &self.platform_property_manager
    }

    async fn add_worker(&self, worker: Worker) -> Result<(), Error> {
        event!(Level::INFO, worker_id = ?worker.id, "NixWorkerScheduler: Adding worker (no-op)");
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
            "NixWorkerScheduler: Update action (no-op)"
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
            "NixWorkerScheduler: Worker keep-alive received (no-op)"
        );
        Ok(())
    }

    async fn remove_worker(&self, worker_id: &WorkerId) -> Result<(), Error> {
        event!(
            Level::INFO,
            ?worker_id,
            "NixWorkerScheduler: Removing worker (no-op)"
        );
        Ok(())
    }

    async fn remove_timedout_workers(&self, now_timestamp: WorkerTimestamp) -> Result<(), Error> {
        event!(
            Level::DEBUG,
            ?now_timestamp,
            "NixWorkerScheduler: Removing timed-out workers (no-op)"
        );
        Ok(())
    }

    async fn set_drain_worker(&self, worker_id: &WorkerId, is_draining: bool) -> Result<(), Error> {
        event!(
            Level::INFO,
            ?worker_id,
            ?is_draining,
            "NixWorkerScheduler: Setting worker drain status (no-op)"
        );
        Ok(())
    }
}

impl RootMetricsComponent for NixScheduler {}
impl RootMetricsComponent for NixWorkerScheduler {}
