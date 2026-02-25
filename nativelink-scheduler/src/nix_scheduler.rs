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

use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use derivative::Derivative;
use futures::{Future, stream};
use nativelink_config::schedulers::NixProxySpec;
use nativelink_error::{Code, Error, make_err};
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
use nativelink_util::origin_event::OriginMetadata;
use nativelink_util::shutdown_guard::ShutdownGuard;
use nativelink_util::store_trait::Store;

use tokio::sync::{Mutex as TokioMutex, Notify, watch};
use tokio::time;

use crate::awaited_action_db::AwaitedActionDb;
use crate::nix_worker::{ActionUpdater, execute_action};
use crate::platform_property_manager::PlatformPropertyManager;
use crate::worker::{Worker, WorkerTimestamp};
use crate::worker_scheduler::WorkerScheduler;

/// Type alias for the shared active-actions map used throughout the scheduler.
pub(crate) type ActiveActionsMap =
    HashMap<OperationId, (Arc<ActionInfo>, watch::Sender<Arc<ActionState>>)>;

// Struct to implement ActionStateResult for Nix scheduler
struct NixActionStateResult {
    client_operation_id: OperationId,
    action_info: Arc<ActionInfo>,
    state_rx: watch::Receiver<Arc<ActionState>>,
}

#[async_trait]
impl ActionStateResult for NixActionStateResult {
    async fn as_state(&self) -> Result<(Arc<ActionState>, Option<OriginMetadata>), Error> {
        let mut state = self.state_rx.borrow().clone();
        Arc::make_mut(&mut state).client_operation_id = self.client_operation_id.clone();
        Ok((state, None))
    }

    async fn changed(&mut self) -> Result<(Arc<ActionState>, Option<OriginMetadata>), Error> {
        // Wait for the state to change
        if self.state_rx.changed().await.is_ok() {
            let mut state = self.state_rx.borrow_and_update().clone();
            Arc::make_mut(&mut state).client_operation_id = self.client_operation_id.clone();
            Ok((state, None))
        } else {
            // Channel closed
            Err(make_err!(
                Code::Internal,
                "NixActionStateResult: changed() failed, channel closed"
            ))
        }
    }

    async fn as_action_info(&self) -> Result<(Arc<ActionInfo>, Option<OriginMetadata>), Error> {
        Ok((self.action_info.clone(), None))
    }
}

/// A simplified Nix scheduler that simulates running actions until timeout.
///
/// This struct implements both [`ClientStateManager`] (for accepting actions from
/// clients) and [`WorkerScheduler`] (for receiving execution-status updates).
/// There is no separate worker-scheduler object — `NixScheduler` owns all the
/// state directly.
#[derive(MetricsComponent)]
pub struct NixScheduler<I: InstantWrapper, NowFn: Fn() -> I + Clone + Send + Unpin + Sync + 'static>
{
    /// Platform property manager
    #[metric(group = "platform_properties")]
    platform_property_manager: Arc<PlatformPropertyManager>,

    /// All active actions (operation_id -> action state channel sender)
    active_actions: Arc<TokioMutex<ActiveActionsMap>>,

    /// Priority queue of actions ordered by timeout time
    timeout_queue: Arc<TokioMutex<BinaryHeap<TimeoutEntry<I>>>>,

    /// Store manager for actions
    #[allow(dead_code)]
    ac_store: Store,

    /// Store manager for data
    cas_store: Store,

    /// Function to get the current time
    now_fn: NowFn,

    /// Notify when actions change
    task_change_notify: Arc<Notify>,
}

impl<I: InstantWrapper, NowFn: Fn() -> I + Clone + Send + Unpin + Sync + 'static> std::fmt::Debug
    for NixScheduler<I, NowFn>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NixScheduler")
            .field("platform_property_manager", &self.platform_property_manager)
            .finish_non_exhaustive()
    }
}

// Struct to represent an action in the timeout priority queue
#[derive(Clone, Debug, Derivative)]
#[derivative(PartialOrd, Ord, PartialEq, Eq)]
struct TimeoutEntry<I: InstantWrapper> {
    // The time when this action will timeout
    timeout_time: I,
    // The operation ID for this action
    #[derivative(PartialOrd = "ignore", PartialEq = "ignore", Ord = "ignore")]
    operation_id: OperationId,
}

