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
//! (`NixStore::new`, `NoopStore::new`, the per-variant scheduler factories,
//! …) rather than routing through the runtime `store_factory` /
//! `scheduler_factory` dispatch on `StoreSpec` / `SchedulerSpec`.  Because
//! only the store and scheduler types actually referenced at a call site
//! are ever named, the linker (with LTO) can drop every backend the binary
//! does not use — the same effect achieved by hand-writing the construction
//! code, but with a readable, config-like surface syntax.
//!
//! # Usage
//!
//! The macro must be invoked inside an `async` context whose function
//! returns a `Result<_, E>` where `E: From<nativelink_error::Error>` (some
//! store constructors and all the scheduler factories use `.await?` / `?`):
//!
//! ```ignore
//! use nativelink_topology::topology;
//!
//! let (store_manager, action_schedulers, worker_schedulers) = topology! {
//!     stores {
//!         void      = Noop,
//!         nix_store = Nix { socket_path: None },
//!     }
//!     schedulers {
//!         nix_scheduler = NixProxy { ac_store: void, cas_store: nix_store },
//!     }
//! };
//! ```
//!
//! ## Store nesting
//!
//! Each store binding is registered in the returned [`StoreManager`] under
//! its identifier name.  Wrapper stores (`FastSlow`, `Verify`, `Dedup`, …)
//! refer to their child stores **by name**, so a child must be declared as
//! its own binding earlier in the `stores { … }` block:
//!
//! ```ignore
//! stores {
//!     fast_fs   = Filesystem { content_path: "…", temp_path: "…", eviction_policy: None },
//!     nix_store = Nix { socket_path: None },
//!     cache     = FastSlow { fast: fast_fs, slow: nix_store,
//!                            fast_direction: Default::default(),
//!                            slow_direction: Default::default() },
//! }
//! ```
//!
//! For wrapper stores the child-store fields are written with a store name;
//! every *other* field of the underlying spec must be supplied verbatim
//! (the macro does not guess defaults).
//!
//! [`StoreManager`]: nativelink_store::store_manager::StoreManager

use nativelink_config::schedulers::{SchedulerSpec, SimpleSpec};
use nativelink_config::stores::{NoopSpec, StoreSpec};

/// Placeholder store spec used to fill the (otherwise unused) child-store
/// fields of a wrapper spec when the child stores are passed to the
/// constructor directly.  The wrapper constructors ignore these fields.
#[doc(hidden)]
#[must_use]
pub fn placeholder_store_spec() -> StoreSpec {
    StoreSpec::Noop(NoopSpec {})
}

