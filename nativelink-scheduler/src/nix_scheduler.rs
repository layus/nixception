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

use nix_compat::store_path::StorePath;
use std::collections::{BTreeMap, BinaryHeap, HashMap};
use std::iter::once;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use bstr::BString;
use derivative::Derivative;
use futures::{Future, stream};
use nativelink_config::schedulers::NixProxySpec;
use nativelink_error::{Code, Error, ResultExt, make_err};
use nativelink_metric::{MetricsComponent, RootMetricsComponent};
use nativelink_store::ac_utils::get_and_decode_digest;
use nativelink_store::nix_store::{NixStore, key_to_store_path};
use nativelink_util::action_messages::{
    ActionInfo, ActionStage, ActionState, OperationId, WorkerId,
};
use nativelink_util::common::DigestInfo;
use nativelink_util::instant_wrapper::InstantWrapper;
use nativelink_util::known_platform_property_provider::KnownPlatformPropertyProvider;
use nativelink_util::operation_state_manager::{
    ActionStateResult, ActionStateResultStream, ClientStateManager, OperationFilter,
    OperationStageFlags, UpdateOperationType,
};
use nativelink_util::origin_event::OriginMetadata;
use nativelink_util::shutdown_guard::ShutdownGuard;
use nativelink_util::store_trait::{Store, StoreKey, StoreLike};

use nix_compat::derivation::{Derivation, Output};
use nix_compat::nixhash::CAHash;

use tokio::sync::{Mutex as TokioMutex, Notify, watch};
use tokio::time;
use tracing::{Level, event};

use nativelink_proto::build::bazel::remote::execution::v2::{
    Command as ProtoCommand, Directory as ProtoDirectory,
};

use crate::awaited_action_db::AwaitedActionDb;
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
            Err(nativelink_error::make_err!(
                Code::Internal,
                "NixActionStateResult: changed() failed, channel closed"
            ))
        }
    }

    async fn as_action_info(&self) -> Result<(Arc<ActionInfo>, Option<OriginMetadata>), Error> {
        Ok((self.action_info.clone(), None))
    }
}

/// A simplified Nix scheduler that simulates running actions until timeout
#[derive(MetricsComponent)]
pub struct NixScheduler<I: InstantWrapper, NowFn: Fn() -> I + Clone + Send + Unpin + Sync + 'static>
{
    /// Platform property manager
    #[metric(group = "platform_properties")]
    platform_property_manager: Arc<PlatformPropertyManager>,

    /// All active actions (operation_id -> action state channel sender)
    active_actions:
        Arc<TokioMutex<HashMap<OperationId, (Arc<ActionInfo>, watch::Sender<Arc<ActionState>>)>>>,

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

    /// Worker scheduler for dispatching action updates
    #[metric(group = "worker_scheduler")]
    worker_scheduler: Arc<NixWorkerScheduler>,
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

        // Create shared state before both schedulers so they can reference it.
        let active_actions = Arc::new(TokioMutex::new(HashMap::<
            OperationId,
            (Arc<ActionInfo>, watch::Sender<Arc<ActionState>>),
        >::new()));

        let timeout_queue = Arc::new(TokioMutex::new(BinaryHeap::new()));

        // Create a Nix worker scheduler with shared active_actions
        let worker_scheduler = Arc::new(NixWorkerScheduler {
            platform_property_manager: platform_property_manager.clone(),
            active_actions: active_actions.clone(),
        });

        let worker_scheduler_for_scheduler = worker_scheduler.clone();
        let worker_scheduler_ret: Arc<dyn WorkerScheduler> = worker_scheduler;

        let action_scheduler = Arc::new_cyclic(move |_weak_self| -> Self {
            let scheduler = NixScheduler {
                platform_property_manager,
                active_actions: active_actions.clone(),
                timeout_queue: timeout_queue.clone(),
                ac_store,
                cas_store,
                now_fn: now_fn.clone(),
                task_change_notify: task_change_notify.clone(),
                worker_scheduler: worker_scheduler_for_scheduler.clone(),
            };

            // Start background task for monitoring action timeouts
            // event!(Level::INFO, "Spawning poll task");
            tokio::spawn(Self::timeout_monitor_task(
                active_actions,
                timeout_queue,
                task_change_notify.clone(),
                now_fn.clone(),
            ));

            scheduler
        });

