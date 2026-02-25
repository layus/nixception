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

use std::collections::{BTreeMap, BTreeSet};
use std::iter::once;
use std::path::PathBuf;
use std::sync::Arc;

use nix_compat::derivation::{Derivation, Output};
use nix_compat::nixhash::CAHash;
use nix_compat::store_path::StorePath;

use bstr::BString;
use nativelink_error::{Code, Error, ResultExt, make_err};
use nativelink_store::ac_utils::get_and_decode_digest;
use nativelink_store::nix_store::{NixStore, key_to_store_path};
use nativelink_util::action_messages::{ActionInfo, ActionStage, OperationId, WorkerId};
use nativelink_util::common::DigestInfo;
use nativelink_util::operation_state_manager::UpdateOperationType;
use nativelink_util::store_trait::{Store, StoreKey, StoreLike};
use tokio::time;
use tracing::{Level, event};

use nativelink_proto::build::bazel::remote::execution::v2::{
    Command as ProtoCommand, Directory as ProtoDirectory,
};

use crate::worker_scheduler::WorkerScheduler;

#[derive(Clone, Debug)]
struct PathEntry {
    path: PathBuf,
    store_path: StorePath<String>,
}

/// A worker that executes a single action end-to-end, reporting every state
/// transition back to the scheduler through the [`WorkerStateManager`].
///
/// Created by the scheduler in [`NixScheduler::create_running_action`] and
/// then spawned via [`NixWorker::run`].
pub(crate) struct NixWorker {
    /// The state manager shared with the scheduler, used to update action state.
    worker_scheduler: Arc<dyn WorkerScheduler>,
    /// A synthetic worker id assigned to this nix worker instance.
    worker_id: WorkerId,
    /// The operation this worker is executing (assigned by the db).
    operation_id: OperationId,
    /// CAS store used to fetch action inputs and upload derivations.
    cas_store: Store,
    /// The action metadata describing what to execute.
    action_info: Arc<ActionInfo>,
}

impl NixWorker {
    pub(crate) fn new(
        worker_scheduler: Arc<dyn WorkerScheduler>,
        worker_id: WorkerId,
        operation_id: OperationId,
        cas_store: Store,
        action_info: Arc<ActionInfo>,
    ) -> Self {
        Self {
            worker_scheduler,
            worker_id,
            operation_id,
            cas_store,
            action_info,
        }
    }

    // ----- state-transition helpers -----

    /// Send a state update, returning an error if the operation is no longer
    /// in the db (e.g. it was already timed-out or cancelled).
    async fn try_update(&self, update: UpdateOperationType) -> Result<(), Error> {
        self.worker_scheduler
            .update_action(&self.worker_id, &self.operation_id, update)
            .await
    }

    /// Fire-and-forget update: sends the update and logs any error internally.
    async fn update(&self, update: UpdateOperationType) {
        if let Err(e) = self.try_update(update).await {
            event!(
                Level::ERROR,
                error = ?e,
                operation_id = ?self.operation_id,
                "Failed to update action state"
            );
        }
    }

    // ----- execution entry-point -----

    /// Execute the action from start to finish.
    ///
    ///  1. Fetch the command and input tree from the CAS.
    ///  2. Build a Nix derivation from the inputs.
    ///  3. Upload the derivation to the Nix store → transition to `Executing`.
    ///  4. TODO: build the derivation and collect outputs.
    ///  5. Mark the action as completed.
    ///
    /// The action's timeout (from [`ActionInfo::timeout`]) is enforced with
    /// [`tokio::time::timeout`].  On timeout the action is marked as
    /// completed with exit code 124 (the standard timeout exit code).
    ///
    /// All outcomes (success, error, timeout) are communicated exclusively
    /// via the [`WorkerStateManager`] — this method returns nothing.
    pub(crate) async fn run(self) {
        let timeout_duration = self.action_info.timeout;
        match time::timeout(timeout_duration, self.run_inner()).await {
            Ok(()) => { /* run_inner handled all state transitions */ }
            Err(_elapsed) => {
                event!(
                    Level::WARN,
                    operation_id = ?self.operation_id,
                    ?timeout_duration,
                    "Action timed out"
                );
                let timeout_result = nativelink_util::action_messages::ActionResult {
                    exit_code: 124,
                    ..Default::default()
                };
                self.update(UpdateOperationType::UpdateWithActionStage(
                    ActionStage::Completed(timeout_result),
                ))
                .await;
            }
        }
    }

    /// The actual execution logic, called inside a timeout wrapper.
    async fn run_inner(&self) {
        let result = self.prepare_derivation().await;
        let drv_path = match result {
            Ok(path) => path,
            Err(e) => {
                self.update(UpdateOperationType::UpdateWithError(e)).await;
                return;
            }
        };

        // The derivation has been uploaded — transition Queued → Executing.
        event!(
            Level::INFO,
            drv_path = ?drv_path.to_absolute_path(),
            operation_id = ?self.operation_id,
            "Derivation uploaded, marking action as Executing"
        );
        self.update(UpdateOperationType::UpdateWithActionStage(
            ActionStage::Executing,
        ))
        .await;

        // Phase 2 — TODO: build the derivation / wait for the build to
        // finish and collect outputs.  This is not yet implemented; for now
        // we immediately mark the action as completed.
        self.update(UpdateOperationType::UpdateWithActionStage(
            ActionStage::Completed(nativelink_util::action_messages::ActionResult {
                exit_code: 0,
                ..Default::default()
            }),
        ))
        .await;
    }

