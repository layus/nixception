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
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nix_compat::derivation::{Derivation, Output};
use nix_compat::nixhash::CAHash;
use nix_compat::store_path::StorePath;

use crate::runner_info::RunnerInfo;

use bstr::BString;
use bytes::Bytes;
use nativelink_error::{Code, Error, ResultExt, make_err};
use nativelink_store::ac_utils::get_and_decode_digest;
use nativelink_store::nix_store::{NixStore, key_to_store_path};
use nativelink_util::action_messages::{
    ActionInfo, ActionStage, FileInfo, NameOrPath, OperationId, WorkerId,
};
use nativelink_util::common::DigestInfo;
use nativelink_util::digest_hasher::DigestHasher;
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
    /// Runner metadata for constructing action derivations.
    runner_info: Arc<RunnerInfo>,
}

impl NixWorker {
    pub(crate) fn new(
        worker_scheduler: Arc<dyn WorkerScheduler>,
        worker_id: WorkerId,
        operation_id: OperationId,
        cas_store: Store,
        action_info: Arc<ActionInfo>,
        runner_info: Arc<RunnerInfo>,
    ) -> Self {
        Self {
            worker_scheduler,
            worker_id,
            operation_id,
            cas_store,
            action_info,
            runner_info,
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
        let result = time::timeout(timeout_duration, self.run_inner()).await;

        let stage = match result {
            Ok(Ok(())) => return, // run_inner already sent the Completed update
            Ok(Err(e)) => {
                event!(
                    Level::ERROR,
                    error = ?e,
                    operation_id = ?self.operation_id,
                    "Action failed"
                );
                ActionStage::Completed(nativelink_util::action_messages::ActionResult {
                    error: Some(e),
                    ..Default::default()
                })
            }
            Err(_elapsed) => {
                event!(
                    Level::WARN,
                    operation_id = ?self.operation_id,
                    ?timeout_duration,
                    "Action timed out"
                );
                ActionStage::Completed(nativelink_util::action_messages::ActionResult {
                    exit_code: 124,
                    ..Default::default()
                })
            }
        };

        self.update(UpdateOperationType::UpdateWithActionStage(stage))
            .await;
    }

    /// The actual execution logic, called inside a timeout wrapper.
    ///
    /// On success the action is marked as [`ActionStage::Completed`] before
    /// returning `Ok(())`.  On error an `Err` is returned and the caller
    /// (`run`) is responsible for reporting the failure.
    async fn run_inner(&self) -> Result<(), Error> {
        let (drv_path, out_path) = self.prepare_derivation().await?;

        // The derivation has been uploaded — transition Queued → Executing.
        event!(
            Level::INFO,
            drv_path = ?drv_path.to_absolute_path(),
            out_path = ?out_path,
            "Derivation uploaded, marking action as Executing"
        );
        self.update(UpdateOperationType::UpdateWithActionStage(
            ActionStage::Executing,
        ))
        .await;

        self.execute_derivation(&drv_path).await?;

        let action_result = self.collect_action_result(Path::new(&out_path)).await?;

        self.try_update(UpdateOperationType::UpdateWithActionStage(
            ActionStage::Completed(action_result),
        ))
        .await?;

        Ok(())
    }

    /// Build the derivation by sending it to the nix daemon and waiting
    /// for completion.
    async fn execute_derivation(&self, drv_path: &StorePath<String>) -> Result<(), Error> {
        let drv_abs_path = drv_path.to_absolute_path();
        let nix_store = self
            .cas_store
            .downcast_ref::<NixStore>(None)
            .ok_or_else(|| make_err!(Code::Internal, "CAS store is not a NixStore"))?;

        let build_results = nix_store
            .build_derivation(&drv_abs_path)
            .err_tip(|| format!("Building derivation {}", drv_abs_path))?;

        event!(
            Level::INFO,
            drv_path = ?drv_abs_path,
            results = ?build_results,
            "Build completed"
        );

        Ok(())
    }

    /// Read the exit code, upload stdout/stderr, walk the outputs
    /// sub-directory, and return a populated [`ActionResult`].
    ///
    /// `out_dir` is the absolute path of the derivation's "out" output
    /// (e.g. `/nix/store/xxx-reapi-action`), which is expected to
    /// contain `exitcode`, `stdout`, `stderr` files and an `outputs/`
    /// sub-directory.
    async fn collect_action_result(
        &self,
        out_dir: &Path,
    ) -> Result<nativelink_util::action_messages::ActionResult, Error> {
        // Read the exit code produced by the script.
        let exit_code: i32 = tokio::fs::read_to_string(out_dir.join("exitcode"))
            .await
            .map_err(|e| make_err!(Code::Internal, "Failed to read exitcode: {}", e))?
            .trim()
            .parse::<i32>()
            .map_err(|e| make_err!(Code::Internal, "Failed to parse exitcode: {}", e))?;

        // Upload stdout and stderr to the CAS and obtain their digests.
        let stdout_digest = self
            .upload_file_to_cas(&out_dir.join("stdout"))
            .await
            .err_tip(|| "Uploading stdout to CAS")?;
        let stderr_digest = self
            .upload_file_to_cas(&out_dir.join("stderr"))
            .await
            .err_tip(|| "Uploading stderr to CAS")?;

        // Walk $out/outputs/ and collect FileInfo entries.
        let outputs_dir = out_dir.join("outputs");
        let output_files = self
            .collect_output_files(&outputs_dir)
            .await
            .err_tip(|| format!("Collecting output files from {}", outputs_dir.display()))?;

        event!(
            Level::INFO,
            num_output_files = output_files.len(),
            exit_code,
            "Output files collected"
        );

        Ok(nativelink_util::action_messages::ActionResult {
            output_files,
            exit_code,
            stdout_digest,
            stderr_digest,
            message: out_dir.to_string_lossy().to_string(),
            ..Default::default()
        })
    }

    // ----- derivation helpers -----

    /// Prepare and upload a Nix derivation for this action:
    ///  1. Fetch the command and input tree from the CAS
    ///  2. Build a Nix derivation from the inputs
    ///  3. Upload the derivation to the Nix store
    ///
    /// Returns a tuple of (derivation store path, output store path) on
    /// success.
    async fn prepare_derivation(&self) -> Result<(StorePath<String>, String), Error> {
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
                    concat!(
                        "mkdir -p $(dirname {path})\n",
                        // "ln {store_path} {path} || ",
                        "cp {store_path} {path} --no-preserve=all",
                    ),
                    path = e.path.to_string_lossy(),
                    store_path = e.store_path.to_absolute_path()
                )
            })
            .chain(once(format!("tree"))) // for debugging: print the input tree
            // From now on, we only deal with outputs, and these are relative to
            // the working directory, so `cd` into it first.
            .chain(once(format!(
                "mkdir -p {cwd} && cd {cwd}",
                cwd = command.working_directory
            )))
            // Create output directory structure before running the command
            // so that stdout/stderr redirections have a target.
            .chain(
                command
                    .output_directories
                    .clone()
                    .into_iter()
                    .map(|dir| format!("mkdir -p {dir}")),
            )
            .chain(
                command
                    .output_files
                    .clone()
                    .into_iter()
                    .map(|file| format!("mkdir -p $(dirname {file})")),
            )
            .chain(
                command
                    .output_paths
                    .clone()
                    .into_iter()
                    .map(|path| format!("mkdir -p $(dirname {path})")),
            )
            // Create $out for stdout, stderr and exitcode.
            // Prepare $out/outputs for the actual outputs extracted later.
            .chain(once("mkdir -p $out/outputs".into()))
            .chain(once(format!("("))) // Start a subshell so that `export` commands do not affect the rest of the script (`tree` in particular).
            .chain(command.environment_variables.into_iter().map(|var| {
                format!(
                    "export {name}='{value}'",
                    name = var.name,
                    value = var.value
                )
            }))
            // Run the actual command, capturing stdout and stderr.
            .chain(once(format!(
                concat!(
                    "{cmd} >$out/stdout 2>$out/stderr\n",
                    "echo $? >$out/exitcode",
                ),
                cmd = command.arguments.join(" "),
            )))
            .chain(once(format!(")"))) // End of subshell.
            .chain(once(format!("tree"))) // for debugging: print the output tree after execution
            // Copy outputs into $out/outputs/. Use || true so that
            // missing outputs do not cause the nix build to fail — the
            // real exit code is already saved in $out/exitcode.
            .chain(
                command
                    .output_directories
                    .into_iter()
                    .map(|dir| format!("cp --parents -r {dir} $out/outputs || true")),
            )
            .chain(
                command
                    .output_files
                    .into_iter()
                    .map(|file| format!("cp --parents {file} $out/outputs || true")),
            )
            .chain(
                command
                    .output_paths
                    .into_iter()
                    .map(|path| format!("cp --parents {path} $out/outputs || true")),
            )
            .chain(once(format!("tree $out"))) // for debugging: print the final output tree
            .collect::<Vec<_>>()
            .join("\n");

        let outputs = BTreeMap::from([("out".to_string(), Output::default())]);
        let environment = BTreeMap::from([
            ("script".into(), BString::new(script.as_bytes().to_vec())),
            ("out".into(), BString::default()),
        ]);

        let ri = &self.runner_info;

        // Use the runner .drv store path directly from RunnerInfo rather than
        // querying the Nix daemon's query_path_info (whose `deriver` field may
        // not always contain a valid absolute store path).
        let builder_deriver = ri.drv_store_path.clone();

        let mut derivation: Derivation = Derivation {
            arguments: vec!["set -x; eval \"$script\"".into()],
            builder: ri.builder_path.clone().into(),
            environment,
            input_derivations: BTreeMap::from([(builder_deriver, BTreeSet::from(["out".into()]))]),
            input_sources: entries.iter().map(|e| e.store_path.to_owned()).collect(),
            outputs,
            system: ri.system.clone().into(),
        };

        let hash_modulo = derivation.hash_derivation_modulo(|input_drv_path| {
            // The only input derivation is the runner.
            assert_eq!(
                input_drv_path.to_absolute_path(),
                ri.drv_store_path.to_absolute_path(),
                "Unexpected input derivation"
            );
            ri.hash_derivation_modulo
        });
        derivation
            .calculate_output_paths("reapi-action", &hash_modulo)
            .map_err(|e| make_err!(Code::Internal, "Failed to calculate output paths: {}", e))?;

        let out_path = derivation
            .outputs
            .get("out")
            .and_then(|o| o.path.as_ref())
            .map(|sp| sp.to_absolute_path())
            .ok_or_else(|| {
                make_err!(
                    Code::Internal,
                    "Derivation has no 'out' output path after calculate_output_paths"
                )
            })?;

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
        Ok((drv_path, out_path))
    }

    /// Read a file from disk, upload it to the CAS store, and return its
    /// digest.
    async fn upload_file_to_cas(&self, path: &Path) -> Result<DigestInfo, Error> {
        let content = tokio::fs::read(path)
            .await
            .map_err(|e| make_err!(Code::Internal, "Failed to read {}: {}", path.display(), e))?;

        let digest_function = self.action_info.unique_qualifier.digest_function();
        let mut hasher = digest_function.hasher();
        hasher.update(&content);
        let digest = hasher.finalize_digest();

        self.cas_store
            .update_oneshot(StoreKey::Digest(digest), Bytes::from(content))
            .await
            .err_tip(|| format!("Uploading {} to CAS", path.display()))?;

        Ok(digest)
    }

    /// Walk the Nix output directory, upload every regular file to the CAS
    /// store, and return a [`FileInfo`] for each one.
    ///
    /// `out_dir` is the absolute path of the derivation's outputs
    /// sub-directory (e.g. `/nix/store/xxx-reapi-action/outputs`).
    async fn collect_output_files(&self, out_dir: &Path) -> Result<Vec<FileInfo>, Error> {
        let mut files: Vec<FileInfo> = Vec::new();
        self.walk_and_upload(out_dir, out_dir, &mut files).await?;
        Ok(files)
    }

    /// Recursively walk `current_dir`, uploading each regular file to the
    /// CAS and appending a [`FileInfo`] to `files`.  Paths in the
    /// resulting `FileInfo` entries are relative to `root_dir`.
    async fn walk_and_upload(
        &self,
        root_dir: &Path,
        current_dir: &Path,
        files: &mut Vec<FileInfo>,
    ) -> Result<(), Error> {
        let mut read_dir = tokio::fs::read_dir(current_dir).await.map_err(|e| {
            make_err!(
                Code::Internal,
                "Failed to read directory {}: {}",
                current_dir.display(),
                e
            )
        })?;

        while let Some(entry) = read_dir.next_entry().await.map_err(|e| {
            make_err!(
                Code::Internal,
                "Failed to read dir entry in {}: {}",
                current_dir.display(),
                e
            )
        })? {
            let file_type = entry.file_type().await.map_err(|e| {
                make_err!(
                    Code::Internal,
                    "Failed to get file type for {:?}: {}",
                    entry.path(),
                    e
                )
            })?;

            let path = entry.path();

            if file_type.is_dir() {
                Box::pin(self.walk_and_upload(root_dir, &path, files)).await?;
            } else if file_type.is_file() {
                let relative_path = path
                    .strip_prefix(root_dir)
                    .map_err(|e| {
                        make_err!(
                            Code::Internal,
                            "Failed to strip prefix {} from {}: {}",
                            root_dir.display(),
                            path.display(),
                            e
                        )
                    })?
                    .to_string_lossy()
                    .into_owned();

                let digest = self
                    .upload_file_to_cas(&path)
                    .await
                    .err_tip(|| format!("Uploading output file {} to CAS", relative_path))?;

                let metadata = entry.metadata().await.map_err(|e| {
                    make_err!(
                        Code::Internal,
                        "Failed to read metadata for {}: {}",
                        path.display(),
                        e
                    )
                })?;
                let is_executable = metadata.permissions().mode() & 0o111 != 0;

                files.push(FileInfo {
                    name_or_path: NameOrPath::Path(relative_path),
                    digest,
                    is_executable,
                });
            }
            // Symlinks and other special files are silently skipped.
        }

        Ok(())
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