/// Placeholder scheduler spec used to fill the (otherwise unused) nested
/// `scheduler` field of a wrapper scheduler spec when the nested scheduler
/// is built separately and passed to the factory directly.
#[doc(hidden)]
#[must_use]
pub fn placeholder_sched_spec() -> SchedulerSpec {
    SchedulerSpec::Simple(SimpleSpec::default())
}

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

    pub use nativelink_config::schedulers::{
        CacheLookupSpec, GrpcSpec as SchedGrpcSpec, NixProxySpec, PropertyModifierSpec, SimpleSpec,
    };
    pub use nativelink_config::stores::{
        CompressionSpec, DedupSpec, ExistenceCacheSpec, ExperimentalAwsSpec, ExperimentalGcsSpec,
        ExperimentalMongoSpec, ExperimentalOntapS3Spec, FastSlowSpec, FilesystemSpec,
        GrpcSpec as StoreGrpcSpec, MemorySpec, NixSpec, OntapS3ExistenceCacheSpec, RedisSpec,
        RefSpec, ShardConfig, ShardSpec, SizePartitioningSpec, VerifySpec,
    };
    pub use nativelink_scheduler::default_scheduler_factory::{
        cache_lookup_scheduler_factory, grpc_scheduler_factory, nix_scheduler_factory,
        property_modifier_scheduler_factory, simple_scheduler_factory,
    };
    pub use nativelink_scheduler::worker_scheduler::WorkerScheduler;
    pub use nativelink_store::completeness_checking_store::CompletenessCheckingStore;
    pub use nativelink_store::compression_store::CompressionStore;
    pub use nativelink_store::dedup_store::DedupStore;
    pub use nativelink_store::existence_cache_store::ExistenceCacheStore;
    pub use nativelink_store::fast_slow_store::FastSlowStore;
    pub use nativelink_store::filesystem_store::FilesystemStore;
    pub use nativelink_store::gcs_store::GcsStore;
    pub use nativelink_store::grpc_store::GrpcStore;
    pub use nativelink_store::memory_store::MemoryStore;
    pub use nativelink_store::mongo_store::ExperimentalMongoStore;
    pub use nativelink_store::nix_store::NixStore;
    pub use nativelink_store::noop_store::NoopStore;
    pub use nativelink_store::ontap_s3_existence_cache_store::OntapS3ExistenceCache;
    pub use nativelink_store::ontap_s3_store::OntapS3Store;
    pub use nativelink_store::redis_store::RedisStore;
    pub use nativelink_store::ref_store::RefStore;
    pub use nativelink_store::s3_store::S3Store;
    pub use nativelink_store::shard_store::ShardStore;
    pub use nativelink_store::size_partitioning_store::SizePartitioningStore;
    pub use nativelink_store::store_manager::StoreManager;
    pub use nativelink_store::verify_store::VerifyStore;
    pub use nativelink_util::operation_state_manager::ClientStateManager;
    pub use nativelink_util::store_trait::Store;
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

        // Stores: each becomes a named `Store` binding usable by later
        // (wrapper) stores and registered in the store manager.
        $(
            let $sname: $crate::__rt::Store =
                $crate::topology!(@store store_manager, $skw $({ $($sf)* })?);
            store_manager.add_store(stringify!($sname), $sname.clone());
        )*

        // Schedulers: delegate to the shared per-variant leaf factories and
        // insert the results into the action / worker maps.
        $(
            let (__action, __worker) =
                $crate::topology!(@scheduler store_manager, $schkw { $($schf)* });
            if let Some(__a) = __action {
                let __a: $crate::__rt::Arc<dyn $crate::__rt::ClientStateManager> = __a;
                action_schedulers.insert(stringify!($schname).to_string(), __a);
            }
            if let Some(__w) = __worker {
                worker_schedulers.insert(stringify!($schname).to_string(), __w);
            }
        )*

        (store_manager, action_schedulers, worker_schedulers)
    }};

    // ── Leaf stores ────────────────────────────────────────────────────
    // The `{ … }` fields are forwarded verbatim into the underlying spec,
    // so the caller writes valid spec fields directly.
    (@store $sm:ident, Noop) => {
        $crate::__rt::Store::new($crate::__rt::NoopStore::new())
    };
    (@store $sm:ident, Memory { $($f:tt)* }) => {
        $crate::__rt::Store::new($crate::__rt::MemoryStore::new(&$crate::__rt::MemorySpec { $($f)* }))
    };
    (@store $sm:ident, Nix { $($f:tt)* }) => {
        $crate::__rt::Store::new($crate::__rt::NixStore::new(&$crate::__rt::NixSpec { $($f)* }).await?)
    };
    (@store $sm:ident, Filesystem { $($f:tt)* }) => {
        $crate::__rt::Store::new(
            <$crate::__rt::FilesystemStore>::new(&$crate::__rt::FilesystemSpec { $($f)* }).await?,
        )
    };
    (@store $sm:ident, Redis { $($f:tt)* }) => {
        $crate::__rt::Store::new($crate::__rt::RedisStore::new($crate::__rt::RedisSpec { $($f)* })?)
    };
    (@store $sm:ident, Grpc { $($f:tt)* }) => {
        $crate::__rt::Store::new($crate::__rt::GrpcStore::new(&$crate::__rt::StoreGrpcSpec { $($f)* }).await?)
    };
    (@store $sm:ident, Mongo { $($f:tt)* }) => {
        $crate::__rt::Store::new(
            $crate::__rt::ExperimentalMongoStore::new($crate::__rt::ExperimentalMongoSpec { $($f)* }).await?,
        )
    };
    (@store $sm:ident, Aws { $($f:tt)* }) => {
        $crate::__rt::Store::new(
            $crate::__rt::S3Store::new(&$crate::__rt::ExperimentalAwsSpec { $($f)* }, $crate::__rt::SystemTime::now).await?,
        )
    };
    (@store $sm:ident, Gcs { $($f:tt)* }) => {
        $crate::__rt::Store::new(
            $crate::__rt::GcsStore::new(&$crate::__rt::ExperimentalGcsSpec { $($f)* }, $crate::__rt::SystemTime::now).await?,
        )
    };
    (@store $sm:ident, OntapS3 { $($f:tt)* }) => {
        $crate::__rt::Store::new(
            $crate::__rt::OntapS3Store::new(&$crate::__rt::ExperimentalOntapS3Spec { $($f)* }, $crate::__rt::SystemTime::now).await?,
        )
    };
    (@store $sm:ident, OntapS3ExistenceCache { $($f:tt)* }) => {
        $crate::__rt::Store::new(
            $crate::__rt::OntapS3ExistenceCache::new(&$crate::__rt::OntapS3ExistenceCacheSpec { $($f)* }, $crate::__rt::SystemTime::now).await?,
        )
    };
    (@store $sm:ident, Ref { $($f:tt)* }) => {
        $crate::__rt::Store::new($crate::__rt::RefStore::new(
            &$crate::__rt::RefSpec { $($f)* },
            $crate::__rt::Arc::downgrade(&$sm),
        ))
    };

    // ── Wrapper stores ─────────────────────────────────────────────────
    // Child-store fields take the *name* of an earlier store binding; every
    // remaining spec field must be supplied verbatim after the children.
    (@store $sm:ident, Verify { backend: $b:ident $(, $($rest:tt)*)? }) => {
        $crate::__rt::Store::new($crate::__rt::VerifyStore::new(
            &$crate::__rt::VerifySpec { backend: $crate::placeholder_store_spec() $(, $($rest)*)? },
            $b.clone(),
        ))
    };
    (@store $sm:ident, Compression { backend: $b:ident $(, $($rest:tt)*)? }) => {
        $crate::__rt::Store::new($crate::__rt::CompressionStore::new(
            &$crate::__rt::CompressionSpec { backend: $crate::placeholder_store_spec() $(, $($rest)*)? },
            $b.clone(),
        )?)
    };
    (@store $sm:ident, ExistenceCache { backend: $b:ident $(, $($rest:tt)*)? }) => {
        $crate::__rt::Store::new($crate::__rt::ExistenceCacheStore::new(
            &$crate::__rt::ExistenceCacheSpec { backend: $crate::placeholder_store_spec() $(, $($rest)*)? },
            $b.clone(),
        ))
    };
    (@store $sm:ident, Dedup { index_store: $i:ident, content_store: $c:ident $(, $($rest:tt)*)? }) => {
        $crate::__rt::Store::new($crate::__rt::DedupStore::new(
            &$crate::__rt::DedupSpec {
                index_store: $crate::placeholder_store_spec(),
                content_store: $crate::placeholder_store_spec()
                $(, $($rest)*)?
            },
            $i.clone(),
            $c.clone(),
        )?)
    };
    (@store $sm:ident, CompletenessChecking { backend: $b:ident, cas_store: $c:ident $(,)? }) => {
        $crate::__rt::Store::new($crate::__rt::CompletenessCheckingStore::new($b.clone(), $c.clone()))
    };
    (@store $sm:ident, FastSlow { fast: $f:ident, slow: $s:ident $(, $($rest:tt)*)? }) => {
        $crate::__rt::Store::new($crate::__rt::FastSlowStore::new(
            &$crate::__rt::FastSlowSpec {
                fast: $crate::placeholder_store_spec(),
                slow: $crate::placeholder_store_spec()
                $(, $($rest)*)?
            },
            $f.clone(),
            $s.clone(),
        ))
    };
    (@store $sm:ident, SizePartitioning { lower_store: $l:ident, upper_store: $u:ident $(, $($rest:tt)*)? }) => {
        $crate::__rt::Store::new($crate::__rt::SizePartitioningStore::new(
            &$crate::__rt::SizePartitioningSpec {
                lower_store: $crate::placeholder_store_spec(),
                upper_store: $crate::placeholder_store_spec()
                $(, $($rest)*)?
            },
            $l.clone(),
            $u.clone(),
        ))
    };
    (@store $sm:ident, Shard { stores: [ $($s:ident),* $(,)? ] $(,)? }) => {
        $crate::__rt::Store::new($crate::__rt::ShardStore::new(
            &$crate::__rt::ShardSpec {
                stores: ::std::vec![
                    $( $crate::__rt::ShardConfig {
                        // `stringify!($s)` ties this element to the `$s`
                        // repetition (one placeholder per referenced store)
                        // without affecting the produced value.
                        store: {
                            let _: &str = stringify!($s);
                            $crate::placeholder_store_spec()
                        },
                        weight: None,
                    } ),*
                ],
            },
            ::std::vec![ $( $s.clone() ),* ],
        )?)
    };

    // ── Schedulers ─────────────────────────────────────────────────────
    // Every arm yields a `SchedulerFactoryResults`
    // (`(Option<Arc<dyn ClientStateManager>>, Option<Arc<dyn WorkerScheduler>>)`)
    // by delegating to the shared per-variant leaf factories.
    (@scheduler $sm:ident, Simple { $($f:tt)* }) => {
        $crate::__rt::simple_scheduler_factory(
            &$crate::__rt::SimpleSpec { $($f)* },
            &$sm,
            $crate::__rt::SystemTime::now,
            None,
        )?
    };
    (@scheduler $sm:ident, Grpc { $($f:tt)* }) => {
        $crate::__rt::grpc_scheduler_factory(&$crate::__rt::SchedGrpcSpec { $($f)* })?
    };
    (@scheduler $sm:ident, NixProxy { ac_store: $ac:ident, cas_store: $cas:ident $(,)? }) => {
        $crate::__rt::nix_scheduler_factory(
            &$crate::__rt::NixProxySpec {
                ac_store: stringify!($ac).to_string(),
                cas_store: stringify!($cas).to_string(),
            },
            &$sm,
            $crate::__rt::SystemTime::now,
        )?
    };
    (@scheduler $sm:ident, CacheLookup {
        ac_store: $ac:ident,
        scheduler: $inner_kw:ident { $($inner_f:tt)* }
        $(, $($rest:tt)*)?
    }) => {{
        let nested = $crate::topology!(@scheduler $sm, $inner_kw { $($inner_f)* });
        $crate::__rt::cache_lookup_scheduler_factory(
            &$crate::__rt::CacheLookupSpec {
                ac_store: stringify!($ac).to_string(),
                scheduler: ::std::boxed::Box::new($crate::placeholder_sched_spec())
                $(, $($rest)*)?
            },
            &$sm,
            nested,
        )?
    }};
    (@scheduler $sm:ident, PropertyModifier {
        scheduler: $inner_kw:ident { $($inner_f:tt)* }
        $(, $($rest:tt)*)?
    }) => {{
        let nested = $crate::topology!(@scheduler $sm, $inner_kw { $($inner_f)* });
        $crate::__rt::property_modifier_scheduler_factory(
            &$crate::__rt::PropertyModifierSpec {
                scheduler: ::std::boxed::Box::new($crate::placeholder_sched_spec())
                $(, $($rest)*)?
            },
            nested,
        )?
    }};
}
