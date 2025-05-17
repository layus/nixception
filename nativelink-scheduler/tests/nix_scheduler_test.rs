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
use std::time::{SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use nativelink_config::schedulers::NixProxySpec;
use nativelink_error::Error;
use nativelink_macro::nativelink_test;
use nativelink_scheduler::default_scheduler_factory::memory_awaited_action_db_factory;
use nativelink_scheduler::nix_scheduler::NixScheduler;
use nativelink_util::action_messages::{
    ActionInfo, ActionStage, ActionUniqueKey, ActionUniqueQualifier, OperationId, WorkerId,
};
use nativelink_util::common::DigestInfo;
use nativelink_util::digest_hasher::DigestHasherFunc;
use nativelink_util::instant_wrapper::MockInstantWrapped;
use nativelink_util::operation_state_manager::ClientStateManager;
use nativelink_util::platform_properties::PlatformProperties;
use tokio::sync::{mpsc, Notify};
use uuid::Uuid;

// Constants for testing
const INSTANCE_NAME: &str = "test_instance";

// Helper function to create a test action info
fn create_test_action_info(digest: DigestInfo) -> Arc<ActionInfo> {
    Arc::new(ActionInfo {
        command_digest: DigestInfo::zero_digest(),
        input_root_digest: DigestInfo::zero_digest(),
        timeout: std::time::Duration::from_secs(60),
        platform_properties: HashMap::new(),
        priority: 0,
        load_timestamp: UNIX_EPOCH,
        insert_timestamp: SystemTime::now(),
        unique_qualifier: ActionUniqueQualifier::Cachable(ActionUniqueKey {
            instance_name: INSTANCE_NAME.to_string(),
            digest_function: DigestHasherFunc::Sha256,
            digest,
        }),
    })
}

#[nativelink_test]
async fn test_nix_scheduler_immediate_completion() -> Result<(), Error> {
    // Create a NixScheduler
    let task_change_notify = Arc::new(Notify::new());
    let awaited_action_db = memory_awaited_action_db_factory(
        0,
        &task_change_notify.clone(),
        MockInstantWrapped::default,
    );

    let (scheduler, _worker_scheduler) = NixScheduler::new(
        &NixProxySpec::default(),
        awaited_action_db,
        task_change_notify,
    );

    // Create a test action
    let action_digest = DigestInfo::new([1u8; 32], 100);
    let action_info = create_test_action_info(action_digest);
    let client_operation_id = OperationId::default();

    // Add the action to the scheduler - it should return immediately with a completed result
    let action_result = scheduler
        .add_action(client_operation_id.clone(), action_info.clone())
        .await?;

    // The action should be immediately completed
    let state = action_result.as_state().await?;

    // Verify the state shows completed
    match &state.stage {
        ActionStage::Completed(_) => {
            // Success! The action was immediately completed as expected
        }
        other => {
            panic!("Expected ActionStage::Completed, got: {:?}", other);
        }
    }

    Ok(())
}

#[nativelink_test]
async fn test_nix_scheduler_empty_filter_results() -> Result<(), Error> {
    // Create a NixScheduler
    let task_change_notify = Arc::new(Notify::new());
    let awaited_action_db = memory_awaited_action_db_factory(
        0,
        &task_change_notify.clone(),
        MockInstantWrapped::default,
    );

    let (scheduler, _worker_scheduler) = NixScheduler::new(
        &NixProxySpec::default(),
        awaited_action_db,
        task_change_notify,
    );

    // Call filter_operations - it should return an empty stream
    let mut filter_stream = scheduler.filter_operations(Default::default()).await?;

    // Should not have any results in the stream
    let next_item = filter_stream.next().await;
    assert!(
        next_item.is_none(),
        "Expected empty stream from filter_operations"
    );

    Ok(())
}

#[nativelink_test]
async fn test_nix_scheduler_worker_operations() -> Result<(), Error> {
    // Create a NixScheduler
    let task_change_notify = Arc::new(Notify::new());
    let awaited_action_db = memory_awaited_action_db_factory(
        0,
        &task_change_notify.clone(),
        MockInstantWrapped::default,
    );

    let (scheduler, worker_scheduler) = NixScheduler::new(
        &NixProxySpec::default(),
        awaited_action_db,
        task_change_notify,
    );

    // Create a worker
    let worker_id = WorkerId(Uuid::new_v4());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let worker = nativelink_scheduler::worker::Worker::new(
        worker_id.clone(),
        PlatformProperties::default(),
        tx,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    );

    // Add the worker - should receive acknowledgment
    worker_scheduler.add_worker(worker).await?;

    // Should receive the initial connection message
    let connection_message = rx.recv().await;
    assert!(connection_message.is_some(), "Expected connection message");

    // Test worker operations - they should all succeed as no-ops
    worker_scheduler
        .worker_keep_alive_received(
            &worker_id,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .await?;

    worker_scheduler.set_drain_worker(&worker_id, true).await?;

    worker_scheduler.remove_worker(&worker_id).await?;

    // No messages should be sent after the initial connection
    let timeout = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
    assert!(
        timeout.is_err() || timeout.unwrap().is_none(),
        "Expected no more messages after connection response"
    );

    Ok(())
}
