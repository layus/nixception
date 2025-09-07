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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use mock_instant::thread_local::MockClock;
use nativelink_config::schedulers::NixProxySpec;
use nativelink_config::stores::MemorySpec;
use nativelink_error::Error;
use nativelink_macro::nativelink_test;
use nativelink_scheduler::default_scheduler_factory::memory_awaited_action_db_factory;
use nativelink_scheduler::nix_scheduler::NixScheduler;
use nativelink_store::memory_store::MemoryStore;
use nativelink_util::action_messages::{
    ActionInfo, ActionStage, ActionUniqueKey, ActionUniqueQualifier, OperationId,
};
use nativelink_util::common::DigestInfo;
use nativelink_util::digest_hasher::DigestHasherFunc;
use nativelink_util::instant_wrapper::MockInstantWrapped;
use nativelink_util::operation_state_manager::{
    ClientStateManager, OperationFilter, OperationStageFlags,
};
use nativelink_util::store_trait::Store;
use tokio::sync::Notify;

// Constants for testing
const INSTANCE_NAME: &str = "test_instance";

// Helper function to create a test action info with a specific timeout
fn create_test_action_info(digest: DigestInfo, timeout_secs: u64) -> Arc<ActionInfo> {
    Arc::new(ActionInfo {
        command_digest: DigestInfo::zero_digest(),
        input_root_digest: DigestInfo::zero_digest(),
        timeout: Duration::from_secs(timeout_secs),
        platform_properties: HashMap::new(),
        priority: 0,
        load_timestamp: UNIX_EPOCH,
        insert_timestamp: SystemTime::now(),
        unique_qualifier: ActionUniqueQualifier::Cacheable(ActionUniqueKey {
            instance_name: INSTANCE_NAME.to_string(),
            digest_function: DigestHasherFunc::Sha256,
            digest,
        }),
    })
}

// Removed unused helper function

// Test removed as NixScheduler no longer immediately completes actions
// Instead, actions stay in running state until they time out

#[nativelink_test]
async fn test_nix_scheduler_action_timeout() -> Result<(), Error> {
    // Set mock clock to a deterministic starting point
    MockClock::set_time(Duration::from_secs(11363015));

    // Create a NixScheduler
    let task_change_notify = Arc::new(Notify::new());
    let awaited_action_db = memory_awaited_action_db_factory(
        0,
        &task_change_notify.clone(),
        MockInstantWrapped::default,
    );

    let ac_store = Store::new(MemoryStore::new(&MemorySpec::default()));
    let (scheduler, _worker_scheduler) = NixScheduler::new(
        &NixProxySpec::default(),
        awaited_action_db,
        task_change_notify.clone(),
        MockInstantWrapped::default,
        ac_store,
    );

    // Create a test action with a small timeout (1 second)
    // This will be added to the priority queue with a timeout 1 second from now
    let action_digest = DigestInfo::new([2u8; 32], 100);
    let action_info = create_test_action_info(action_digest, 1); // 1 second timeout
    let client_operation_id = OperationId::default();

    // Add the action to the scheduler
    let action_result = scheduler
        .add_action(client_operation_id.clone(), action_info.clone())
        .await?;

    // Verify the action is in running state
    let (state, _) = action_result.as_state().await?;
    match &state.stage {
        ActionStage::Executing => {
            // Expected state - action should be running
        }
        other => {
            panic!("Expected ActionStage::Executing, got: {:?}", other);
        }
    }

    // Should be able to find the action using filter_operations
    let filter = OperationFilter {
        operation_id: Some(client_operation_id.clone()),
        stages: OperationStageFlags::Executing,
        ..Default::default()
    };

    let mut filter_stream = scheduler.filter_operations(filter).await?;
    let found_action = filter_stream.next().await;
    assert!(
        found_action.is_some(),
        "Action should be found in filter results"
    );

    tokio::task::yield_now().await;

    // Advance mock clock by 2 seconds to simulate timeout
    MockClock::advance(Duration::from_secs(2));

    task_change_notify.notify_one();
    tokio::task::yield_now().await;

    // Check that the action has timed out
    let (state, _) = action_result.as_state().await?;
    match &state.stage {
        ActionStage::Completed(result) => {
            // Action should be completed with a timeout error
            assert_eq!(result.exit_code, 124, "Expected timeout exit code 124");
        }
        other => {
            panic!("Expected ActionStage::Completed, got: {:?}", other);
        }
    }

    Ok(())
}

#[nativelink_test]
async fn test_nix_scheduler_empty_filter_results() -> Result<(), Error> {
    // Create a NixScheduler with no actions
    let task_change_notify = Arc::new(Notify::new());
    let awaited_action_db = memory_awaited_action_db_factory(
        0,
        &task_change_notify.clone(),
        MockInstantWrapped::default,
    );
    let ac_store = Store::new(MemoryStore::new(&MemorySpec::default()));

    let (scheduler, _worker_scheduler) = NixScheduler::new(
        &NixProxySpec::default(),
        awaited_action_db,
        task_change_notify,
        MockInstantWrapped::default,
        ac_store,
    );

    // Call filter_operations with no actions added - should return an empty stream
    let mut filter_stream = scheduler.filter_operations(Default::default()).await?;

    // Should not have any results in the stream
    let next_item = filter_stream.next().await;
    assert!(
        next_item.is_none(),
        "Expected empty stream from filter_operations"
    );

    Ok(())
}