    // ----- derivation helpers -----

    /// Prepare and upload a Nix derivation for this action:
    ///  1. Fetch the command and input tree from the CAS
    ///  2. Build a Nix derivation from the inputs
    ///  3. Upload the derivation to the Nix store
    ///
    /// Returns the derivation store path on success.
    async fn prepare_derivation(&self) -> Result<StorePath<String>, Error> {
        let command = get_and_decode_digest::<ProtoCommand>(
            &self.cas_store,
            self.action_info.command_digest.into(),
        )
        .await
        .err_tip(|| "Converting command_digest to Command")?;

        let mut entries: Vec<PathEntry> = Vec::new();
        self.unfold(
            "./".into(),
            &self.action_info.input_root_digest,
            &mut entries,
        )
        .await
        .err_tip(|| "Converting digest to Directory")?;

        let script = entries
            .iter()
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
            ("script".into(), BString::new(script.as_bytes().to_vec())),
            ("out".into(), BString::default()),
        ]);
        let builder_path: StorePath<&str> = StorePath::from_absolute_path_full(
            "/nix/store/03r3ipfa69l8nla101khyw3g67j24357-bash-5.3p9.drv",
        )
        .map_err(|e| make_err!(Code::Internal, "Failed to parse store path: {e}"))?
        .0;
        let builder_hash: [u8; 32] = [
            148, 82, 165, 89, 140, 175, 83, 34, 16, 61, 21, 144, 125, 83, 22, 252, 171, 33, 140,
            108, 140, 218, 28, 83, 107, 67, 174, 18, 43, 166, 173, 163,
        ];
        let builder_info = self
            .cas_store
            .downcast_ref::<NixStore>(None)
            .unwrap()
            .as_pin()
            .query_path_info("/nix/store/f15k3dpilmiyv6zgpib289rnjykgr1r4-bash-5.3p9")?
            .ok_or(make_err!(Code::Internal, "Could not query path info"))?;
        let builder_deriver = StorePath::from_absolute_path(&builder_info.deriver.0.0)
            .map_err(|e| make_err!(Code::Internal, "Failed to parse store path: {e}"))?;
        let mut derivation: Derivation = Derivation {
            arguments: vec!["-ec".into(), "eval \"$script\"".into()],
            builder: "/nix/store/f15k3dpilmiyv6zgpib289rnjykgr1r4-bash-5.3p9/bin/bash".into(),
            environment,
            input_derivations: BTreeMap::from([(builder_deriver, BTreeSet::from(["out".into()]))]),
            input_sources: entries.iter().map(|e| e.store_path.to_owned()).collect(),
            outputs,
            system: "x86_64-linux".into(),
        };

        let hash_modulo = derivation.hash_derivation_modulo(|a| {
            assert_eq!(a, &builder_path);
            builder_hash
        });
        derivation
            .calculate_output_paths("reapi-action", &hash_modulo)
            .map_err(|e| make_err!(Code::Internal, "Failed to calculate output paths: {}", e))?;

        let drv_path = derivation
            .calculate_derivation_path("reapi-action")
            .map_err(|e| make_err!(Code::Internal, "Failed to calculate derivation path: {}", e))?;

        let serialized_derivation = derivation.to_aterm_bytes();
        let references: Vec<StorePath<String>> = derivation
            .input_sources
            .into_iter()
            .chain(derivation.input_derivations.into_keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        let path_info = self
            .cas_store
            .downcast_ref::<NixStore>(None)
            .unwrap()
            .as_pin()
            .add_to_store(
                CAHash::Text([0; 32]).to_nix_nixbase32_string(),
                &serialized_derivation,
                "reapi-action.drv",
                &references,
            )
            .await?;

        assert_eq!(&path_info.path.0.0, &drv_path.to_absolute_path());
        Ok(drv_path)
    }

    /// Recursively walk a CAS directory tree and collect every file as a
    /// [`PathEntry`] (relative path + Nix store path).
    async fn unfold<'a>(
        &self,
        root: PathBuf,
        digest: &DigestInfo,
        entries: &'a mut Vec<PathEntry>,
    ) -> Result<&'a mut Vec<PathEntry>, Error> {
        let directory =
            get_and_decode_digest::<ProtoDirectory>(&self.cas_store, digest.into()).await?;

        for file in directory.files {
            let digest: DigestInfo = file
                .digest
                .err_tip(|| "Expected Digest to exist in Directory::directories::digest")?
                .try_into()
                .err_tip(|| "In Directory::file::digest")?;
            entries.push(PathEntry {
                path: root.join(file.name),
                store_path: key_to_store_path(&StoreKey::Digest(digest))?,
            });
        }

        for dir in directory.directories {
            let digest: DigestInfo = dir
                .digest
                .err_tip(|| "Expected Digest to exist in Directory::directories::digest")?
                .try_into()
                .err_tip(|| "In Directory::file::digest")?;

            Box::pin(self.unfold(root.join(dir.name), &digest, entries)).await?;
        }

        Ok(entries)
    }
}
