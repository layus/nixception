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

use nativelink_metric::{MetricsComponent, RootMetricsComponent};
use nativelink_util::action_messages::{
    ActionInfo, ActionResult, ActionStage, ActionState, OperationId,
};
use nativelink_util::common::DigestInfo;
use nativelink_util::spawn;
use nativelink_util::store_trait::Store;
use nativelink_util::task::JoinHandleDropGuard;
use tokio::sync::{watch, Mutex as TokioMutex, Notify};
use tokio::time::Duration;
use tracing::{event, Level};

/// Struct for the Nix executor/worker that processes tasks
#[derive(MetricsComponent)]
pub struct NixExecutor {
    /// All active actions (operation_id -> action state channel sender)
    active_actions: Arc<TokioMutex<HashMap<OperationId, (Arc<ActionInfo>, watch::Sender<Arc<ActionState>>)>>>,

    /// Store manager for accessing content
    ac_store: Store,

    /// Background task for monitoring tasks and handling timeouts
    _task_monitoring_handle: JoinHandleDropGuard<()>,
}

impl NixExecutor {
    /// Creates a new NixExecutor that monitors tasks and handles timeouts
    pub fn new(
        active_actions: Arc<TokioMutex<HashMap<OperationId, (Arc<ActionInfo>, watch::Sender<Arc<ActionState>>)>>>,
        task_change_notify: Arc<Notify>,
        ac_store: Store,
    ) -> Self {
        let active_actions_clone = active_actions.clone();

        let task_monitoring_handle = spawn!("nix_executor_task_monitoring", async move {
            // Monitor active actions for timeouts
            loop {
                match task_change_notify.notified().await {
                    () => {
                        // Sleep a bit then check for timeouts
                        tokio::time::sleep(Duration::from_millis(100)).await;

                        // Lock the actions
                        let actions = active_actions_clone.lock().await;

                        // Find actions that have timed out
                        let now = SystemTime::now();
                        let timed_out: Vec<_> = actions
                            .iter()
                            .filter_map(|(op_id, (action_info, _state_tx))| {
                                let start_time = SystemTime::now()
                                    .checked_sub(Duration::from_secs(10))
                                    .unwrap_or(SystemTime::now());
                                let timeout_duration = action_info.timeout;

                                match now.duration_since(start_time) {
                                    Ok(elapsed) if elapsed > timeout_duration => {
                                        Some(op_id.clone())
                                    }
                                    _ => None,
                                }
                            })
                            .collect();

                        // Update the timed out actions
                        for op_id in &timed_out {
                            if let Some((action_info, state_tx)) = actions.get(op_id) {
                                // We don't need to retrieve the action command info here
                                let action_digest = action_info.digest();

                                // Create timeout error message
                                let mut result = ActionResult::default();
                                result.exit_code = 124; // Timeout exit code
                                result.message = "Action timed out".to_string();
                                result.stderr_digest = DigestInfo::zero_digest();

                                // Update the state
                                let new_state = Arc::new(ActionState {
                                    client_operation_id: op_id.clone(),
                                    stage: ActionStage::Completed(result),
                                    action_digest,
                                });

                                let _ = state_tx.send(new_state);

                                event!(
                                    Level::INFO,
                                    ?op_id,
                                    ?action_digest,
                                    command_digest = ?action_info.command_digest,
                                    input_root_digest = ?action_info.input_root_digest,
                                    timeout_secs = ?action_info.timeout.as_secs(),
                                    platform_props = ?action_info.platform_properties,
                                    "NixExecutor: Action timed out after {}s",
                                    action_info.timeout.as_secs()
                                );
                            }
                        }

                        // Remove timed out actions after a small delay to allow clients to see the completed state
                        let timed_out_vec = timed_out.clone();
                        let actions_clone = active_actions_clone.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            let mut actions = actions_clone.lock().await;
                            for op_id in timed_out_vec {
                                actions.remove(&op_id);
                            }
                        });
                    }
                }
            }
        });

        event!(Level::INFO, "NixExecutor: Initialized");

        NixExecutor {
            active_actions,
            ac_store,
            _task_monitoring_handle: task_monitoring_handle,
        }
    }

    /// Gets a reference to the active actions
    pub fn active_actions(&self) -> Arc<TokioMutex<HashMap<OperationId, (Arc<ActionInfo>, watch::Sender<Arc<ActionState>>)>>> {
        self.active_actions.clone()
    }

    /// Gets a reference to the content store
    pub fn store(&self) -> Store {
        self.ac_store.clone()
    }
}

impl RootMetricsComponent for NixExecutor {}