        (action_scheduler, worker_scheduler_ret)
    }

    // Background task to monitor timeouts for actions
    async fn timeout_monitor_task(
        active_actions: Arc<
            TokioMutex<HashMap<OperationId, (Arc<ActionInfo>, watch::Sender<Arc<ActionState>>)>>,
        >,
        timeout_queue: Arc<TokioMutex<BinaryHeap<TimeoutEntry<I>>>>,
        task_change_notify: Arc<Notify>,
        now_fn: NowFn,
    ) {
        // event!(
        //     Level::INFO,
        //     "Starting NixScheduler timeout monitor task with priority queue"
        // );

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
                // event!(Level::INFO, ?next_timeout_duration, "NixScheduler: Polling");
                tokio::select! {
                    // Sleep until next timeout
                    _ = time::sleep(next_timeout_duration), if next_timeout_duration < Duration::MAX => {},
                    // Or wait for notification about new actions
                    _ = task_change_notify.notified() => {},
                }
                // event!(Level::INFO, "NixScheduler: Poll done");
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

                    // event!(
                    //     Level::INFO,
                    //     ?op_id,
                    //     ?action_digest,
                    //     "NixScheduler: Action timed out"
                    // );
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

        // Create a running action state
        let action_digest = action_info.digest();
        let running_state = Arc::new(ActionState {
            client_operation_id: client_operation_id.clone(),
            stage: ActionStage::Executing,
            action_digest,
            last_transition_timestamp: SystemTime::now(),
        });

        // Create a watch channel with the running state
        let (tx, rx) = watch::channel(running_state);

        self.active_actions.lock().await.insert(
            client_operation_id.clone(),
            (action_info.clone(), tx.clone()), // keep tx around
        );

        // Add the action to the timeout queue
        self.timeout_queue.lock().await.push(TimeoutEntry {
            timeout_time: now.add(Duration::from_secs(60).min(action_info.timeout)),
            operation_id: client_operation_id.clone(),
        });

        // Notify the timeout monitor task
        self.task_change_notify.notify_one();

        let timeout_duration = Duration::from_secs(60).min(action_info.timeout);

        // Spawn the action execution in a background task so we return immediately.
        let cas_store = self.cas_store.clone();
        let worker_scheduler = self.worker_scheduler.clone();
        let op_id = client_operation_id.clone();
        let info = action_info.clone();
        let dummy_worker_id = WorkerId::default();
        tokio::spawn(async move {
            let result =
                time::timeout(timeout_duration, Self::execute_action(&cas_store, &info)).await;

            let update = match result {
                Ok(Ok(drv_path)) => {
                    event!(
                        Level::INFO,
                        drv_path = ?drv_path.to_absolute_path(),
                        "NixScheduler: Action completed successfully"
                    );
                    UpdateOperationType::ExecutionComplete
                }
                Ok(Err(e)) => {
                    event!(
                        Level::ERROR,
                        error = ?e,
                        "NixScheduler: Action failed with error"
                    );
                    UpdateOperationType::UpdateWithError(e)
                }
                Err(_elapsed) => {
                    event!(Level::WARN, "NixScheduler: Action timed out");
                    UpdateOperationType::UpdateWithActionStage(ActionStage::Completed(
                        nativelink_util::action_messages::ActionResult {
                            exit_code: 124,
                            ..Default::default()
                        },
                    ))
                }
            };

            if let Err(e) = worker_scheduler
                .update_action(&dummy_worker_id, &op_id, update)
                .await
            {
                event!(
                    Level::ERROR,
                    error = ?e,
                    operation_id = ?op_id,
                    "NixScheduler: Failed to update action state"
                );
            }
        });

        Box::new(NixActionStateResult {
            client_operation_id,
            action_info,
            state_rx: rx,
        })
    }

    /// Performs the actual action execution:
    ///  1. Fetch the command and input tree from the CAS
    ///  2. Build a Nix derivation from the inputs
    ///  3. Upload the derivation to the Nix store
    ///
    /// Returns the derivation store path on success.
    async fn execute_action(
        cas_store: &Store,
        action_info: &ActionInfo,
    ) -> Result<StorePath<String>, Error> {
        let command =
            get_and_decode_digest::<ProtoCommand>(cas_store, action_info.command_digest.into())
                .await
                .err_tip(|| "Converting command_digest to Command")?;

        let mut _entries: Vec<PathEntry> = Vec::new();
        let entries = unfold(
            "./".into(),
            &action_info.input_root_digest,
            cas_store,
            &mut _entries,
        )
        .await
        .err_tip(|| "Converting digest to Directory")?;

        let script = entries
            .into_iter()
            .map(|e| {
                format!(
                    "mkdir -p $(dirname {path})\nln {store_path} {path}",
                    path = e.path.to_string_lossy(),
                    store_path = e.store_path.to_absolute_path()
                )
            })
            .chain(once(command.arguments.join(" ")))
            .collect::<Vec<_>>()
            .join("\n");

        let outputs = BTreeMap::from([("out".to_string(), Output::default())]);
        let environment = BTreeMap::from([
            (
                "script".into(),
                BString::new(script.to_owned().as_bytes().to_vec()),
            ),
            ("out".into(), BString::default()),
        ]);
        let mut derivation: Derivation = Derivation {
            arguments: vec!["-ec".into(), "echo lol".into()],
            builder: "/nix/store/lw117lsr8d585xs63kx5k233impyrq7q-bash-5.3p3/bin/bash".into(),
            environment: environment,
            input_derivations: Default::default(),
            input_sources: entries
                .into_iter()
                .map(|e| e.store_path.to_owned())
                .collect(),
            outputs: outputs,
            system: "x86_64-linux".into(),
        };

        let hash_modulo = derivation.hash_derivation_modulo(|_| panic!("Should not be called"));
        derivation
            .calculate_output_paths("reapi-action", &hash_modulo)
            .map_err(|e| {
                nativelink_error::make_err!(
                    Code::Internal,
                    "Failed to calculate output paths: {}",
                    e
                )
            })?;

        let drv_path = derivation
            .calculate_derivation_path("reapi-action")
            .map_err(|e| {
                nativelink_error::make_err!(
                    Code::Internal,
                    "Failed to calculate derivation path: {}",
                    e
                )
            })?;
        let mut serialized_derivation = Vec::new();
        derivation
            .serialize(&mut serialized_derivation)
            .map_err(|e| {
                nativelink_error::make_err!(Code::Internal, "Failed to serialize derivation: {}", e)
            })?;

        cas_store
            .downcast_ref::<NixStore>(None)
            .unwrap()
            .as_pin()
            .add_to_store(
                CAHash::Text([0; 32]).to_nix_nixbase32_string(),
                &serialized_derivation,
                "reapi-action.drv",
                &derivation.input_sources.to_owned().into_iter().collect(),
            )
            .await?;

        Ok(drv_path)
    }
}

