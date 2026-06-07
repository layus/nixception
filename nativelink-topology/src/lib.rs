// Copyright 2024 The NativeLink Authors. All rights reserved.
//
// Licensed under the Functional Source License, Version 1.1, Apache 2.0 Future License (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    See LICENSE file for details
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! A declarative DSL for constructing nativelink store and scheduler
//! topologies.
//!
//! The [`topology!`] macro expands directly to concrete constructor calls
//! (`NixStore::new`, `NoopStore::new`, `NixScheduler::new`, …) rather than
//! routing through the runtime `store_factory` / `scheduler_factory`
//! dispatch on `StoreSpec` / `SchedulerSpec`.  Because only the store and
//! scheduler types actually referenced at a call site are ever named, the
//! linker (with LTO) can drop every backend the binary does not use — the
//! same effect achieved by hand-writing the construction code, but with a
//! readable, config-like surface syntax.
//!
//! # Usage
//!
//! The macro must be invoked inside an `async` context whose function
//! returns a `Result<_, E>` where `E: From<nativelink_error::Error>` (the
//! async store constructors use `.await?`):
//!
//! ```ignore
//! use nativelink_topology::topology;
//!
//! let (store_manager, action_schedulers, worker_schedulers) = topology! {
//!     stores {
//!         VOID      = Noop,
//!         NIX_STORE = Nix { socket_path: None },
//!     }
//!     schedulers {
//!         NIX_SCHEDULER = NixProxy { ac_store: VOID, cas_store: NIX_STORE },
//!     }
//! };
//! ```
//!
//! Each store binding becomes a typed `let` handle (so, e.g., the
//! `NixProxy` scheduler can read `cas_store.socket_path()` without a
//! `downcast`) and is also registered in the returned [`StoreManager`]
//! under its identifier name. Referencing a store of the wrong type where a
//! concrete capability is required is therefore a compile error.
//!
//! [`StoreManager`]: nativelink_store::store_manager::StoreManager

/// Hermetic re-exports used by the [`topology!`] macro expansion.
///
/// This module is an implementation detail: it lets the macro reference
/// every type and function it needs via `$crate::__rt::…` so that call
/// sites require no imports beyond the macro itself. It is not covered by
/// semver guarantees.
#[doc(hidden)]
pub mod __rt {
    pub use std::collections::HashMap;
    pub use std::sync::Arc;
    pub use std::time::SystemTime;

    pub use nativelink_config::schedulers::NixProxySpec;
    pub use nativelink_config::stores::{FilesystemSpec, MemorySpec, NixSpec};
    pub use nativelink_scheduler::default_scheduler_factory::memory_awaited_action_db_factory;
    pub use nativelink_scheduler::nix_scheduler::NixScheduler;
    pub use nativelink_scheduler::runner_info::RunnerInfo;
    pub use nativelink_scheduler::worker_scheduler::WorkerScheduler;
    pub use nativelink_store::filesystem_store::FilesystemStore;
    pub use nativelink_store::memory_store::MemoryStore;
    pub use nativelink_store::nix_daemon_connection::NixDaemonConnectionPool;
    pub use nativelink_store::nix_store::NixStore;
    pub use nativelink_store::noop_store::NoopStore;
    pub use nativelink_store::store_manager::StoreManager;
    pub use nativelink_util::operation_state_manager::ClientStateManager;
    pub use nativelink_util::store_trait::Store;
    pub use tokio::sync::Notify;
}