impl<I: InstantWrapper, NowFn: Fn() -> I + Clone + Send + Unpin + Sync + 'static>
    NixScheduler<I, NowFn>
{
    pub fn new<A: AwaitedActionDb>(
        _spec: &NixProxySpec,
        _awaited_action_db: A,
        task_change_notify: Arc<Notify>,
        now_fn: NowFn,
        ac_store: Store,
        cas_store: Store,
    ) -> (Arc<Self>, Arc<dyn WorkerScheduler>) {
        Self::new_with_callback(
            _spec,
            _awaited_action_db,
            || async move {},
            task_change_notify,
            now_fn,
            ac_store,
            cas_store,
        )
    }

    pub fn new_with_callback<
        Fut: Future<Output = ()> + Send,
        F: Fn() -> Fut + Send + Sync + 'static,
        A: AwaitedActionDb,
    >(
        _spec: &NixProxySpec,
        _awaited_action_db: A,
        _on_matching_engine_run: F,
        task_change_notify: Arc<Notify>,
        now_fn: NowFn,
        ac_store: Store,
        cas_store: Store,
    ) -> (Arc<Self>, Arc<dyn WorkerScheduler>) {
        let platform_property_manager = Arc::new(PlatformPropertyManager::new(Default::default()));

        // Create shared state.
        let active_actions = Arc::new(TokioMutex::new(ActiveActionsMap::new()));
        let timeout_queue = Arc::new(TokioMutex::new(BinaryHeap::new()));

        let action_scheduler = Arc::new_cyclic(move |_weak_self| -> Self {
            let scheduler = NixScheduler {
                platform_property_manager,
                active_actions: active_actions.clone(),
                timeout_queue: timeout_queue.clone(),
                ac_store,
                cas_store,
                now_fn: now_fn.clone(),
                task_change_notify: task_change_notify.clone(),
            };

            // Start background task for monitoring action timeouts.
            tokio::spawn(Self::timeout_monitor_task(
                active_actions,
                timeout_queue,
                task_change_notify.clone(),
                now_fn.clone(),
            ));

            scheduler
        });

        // The same Arc serves as both ClientStateManager and WorkerScheduler.
        let worker_scheduler: Arc<dyn WorkerScheduler> = action_scheduler.clone();
        (action_scheduler, worker_scheduler)
    }

    // Background task to monitor timeouts for actions
    async fn timeout_monitor_task(
        active_actions: Arc<TokioMutex<ActiveActionsMap>>,
        timeout_queue: Arc<TokioMutex<BinaryHeap<TimeoutEntry<I>>>>,
        task_change_notify: Arc<Notify>,
        now_fn: NowFn,
    ) {
        loop {
            // Get the next timeout if any
            let now = now_fn();
            let next_timeout_duration = {
                let queue = timeout_queue.lock().await;
                queue.peek().map_or(Duration::MAX, |next_timeout| {
                    next_timeout.timeout_time.saturating_duration_since(&now)
                })
            };

            // Wait for either a timeout or a notification about new actions
            if next_timeout_duration <= Duration::ZERO {
                // Process timeouts immediately
            } else {
                tokio::select! {
                    // Sleep until next timeout
                    _ = time::sleep(next_timeout_duration), if next_timeout_duration < Duration::MAX => {},
                    // Or wait for notification about new actions
                    _ = task_change_notify.notified() => {},
                }
            }

            // Process timeouts
            let now = now_fn();
            let mut queue = timeout_queue.lock().await;
            let actions = active_actions.lock().await;

            // Process all actions that have timed out
            while let Some(entry) = queue.peek() {
                if entry.timeout_time > now {
                    break;
                }

                let entry = queue.pop().unwrap();
                let op_id = &entry.operation_id;
                if let Some((action_info, state_tx)) = actions.get(op_id) {
                    // Create timeout result (exit code 124 is standard for timeout)
                    let timeout_result = nativelink_util::action_messages::ActionResult {
                        exit_code: 124,
                        ..Default::default()
                    };

                    // Update the action state
                    let action_digest = action_info.digest();
                    let completed_state = Arc::new(ActionState {
                        client_operation_id: op_id.clone(),
                        stage: ActionStage::Completed(timeout_result),
                        action_digest,
                        last_transition_timestamp: SystemTime::now(),
                    });

                    // Send the update
                    state_tx.send(completed_state).unwrap();
                }
            }
        }
    }

    async fn create_running_action(
        &self,
        client_operation_id: OperationId,
        action_info: Arc<ActionInfo>,
    ) -> Box<dyn ActionStateResult> {
        // Get current time using now_fn
        let now = (self.now_fn)();

        // Create the action in `Queued` state — it will transition to
        // `Executing` once the derivation has been uploaded to the Nix store.
        let action_digest = action_info.digest();
        let queued_state = Arc::new(ActionState {
            client_operation_id: client_operation_id.clone(),
            stage: ActionStage::Queued,
            action_digest,
            last_transition_timestamp: SystemTime::now(),
        });

        // Create a watch channel with the running state
        let (tx, rx) = watch::channel(queued_state);

        self.active_actions.lock().await.insert(
            client_operation_id.clone(),
            (action_info.clone(), tx.clone()),
        );

        // Add the action to the timeout queue
        self.timeout_queue.lock().await.push(TimeoutEntry {
            timeout_time: now.add(Duration::from_secs(60).min(action_info.timeout)),
            operation_id: client_operation_id.clone(),
        });

        // Notify the timeout monitor task
        self.task_change_notify.notify_one();

        // Spawn the action execution as a fire-and-forget background task.
        // All state transitions are handled inside `execute_action` via
        // the `ActionUpdater`.
        let updater = ActionUpdater::new(self.active_actions.clone(), client_operation_id.clone());
        tokio::spawn(execute_action(
            self.cas_store.clone(),
            updater,
            action_info.clone(),
        ));

        Box::new(NixActionStateResult {
            client_operation_id,
            action_info,
            state_rx: rx,
        })
    }
}

