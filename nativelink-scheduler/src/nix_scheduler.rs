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
use nativelink_error::{Error, ResultExt};
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
use nix_compat::store_path::StorePath;

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
                nativelink_error::Code::Internal,
                "NixActionStateResult: changed() failed, channel closed"
            ))
        }
    }

    async fn as_action_info(&self) -> Result<(Arc<ActionInfo>, Option<OriginMetadata>), Error> {
        Ok((self.action_info.clone(), None))
    }
}

/// A simplified Nix scheduler that simulates running actions until timeout
#[derive(MetricsComponent, Debug)]
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

            let timeout_queue = Arc::new(TokioMutex::new(BinaryHeap::new()));

            let scheduler = NixScheduler {
                platform_property_manager,
                active_actions: active_actions.clone(),
                timeout_queue: timeout_queue.clone(),
                ac_store,
                cas_store,
                now_fn: now_fn.clone(),
                task_change_notify: task_change_notify.clone(),
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

        (action_scheduler, worker_scheduler_clone)
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
            (action_info.clone(), tx), // keep tx around
        );

        // Add the action to the timeout queue
        self.timeout_queue.lock().await.push(TimeoutEntry {
            timeout_time: now.add(Duration::from_secs(60).min(action_info.timeout)),
            operation_id: client_operation_id.clone(),
        });

        // Notify the timeout monitor task
        self.task_change_notify.notify_one();

        let command = get_and_decode_digest::<ProtoCommand>(
            &self.cas_store,
            action_info.command_digest.into(),
        )
        .await
        .err_tip(|| "Converting command_digest to Command")
        .unwrap();

        let mut _entries: Vec<PathEntry> = Vec::new();
        let entries = unfold(
            "./".into(),
            &action_info.input_root_digest,
            &self.cas_store,
            &mut _entries,
        )
        .await
        .err_tip(|| "Converting digest to Directory")
        .unwrap();

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
        // Upload script to store (unused, we pass it as env var for now)
        // let script_digest: DigestInfo =
        //     compute_buf_digest(script.as_bytes(), &mut DigestHasherFunc::Sha256.hasher());
        // self.cas_store
        //     .update_oneshot(script_digest, script.to_owned().into())
        //     .await
        //     .unwrap();
        // let _script_path = key_to_store_path(&StoreKey::Digest(script_digest))
        //     .unwrap()
        //     .to_absolute_path();

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
            // input_sources: Default::default(),
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
            .unwrap();

        let drv_path = derivation
            .calculate_derivation_path("reapi-action")
            .unwrap();
        let mut serialized_derivation = Vec::new();
        derivation.serialize(&mut serialized_derivation).unwrap();

        self.cas_store
            .downcast_ref::<NixStore>(None)
            .unwrap()
            .as_pin()
            .add_to_store(
                CAHash::Text([0; 32]).to_nix_nixbase32_string(),
                &serialized_derivation,
                "reapi-action.drv",
                &derivation.input_sources.to_owned().into_iter().collect(),
            )
            .await
            .unwrap();

        // Log detailed information about the received action
        // Note: In a production environment, we could use the ac_store (currently just storing the name: {})
        // to fetch and log the actual command details using the command_digest
        event!(
            Level::INFO,
            //?client_operation_id,
            //?action_digest,
            //?action_info,
            //command_digest = ?action_info.command_digest,
            //input_root_digest = ?action_info.input_root_digest,
            //timeout_secs = ?action_info.timeout.as_secs(),
            //platform_props = ?action_info.platform_properties,
            //priority = ?action_info.priority,
            //qualifier = ?action_info.unique_qualifier,
            drv_path = ?drv_path.to_absolute_path(),
            "NixScheduler: Received action with detailed info (will timeout)"
        );

        Box::new(NixActionStateResult {
            client_operation_id,
            action_info,
            state_rx: rx,
        })
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
        // event!(Level::INFO, worker_id = ?worker.id, "NixWorkerScheduler: Adding worker (no-op)");
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
        _worker_id: &WorkerId,
        _operation_id: &OperationId,
        _update: UpdateOperationType,
    ) -> Result<(), Error> {
        // event!(
        //     Level::INFO,
        //     ?worker_id,
        //     ?operation_id,
        //     ?update,
        //     "NixWorkerScheduler: Update action (no-op)"
        // );
        Ok(())
    }

    async fn worker_keep_alive_received(
        &self,
        _worker_id: &WorkerId,
        _timestamp: WorkerTimestamp,
    ) -> Result<(), Error> {
        // event!(
        //     Level::DEBUG,
        //     ?worker_id,
        //     ?timestamp,
        //     "NixWorkerScheduler: Worker keep-alive received (no-op)"
        // );
        Ok(())
    }

    async fn remove_worker(&self, _worker_id: &WorkerId) -> Result<(), Error> {
        // event!(
        //     Level::INFO,
        //     ?worker_id,
        //     "NixWorkerScheduler: Removing worker (no-op)"
        // );
        Ok(())
    }

    async fn remove_timedout_workers(&self, _now_timestamp: WorkerTimestamp) -> Result<(), Error> {
        // event!(
        //     Level::DEBUG,
        //     ?now_timestamp,
        //     "NixWorkerScheduler: Removing timed-out workers (no-op)"
        // );
        Ok(())
    }

    async fn set_drain_worker(
        &self,
        _worker_id: &WorkerId,
        _is_draining: bool,
    ) -> Result<(), Error> {
        // event!(
        //     Level::INFO,
        //     ?worker_id,
        //     ?is_draining,
        //     "NixWorkerScheduler: Setting worker drain status (no-op)"
        // );
        Ok(())
    }

    async fn shutdown(&self, _shutdown_guard: ShutdownGuard) {}
}

impl<I: InstantWrapper, NowFn: Fn() -> I + Clone + Send + Unpin + Sync + 'static>
    RootMetricsComponent for NixScheduler<I, NowFn>
{
}

impl RootMetricsComponent for NixWorkerScheduler {}