#[derive(Clone, Debug)]
struct PathEntry {
    path: PathBuf,
    store_path: StorePath<String>,
}

async fn unfold<'a>(
    root: PathBuf,
    digest: &DigestInfo,
    store: &Store,
    entries: &'a mut Vec<PathEntry>,
) -> Result<&'a mut Vec<PathEntry>, Error> {
    let directory = get_and_decode_digest::<ProtoDirectory>(store, digest.into()).await?;

    for file in directory.files {
        let digest: DigestInfo = file
            .digest
            .err_tip(|| "Expected Digest to exist in Directory::directories::digest")?
            .try_into()
            .err_tip(|| "In Directory::file::digest")?;
        entries.push(PathEntry {
            path: root.join(file.name),
            store_path: key_to_store_path(&StoreKey::Digest(digest))?,
        })
    }

    for dir in directory.directories {
        let digest: DigestInfo = dir
            .digest
            .err_tip(|| "Expected Digest to exist in Directory::directories::digest")?
            .try_into()
            .err_tip(|| "In Directory::file::digest")?;

        Box::pin(unfold(root.join(dir.name), &digest, store, entries)).await?;
    }

    Ok(entries)
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
        // event!(
        //     Level::INFO,
        //     ?filter,
        //     "NixScheduler: Filter operations called"
        // );

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

// Implementation of the WorkerScheduler trait for Nix scheduler.
// Holds shared `active_actions` so it can look up, update, and remove
// operations when workers (or the spawned action tasks) report back.
#[derive(MetricsComponent)]
struct NixWorkerScheduler {
    #[metric(group = "platform_property_manager")]
    platform_property_manager: Arc<PlatformPropertyManager>,

    /// Shared map of active actions — the same instance held by `NixScheduler`.
    active_actions:
        Arc<TokioMutex<HashMap<OperationId, (Arc<ActionInfo>, watch::Sender<Arc<ActionState>>)>>>,
}

#[async_trait]
impl WorkerScheduler for NixWorkerScheduler {
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
        // Look up the operation in active_actions.
        let mut actions = self.active_actions.lock().await;
        let (action_info, state_tx) = actions.get(operation_id).ok_or_else(|| {
            make_err!(
                Code::NotFound,
                "Operation {:?} not found in active actions",
                operation_id
            )
        })?;

        let action_digest = action_info.digest();

        // Map the update type to the final ActionStage.
        let stage = match update {
            UpdateOperationType::ExecutionComplete => {
                ActionStage::Completed(nativelink_util::action_messages::ActionResult {
                    exit_code: 0,
                    ..Default::default()
                })
            }
            UpdateOperationType::UpdateWithActionStage(stage) => stage,
            UpdateOperationType::UpdateWithError(ref e) => {
                ActionStage::Completed(nativelink_util::action_messages::ActionResult {
                    exit_code: 1,
                    error: Some(e.clone()),
                    ..Default::default()
                })
            }
            UpdateOperationType::UpdateWithDisconnect => {
                event!(Level::WARN, ?operation_id, "Worker disconnected");
                ActionStage::Completed(nativelink_util::action_messages::ActionResult {
                    exit_code: 1,
                    ..Default::default()
                })
            }
            UpdateOperationType::KeepAlive => {
                // Nothing to change for keep-alive.
                return Ok(());
            }
        };

        // Send the final state to all watchers.
        let completed_state = Arc::new(ActionState {
            client_operation_id: operation_id.clone(),
            stage,
            action_digest,
            last_transition_timestamp: SystemTime::now(),
        });
        drop(state_tx.send(completed_state));

        // Remove the action from active_actions. This also effectively
        // cancels any pending timeout — the timeout monitor will skip
        // entries whose operation_id is no longer in the map.
        actions.remove(operation_id);

        event!(
            Level::INFO,
            ?operation_id,
            "NixWorkerScheduler: Action completed and removed from active actions"
        );

        Ok(())
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

impl RootMetricsComponent for NixWorkerScheduler {}
