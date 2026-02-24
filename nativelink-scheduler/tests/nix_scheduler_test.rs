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

// When the CAS store is empty the background task fails immediately during
// derivation preparation (command digest not found).  The action should
// transition from Queued → Completed with an error (exit code 1).

#[nativelink_test]
async fn test_nix_scheduler_action_fails_with_empty_cas() -> Result<(), Error> {
    // Set mock clock to a deterministic starting point
    MockClock::set_time(Duration::from_secs(11363015));

    // Create a NixScheduler with empty CAS — execute_action will fail
    // immediately because the command digest cannot be found.
    let task_change_notify = Arc::new(Notify::new());
    let awaited_action_db = memory_awaited_action_db_factory(
        0,
        &task_change_notify.clone(),
        MockInstantWrapped::default,
    );

    let ac_store = Store::new(MemoryStore::new(&MemorySpec::default()));
    let cas_store = Store::new(MemoryStore::new(&MemorySpec::default()));
    let (scheduler, _worker_scheduler) = NixScheduler::new(
        &NixProxySpec::default(),
        awaited_action_db,
        task_change_notify.clone(),
        MockInstantWrapped::default,
        ac_store,
        cas_store,
    );

    let action_digest = DigestInfo::new([2u8; 32], 100);
    let action_info = create_test_action_info(action_digest, 1);
    let client_operation_id = OperationId::default();

    // Add the action to the scheduler
    let mut action_result = scheduler
        .add_action(client_operation_id.clone(), action_info.clone())
        .await?;

    // The action starts in Queued state.
    let (state, _) = action_result.as_state().await?;
    match &state.stage {
        ActionStage::Queued => { /* expected */ }
        other => {
            panic!("Expected ActionStage::Queued, got: {:?}", other);
        }
    }

    // Should be able to find the action using filter_operations
    let filter = OperationFilter {
        operation_id: Some(client_operation_id.clone()),
        stages: OperationStageFlags::Queued,
        ..Default::default()
    };

    let mut filter_stream = scheduler.filter_operations(filter).await?;
    let found_action = filter_stream.next().await;
    assert!(
        found_action.is_some(),
        "Action should be found in filter results"
    );

    // Wait for the background task to complete — it will fail because the
    // CAS is empty and transition the action straight to Completed.
    let (state, _) = action_result.changed().await?;
    match &state.stage {
        ActionStage::Completed(result) => {
            assert_eq!(
                result.exit_code, 1,
                "Expected error exit code 1 (derivation preparation failed)"
            );
            assert!(
                result.error.is_some(),
                "Expected an error to be attached to the result"
            );
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
    let cas_store = Store::new(MemoryStore::new(&MemorySpec::default()));

    let (scheduler, _worker_scheduler) = NixScheduler::new(
        &NixProxySpec::default(),
        awaited_action_db,
        task_change_notify,
        MockInstantWrapped::default,
        ac_store,
        cas_store,
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