#[async_trait]
impl<I: InstantWrapper, NowFn: Fn() -> I + Clone + Send + Unpin + Sync + 'static> ClientStateManager
    for NixScheduler<I, NowFn>
{
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
        // Get a snapshot of current actions
        let actions = self.active_actions.lock().await;

        // Apply filters
        let matches = actions
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
                let result: Box<dyn ActionStateResult> = Box::new(NixActionStateResult {
                    client_operation_id: op_id.clone(),
                    action_info: action_info.clone(),
                    state_rx: rx,
                });
                Some(result)
            })
            .collect::<Vec<_>>();

        // Return a stream that yields all matches
        Ok(Box::pin(stream::iter(matches.into_iter())))
    }

    fn as_known_platform_property_provider(&self) -> Option<&dyn KnownPlatformPropertyProvider> {
        Some(self)
    }
}

#[async_trait]
impl<I: InstantWrapper, NowFn: Fn() -> I + Clone + Send + Unpin + Sync + 'static>
    KnownPlatformPropertyProvider for NixScheduler<I, NowFn>
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

// WorkerScheduler is now implemented directly on NixScheduler — no separate
// NixWorkerScheduler struct is needed.
#[async_trait]
impl<I: InstantWrapper, NowFn: Fn() -> I + Clone + Send + Unpin + Sync + 'static> WorkerScheduler
    for NixScheduler<I, NowFn>
{
    fn get_platform_property_manager(&self) -> &PlatformPropertyManager {
        &self.platform_property_manager
    }

    async fn add_worker(&self, worker: Worker) -> Result<(), Error> {
        // Send initial connection response to worker
        worker.tx.send(nativelink_proto::com::github::trace_machina::nativelink::remote_execution::UpdateForWorker {
            update: Some(nativelink_proto::com::github::trace_machina::nativelink::remote_execution::update_for_worker::Update::ConnectionResult(
                nativelink_proto::com::github::trace_machina::nativelink::remote_execution::ConnectionResult {
                    worker_id: worker.id.to_string(),
                }
            )),
        }).map_err(|e| make_err!(Code::Internal, "Failed to send connection result: {}", e))?;
        Ok(())
    }

    async fn update_action(
        &self,
        _worker_id: &WorkerId,
        operation_id: &OperationId,
        update: UpdateOperationType,
    ) -> Result<(), Error> {
        ActionUpdater::new(self.active_actions.clone(), operation_id.clone())
            .try_update(update)
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

impl<I: InstantWrapper, NowFn: Fn() -> I + Clone + Send + Unpin + Sync + 'static>
    RootMetricsComponent for NixScheduler<I, NowFn>
{
}
