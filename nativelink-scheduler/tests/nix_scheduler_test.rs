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
use nativelink_error::{Code, Error};
use nativelink_macro::nativelink_test;
use nativelink_proto::com::github::trace_machina::nativelink::remote_execution::UpdateForWorker;
use nativelink_scheduler::default_scheduler_factory::memory_awaited_action_db_factory;
use nativelink_scheduler::nix_scheduler::NixScheduler;
use nativelink_scheduler::runner_info::RunnerInfo;
use nativelink_scheduler::worker::Worker;
use nativelink_scheduler::worker_scheduler::WorkerScheduler;
use nativelink_store::memory_store::MemoryStore;
use nativelink_store::nix_daemon_connection::NixDaemonConnectionPool;
use nativelink_util::action_messages::{
    ActionInfo, ActionStage, ActionUniqueKey, ActionUniqueQualifier, INTERNAL_ERROR_EXIT_CODE,
    OperationId, WorkerId,
};
use nativelink_util::common::DigestInfo;
use nativelink_util::digest_hasher::DigestHasherFunc;
use nativelink_util::instant_wrapper::MockInstantWrapped;
#[allow(unused_imports)]
use nativelink_util::known_platform_property_provider::KnownPlatformPropertyProvider;
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

/// Helper: build a NixScheduler and return (scheduler_arc, worker_scheduler_arc).
fn make_nix_scheduler() -> (
    Arc<
        NixScheduler<
            impl nativelink_scheduler::awaited_action_db::AwaitedActionDb,
            MockInstantWrapped,
            fn() -> MockInstantWrapped,
        >,
    >,
    Arc<dyn WorkerScheduler>,
) {
    let task_change_notify = Arc::new(Notify::new());
    let awaited_action_db = memory_awaited_action_db_factory(
        0,
        &task_change_notify.clone(),
        MockInstantWrapped::default,
    );
    let ac_store = Store::new(MemoryStore::new(&MemorySpec::default()));
    let cas_store = Store::new(MemoryStore::new(&MemorySpec::default()));
    let nix_connection = NixDaemonConnectionPool::new_default("/dev/null/fake-socket".to_string());
    NixScheduler::new(
        &NixProxySpec::default(),
        awaited_action_db,
        task_change_notify,
        MockInstantWrapped::default,
        ac_store,
        cas_store,
        nix_connection,
        Arc::new(RunnerInfo::dummy()),
    )
}

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
    let nix_connection = NixDaemonConnectionPool::new_default("/dev/null/fake-socket".to_string());
    let (scheduler, _worker_scheduler) = NixScheduler::new(
        &NixProxySpec::default(),
        awaited_action_db,
        task_change_notify.clone(),
        MockInstantWrapped::default,
        ac_store,
        cas_store,
        nix_connection,
        Arc::new(RunnerInfo::dummy()),
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
        client_operation_id: Some(client_operation_id.clone()),
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
    // The first `changed()` may return the initial Queued state (from the
    // subscriber's `mark_changed`), so loop until we reach a terminal stage.
    let final_state = loop {
        let (state, _) = action_result.changed().await?;
        if state.stage.is_finished() {
            break state;
        }
    };
    match &final_state.stage {
        ActionStage::Completed(result) => {
            assert_eq!(
                result.exit_code, INTERNAL_ERROR_EXIT_CODE,
                "Expected INTERNAL_ERROR_EXIT_CODE (derivation preparation failed)"
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

// ---------------------------------------------------------------------------
// WorkerScheduler method tests
// ---------------------------------------------------------------------------

/// `add_worker` must return `Unimplemented` — the NixScheduler does not
/// accept external worker registrations.
#[nativelink_test]
async fn test_nix_scheduler_add_worker_returns_unimplemented() -> Result<(), Error> {
    let (_scheduler, worker_scheduler) = make_nix_scheduler();

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<UpdateForWorker>();
    let worker = Worker::new(
        WorkerId("test-worker".into()),
        nativelink_util::platform_properties::PlatformProperties::default(),
        tx,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    );

    let result = worker_scheduler.add_worker(worker).await;
    assert!(result.is_err(), "add_worker should return an error");
    let err = result.unwrap_err();
    assert_eq!(
        err.code,
        Code::Unimplemented,
        "Expected Unimplemented error from add_worker"
    );
    Ok(())
}

/// The no-op WorkerScheduler methods should succeed without panicking.
#[nativelink_test]
async fn test_nix_scheduler_worker_scheduler_noop_methods() -> Result<(), Error> {
    let (_scheduler, worker_scheduler) = make_nix_scheduler();

    let worker_id = WorkerId("nix-worker-test".into());

    // remove_worker is a no-op
    worker_scheduler.remove_worker(&worker_id).await?;

    // worker_keep_alive_received is a no-op
    worker_scheduler
        .worker_keep_alive_received(&worker_id, 42)
        .await?;

    // remove_timedout_workers is a no-op
    worker_scheduler.remove_timedout_workers(100).await?;

    // set_drain_worker is a no-op
    worker_scheduler.set_drain_worker(&worker_id, true).await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Cacheable action deduplication
// ---------------------------------------------------------------------------

/// Two clients submitting the same cacheable action digest should share the
/// underlying operation.  Both should start in Queued state.  Inspired by
/// `cacheable_items_join_same_action_queued_test` in simple_scheduler_test.
#[nativelink_test]
async fn test_nix_scheduler_cacheable_actions_join() -> Result<(), Error> {
    MockClock::set_time(Duration::from_secs(11363015));
    let (scheduler, _worker_scheduler) = make_nix_scheduler();

    let action_digest = DigestInfo::new([42u8; 32], 200);
    let action_info = create_test_action_info(action_digest, 30);

    let client_op_id_1 = OperationId::default();
    let client_op_id_2 = OperationId::default();

    let mut listener1 = scheduler
        .add_action(client_op_id_1.clone(), action_info.clone())
        .await?;
    let mut listener2 = scheduler
        .add_action(client_op_id_2.clone(), action_info.clone())
        .await?;

    // Both clients should see the initial Queued state.
    let (state1, _) = listener1.as_state().await?;
    let (state2, _) = listener2.as_state().await?;
    assert!(
        matches!(state1.stage, ActionStage::Queued),
        "Client 1 expected Queued, got {:?}",
        state1.stage
    );
    assert!(
        matches!(state2.stage, ActionStage::Queued),
        "Client 2 expected Queued, got {:?}",
        state2.stage
    );

    // Each client should have its own operation id.
    assert_ne!(
        state1.client_operation_id, state2.client_operation_id,
        "Each client should receive a unique client operation id"
    );

    // Both should eventually reach the same terminal state (error, since
    // CAS is empty).
    let final1 = loop {
        let (s, _) = listener1.changed().await?;
        if s.stage.is_finished() {
            break s;
        }
    };
    let final2 = loop {
        let (s, _) = listener2.changed().await?;
        if s.stage.is_finished() {
            break s;
        }
    };
    assert!(final1.stage.is_finished());
    assert!(final2.stage.is_finished());

    Ok(())
}

// ---------------------------------------------------------------------------
// Multiple distinct actions
// ---------------------------------------------------------------------------

/// Submitting actions with different digests should create independent
/// operations that can be found separately via filter_operations.
#[nativelink_test]
async fn test_nix_scheduler_multiple_distinct_actions() -> Result<(), Error> {
    MockClock::set_time(Duration::from_secs(11363015));
    let (scheduler, _worker_scheduler) = make_nix_scheduler();

    let digest_a = DigestInfo::new([10u8; 32], 100);
    let digest_b = DigestInfo::new([20u8; 32], 200);
    let action_a = create_test_action_info(digest_a, 30);
    let action_b = create_test_action_info(digest_b, 30);

    let op_id_a = OperationId::default();
    let op_id_b = OperationId::default();

    let listener_a = scheduler.add_action(op_id_a.clone(), action_a).await?;
    let listener_b = scheduler.add_action(op_id_b.clone(), action_b).await?;

    let (state_a, _) = listener_a.as_state().await?;
    let (state_b, _) = listener_b.as_state().await?;

    // Both should be queued but with different digests.
    assert!(matches!(state_a.stage, ActionStage::Queued));
    assert!(matches!(state_b.stage, ActionStage::Queued));
    assert_ne!(
        state_a.action_digest, state_b.action_digest,
        "Distinct actions should have different digests"
    );

    // Each should be findable by its own client_operation_id.
    let mut stream_a = scheduler
        .filter_operations(OperationFilter {
            client_operation_id: Some(op_id_a.clone()),
            stages: OperationStageFlags::Queued,
            ..Default::default()
        })
        .await?;
    assert!(
        stream_a.next().await.is_some(),
        "Action A should be found by its operation id"
    );

    let mut stream_b = scheduler
        .filter_operations(OperationFilter {
            client_operation_id: Some(op_id_b.clone()),
            stages: OperationStageFlags::Queued,
            ..Default::default()
        })
        .await?;
    assert!(
        stream_b.next().await.is_some(),
        "Action B should be found by its operation id"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Client reconnect
// ---------------------------------------------------------------------------

/// After dropping a listener, the client should be able to reconnect to the
/// same action via `filter_operations` using its `client_operation_id`.
/// Inspired by `client_reconnect_keeps_action_alive` in simple_scheduler_test.
#[nativelink_test]
async fn test_nix_scheduler_client_reconnect() -> Result<(), Error> {
    MockClock::set_time(Duration::from_secs(11363015));
    let (scheduler, _worker_scheduler) = make_nix_scheduler();

    let action_digest = DigestInfo::new([50u8; 32], 300);
    let action_info = create_test_action_info(action_digest, 60);
    let client_op_id = OperationId::default();

    let listener = scheduler
        .add_action(client_op_id.clone(), action_info)
        .await?;

    // Retrieve the client operation id from the listener.
    let (initial_state, _) = listener.as_state().await?;
    let retrieved_op_id = initial_state.client_operation_id.clone();

    // Simulate client disconnecting.
    drop(listener);

    // Reconnect using filter_operations.
    let mut reconnect_stream = scheduler
        .filter_operations(OperationFilter {
            client_operation_id: Some(retrieved_op_id.clone()),
            ..Default::default()
        })
        .await?;

    let reconnected = reconnect_stream.next().await;
    assert!(
        reconnected.is_some(),
        "Should be able to reconnect to the action after dropping the listener"
    );

    let reconnected_listener = reconnected.unwrap();
    let (reconnected_state, _) = reconnected_listener.as_state().await?;
    assert_eq!(
        reconnected_state.client_operation_id, retrieved_op_id,
        "Reconnected action should have the same client operation id"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// KnownPlatformPropertyProvider
// ---------------------------------------------------------------------------

/// The NixScheduler is created with default (empty) platform properties,
/// so `get_known_properties` should return an empty vec.
#[nativelink_test]
async fn test_nix_scheduler_known_platform_properties_empty() -> Result<(), Error> {
    let (scheduler, _worker_scheduler) = make_nix_scheduler();

    let provider = scheduler
        .as_known_platform_property_provider()
        .expect("NixScheduler should implement KnownPlatformPropertyProvider");
    let properties = provider.get_known_properties(INSTANCE_NAME).await?;
    assert!(
        properties.is_empty(),
        "Default NixScheduler should have no known platform properties, got: {:?}",
        properties
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Stage-based filtering
// ---------------------------------------------------------------------------

/// A freshly-added action should be found when filtering for Queued actions
/// but NOT when filtering for Executing or Completed actions.
#[nativelink_test]
async fn test_nix_scheduler_filter_by_stage_flags() -> Result<(), Error> {
    MockClock::set_time(Duration::from_secs(11363015));
    let (scheduler, _worker_scheduler) = make_nix_scheduler();

    let action_digest = DigestInfo::new([77u8; 32], 400);
    let action_info = create_test_action_info(action_digest, 60);
    let client_op_id = OperationId::default();

    let _listener = scheduler
        .add_action(client_op_id.clone(), action_info)
        .await?;

    // Should be found when filtering for Queued.
    let mut queued_stream = scheduler
        .filter_operations(OperationFilter {
            stages: OperationStageFlags::Queued,
            ..Default::default()
        })
        .await?;
    assert!(
        queued_stream.next().await.is_some(),
        "Action should be found when filtering for Queued stage"
    );

    // Should NOT be found when filtering for only Completed.
    let mut completed_stream = scheduler
        .filter_operations(OperationFilter {
            stages: OperationStageFlags::Completed,
            ..Default::default()
        })
        .await?;
    assert!(
        completed_stream.next().await.is_none(),
        "No actions should be found when filtering for Completed stage only"
    );

    Ok(())
}

/// After the background worker finishes (action fails because CAS is empty),
/// the completed action should be findable with Completed stage filter.
#[nativelink_test]
async fn test_nix_scheduler_filter_completed_after_failure() -> Result<(), Error> {
    MockClock::set_time(Duration::from_secs(11363015));
    let (scheduler, _worker_scheduler) = make_nix_scheduler();

    let action_digest = DigestInfo::new([88u8; 32], 500);
    let action_info = create_test_action_info(action_digest, 1);
    let client_op_id = OperationId::default();

    let mut listener = scheduler
        .add_action(client_op_id.clone(), action_info)
        .await?;

    // Wait for the action to complete (will fail because CAS is empty).
    loop {
        let (state, _) = listener.changed().await?;
        if state.stage.is_finished() {
            break;
        }
    }

    // Now it should appear under Completed filter.
    let mut completed_stream = scheduler
        .filter_operations(OperationFilter {
            stages: OperationStageFlags::Completed,
            ..Default::default()
        })
        .await?;
    assert!(
        completed_stream.next().await.is_some(),
        "Completed action should be found when filtering for Completed stage"
    );

    // And should NOT appear under Queued filter anymore.
    let mut queued_stream = scheduler
        .filter_operations(OperationFilter {
            stages: OperationStageFlags::Queued,
            ..Default::default()
        })
        .await?;
    assert!(
        queued_stream.next().await.is_none(),
        "Completed action should not appear when filtering for Queued stage"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// update_action via WorkerScheduler
// ---------------------------------------------------------------------------

/// Calling `update_action` on the worker scheduler with a bogus operation id
/// should not panic.  The current implementation logs a warning and returns
/// `Ok(())` when the operation is missing (it was likely already dropped).
#[nativelink_test]
async fn test_nix_scheduler_update_action_unknown_operation_does_not_panic() -> Result<(), Error> {
    let (_scheduler, worker_scheduler) = make_nix_scheduler();

    let bogus_worker_id = WorkerId("bogus-worker".into());
    let bogus_operation_id = OperationId::default();

    // Should not panic — the state manager silently drops the update for
    // missing operations (logs a warning internally).
    let result = worker_scheduler
        .update_action(
            &bogus_worker_id,
            &bogus_operation_id,
            nativelink_util::operation_state_manager::UpdateOperationType::UpdateWithActionStage(
                ActionStage::Executing,
            ),
        )
        .await;

    assert!(
        result.is_ok(),
        "Updating a missing operation should succeed (warn and drop), got: {:?}",
        result,
    );

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

    let nix_connection = NixDaemonConnectionPool::new_default("/dev/null/fake-socket".to_string());
    let (scheduler, _worker_scheduler) = NixScheduler::new(
        &NixProxySpec::default(),
        awaited_action_db,
        task_change_notify,
        MockInstantWrapped::default,
        ac_store,
        cas_store,
        nix_connection,
        Arc::new(RunnerInfo::dummy()),
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