/// Construct a nativelink store and scheduler topology.
///
/// See the [crate-level documentation](crate) for the full surface syntax,
/// invocation requirements, and rationale.
///
/// Expands to a block evaluating to the tuple
/// `(Arc<StoreManager>, HashMap<String, Arc<dyn ClientStateManager>>,
/// HashMap<String, Arc<dyn WorkerScheduler>>)`.
#[macro_export]
macro_rules! topology {
    // ── Entry point ────────────────────────────────────────────────────
    (
        stores { $( $sname:ident = $skw:ident $({ $($sf:tt)* })? ),* $(,)? }
        schedulers { $( $schname:ident = $schkw:ident { $($schf:tt)* } ),* $(,)? }
    ) => {{
        let store_manager = $crate::__rt::Arc::new($crate::__rt::StoreManager::new());
        let mut action_schedulers: $crate::__rt::HashMap<
            ::std::string::String,
            $crate::__rt::Arc<dyn $crate::__rt::ClientStateManager>,
        > = $crate::__rt::HashMap::new();
        let mut worker_schedulers: $crate::__rt::HashMap<
            ::std::string::String,
            $crate::__rt::Arc<dyn $crate::__rt::WorkerScheduler>,
        > = $crate::__rt::HashMap::new();

        // Stores: each becomes a typed `let` handle and a named registration.
        $(
            let $sname = $crate::topology!(@store $skw $({ $($sf)* })?);
            store_manager.add_store(
                stringify!($sname),
                $crate::__rt::Store::new($sname.clone()),
            );
        )*

        // Schedulers: inserted into both the action and worker maps.
        $(
            $crate::topology!(@scheduler
                name = $schname,
                action = action_schedulers,
                worker = worker_schedulers,
                kw = $schkw { $($schf)* }
            );
        )*

        (store_manager, action_schedulers, worker_schedulers)
    }};

    // ── Store constructors ─────────────────────────────────────────────
    // Add a new arm here for each store backend the DSL should support.
    // Only the arms actually referenced by a call site are expanded, so
    // unused backends stay unreferenced (and droppable by LTO).
    (@store Noop) => {
        $crate::__rt::NoopStore::new()
    };
    (@store Memory { $($f:tt)* }) => {
        $crate::__rt::MemoryStore::new(&$crate::__rt::MemorySpec { $($f)* })
    };
    (@store Nix { $($f:tt)* }) => {
        $crate::__rt::NixStore::new(&$crate::__rt::NixSpec { $($f)* }).await?
    };
    (@store Filesystem { $($f:tt)* }) => {
        $crate::__rt::FilesystemStore::new(&$crate::__rt::FilesystemSpec { $($f)* }).await?
    };

    // ── Scheduler constructors ─────────────────────────────────────────
    (@scheduler
        name = $name:ident,
        action = $action:ident,
        worker = $worker:ident,
        kw = NixProxy { ac_store: $ac:ident, cas_store: $cas:ident $(,)? }
    ) => {{
        let task_change_notify = $crate::__rt::Arc::new($crate::__rt::Notify::new());
        let awaited_action_db = $crate::__rt::memory_awaited_action_db_factory(
            0,
            &task_change_notify,
            $crate::__rt::SystemTime::now,
        );
        // `cas_store` is a typed handle, so the daemon socket path is read
        // directly — no `downcast_ref::<NixStore>` needed.
        let socket_path = $cas.socket_path().to_string();
        let nix_connection = $crate::__rt::NixDaemonConnectionPool::new_default(socket_path);
        let runner_info = $crate::__rt::Arc::new($crate::__rt::RunnerInfo::from_env()?);
        let nix_proxy_spec = $crate::__rt::NixProxySpec {
            ac_store: stringify!($ac).to_string(),
            cas_store: stringify!($cas).to_string(),
        };
        let (action_scheduler, worker_scheduler) = $crate::__rt::NixScheduler::new(
            &nix_proxy_spec,
            awaited_action_db,
            task_change_notify,
            $crate::__rt::SystemTime::now,
            $crate::__rt::Store::new($ac.clone()),
            $crate::__rt::Store::new($cas.clone()),
            nix_connection,
            runner_info,
        );
        let action_scheduler: $crate::__rt::Arc<dyn $crate::__rt::ClientStateManager> =
            action_scheduler;
        $action.insert(stringify!($name).to_string(), action_scheduler);
        $worker.insert(stringify!($name).to_string(), worker_scheduler);
    }};
}
