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
//! store constructors and all the scheduler factories use `.await?` / `?`).
//! It evaluates to a [`Topology`] holding the store manager, the scheduler
//! maps, and the (optional) tonic [`Routes`]:
//!
//! ```ignore
//! use nativelink_topology::topology;
//!
//! let topology = topology! {
//!     stores {
//!         void      = Noop,
//!         nix_store = Nix { socket_path: None },
//!     }
//!     schedulers {
//!         nix_scheduler = NixProxy { ac_store: void, cas_store: nix_store },
//!     }
//!     services {
//!         cas:       Cas { cas_store: nix_store },
//!         execution: Execution { cas_store: nix_store, scheduler: nix_scheduler },
//!     }
//! };
//! let router = topology.routes.into_axum_router();
//! ```
//!
//! ## Services
//!
//! The `services { … }` block is optional.  Because stores, schedulers and
//! services share a single expansion scope, every store / scheduler a
//! service refers to is checked against its declaration **at compile time**
//! (a misspelled name is an `E0425`), even though the service constructors
//! still resolve those names through the [`StoreManager`] / scheduler maps
//! at runtime.
//!
//! Each service is registered under an *instance name*, written as an
//! optional string literal between the binding name and the service kind;
//! it defaults to `"main"` when omitted:
//!
//! ```ignore
//! services {
//!     cas:       Cas { cas_store: nix_store },                 // instance "main"
//!     other_cas: "other" Cas { cas_store: nix_store },         // instance "other"
//! }
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
//!     cache     = FastSlow { fast: fast_fs, slow: nix_store },
//! }
//! ```
//!
//! For wrapper stores the child-store fields are written with a store name;
//! every *other* field of the underlying spec must be supplied verbatim
//! (the macro does not guess defaults).
//!
//! [`StoreManager`]: nativelink_store::store_manager::StoreManager

use std::collections::HashMap;
use std::sync::Arc;

use nativelink_config::cas_server::WithInstanceName;
use nativelink_config::schedulers::{SchedulerSpec, SimpleSpec};
use nativelink_config::stores::{ExperimentalCloudObjectSpec, NoopSpec, StoreSpec};
use nativelink_scheduler::worker_scheduler::WorkerScheduler;
use nativelink_store::store_manager::StoreManager;
use nativelink_util::operation_state_manager::ClientStateManager;
use tonic::service::Routes;

/// The fully-constructed store / scheduler / service topology produced by
/// the [`topology!`] macro.
///
/// The `routes` field is built from the optional `services { … }` block; it
/// is an empty [`Routes`] when that block is omitted.
pub struct Topology {
    /// Registry of every named store declared in the `stores { … }` block.
    pub store_manager: Arc<StoreManager>,
    /// Action schedulers keyed by the names in the `schedulers { … }` block.
    pub action_schedulers: HashMap<String, Arc<dyn ClientStateManager>>,
    /// Worker schedulers keyed by the names in the `schedulers { … }` block.
    pub worker_schedulers: HashMap<String, Arc<dyn WorkerScheduler>>,
    /// Tonic routing builder populated from the optional `services { … }`
    /// block, ready for `.into_axum_router()`.
    pub routes: Routes,
}

impl core::fmt::Debug for Topology {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The scheduler trait objects are not `Debug`, so report their
        // registered names instead.
        f.debug_struct("Topology")
            .field("store_manager", &self.store_manager)
            .field(
                "action_schedulers",
                &self.action_schedulers.keys().collect::<Vec<_>>(),
            )
            .field(
                "worker_schedulers",
                &self.worker_schedulers.keys().collect::<Vec<_>>(),
            )
            .field("routes", &self.routes)
            .finish()
    }
}

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

/// Wrap a single service config in the `WithInstanceName` vec expected by
/// the service constructors, under the given instance name.
#[doc(hidden)]
#[must_use]
pub fn with_instance<T>(instance_name: impl Into<String>, config: T) -> Vec<WithInstanceName<T>> {
    vec![WithInstanceName {
        instance_name: instance_name.into(),
        config,
    }]
}

// ── Exhaustiveness guards ──────────────────────────────────────────────
//
// `macro_rules!` has no knowledge of the type system, so nothing links the
// `topology!` `@store` / `@scheduler` arms to the variants of `StoreSpec` /
// `SchedulerSpec`. These never-executed functions borrow the compiler's
// own exhaustiveness checking instead: each is a wildcard-free `match`, so
// adding a new spec variant fails to compile *here* ("non-exhaustive
// patterns"), reminding whoever adds the variant to also add the matching
// macro arm named in the comment beside each pattern.

/// Compile-time guard mirroring the `@store` keyword set against
/// [`StoreSpec`]. Each arm names the `topology!` store keyword it maps to.
#[doc(hidden)]
fn _assert_store_spec_exhaustive(spec: &StoreSpec) {
    match spec {
        StoreSpec::Memory(_) => {}                       // Memory
        StoreSpec::ExperimentalCloudObjectStore(_) => {} // Aws / Gcs / OntapS3
        StoreSpec::OntapS3ExistenceCache(_) => {}        // OntapS3ExistenceCache
        StoreSpec::Verify(_) => {}                       // Verify
        StoreSpec::CompletenessChecking(_) => {}         // CompletenessChecking
        StoreSpec::Compression(_) => {}                  // Compression
        StoreSpec::Dedup(_) => {}                        // Dedup
        StoreSpec::ExistenceCache(_) => {}               // ExistenceCache
        StoreSpec::FastSlow(_) => {}                     // FastSlow
        StoreSpec::Shard(_) => {}                        // Shard
        StoreSpec::Filesystem(_) => {}                   // Filesystem
        StoreSpec::RefStore(_) => {}                     // Ref
        StoreSpec::SizePartitioning(_) => {}             // SizePartitioning
        StoreSpec::Grpc(_) => {}                         // Grpc
        StoreSpec::RedisStore(_) => {}                   // Redis
        StoreSpec::NixStore(_) => {}                     // Nix
        StoreSpec::Noop(_) => {}                         // Noop
        StoreSpec::ExperimentalMongo(_) => {}            // Mongo
    }
}

/// Compile-time guard for the cloud-object provider sub-enum, which the
/// macro splits into the distinct `Aws` / `Gcs` / `OntapS3` keywords.
#[doc(hidden)]
fn _assert_cloud_object_spec_exhaustive(spec: &ExperimentalCloudObjectSpec) {
    match spec {
        ExperimentalCloudObjectSpec::Aws(_) => {}   // Aws
        ExperimentalCloudObjectSpec::Gcs(_) => {}   // Gcs
        ExperimentalCloudObjectSpec::Ontap(_) => {} // OntapS3
    }
}

/// Compile-time guard mirroring the `@scheduler` keyword set against
/// [`SchedulerSpec`]. Each arm names the `topology!` scheduler keyword.
#[doc(hidden)]
fn _assert_scheduler_spec_exhaustive(spec: &SchedulerSpec) {
    match spec {
        SchedulerSpec::Simple(_) => {}           // Simple
        SchedulerSpec::Grpc(_) => {}             // Grpc
        SchedulerSpec::CacheLookup(_) => {}      // CacheLookup
        SchedulerSpec::PropertyModifier(_) => {} // PropertyModifier
        SchedulerSpec::NixProxy(_) => {}         // NixProxy
    }
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

    pub use nativelink_config::cas_server::{
        AcStoreConfig, ByteStreamConfig, CapabilitiesConfig, CapabilitiesRemoteExecutionConfig,
        CasStoreConfig, ExecutionConfig,
    };
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
    pub use nativelink_service::ac_server::AcServer;
    pub use nativelink_service::bytestream_server::ByteStreamServer;
    pub use nativelink_service::capabilities_server::CapabilitiesServer;
    pub use nativelink_service::cas_server::CasServer;
    pub use nativelink_service::execution_server::ExecutionServer;
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
    pub use tonic::service::Routes;
}

/// Construct a nativelink store, scheduler and service topology.
///
/// See the [crate-level documentation](crate) for the full surface syntax,
/// invocation requirements, and rationale.
///
/// Expands to a block evaluating to a [`Topology`]. The `services { … }`
/// block is optional; when omitted, [`Topology::routes`] is empty.
///
/// An optional leading `clock: <expr>;` supplies the `fn() -> SystemTime`
/// the schedulers use as their clock (e.g. a deterministic clock in tests);
/// it defaults to `SystemTime::now`. The clock must be a non-capturing
/// function coercible to `fn() -> SystemTime` (the factories take a bare
/// function pointer, so a closure holding state cannot be used).
#[macro_export]
macro_rules! topology {
    // ── Entry point (explicit scheduler clock) ─────────────────────────
    (
        clock: $clock:expr;
        stores { $( $sname:ident = $skw:ident $({ $($sf:tt)* })? ),* $(,)? }
        schedulers { $( $schname:ident = $schkw:ident { $($schf:tt)* } ),* $(,)? }
        $( services { $( $svc_name:ident : $( $svc_in:literal )? $svc_kw:ident { $($svc_f:tt)* } ),* $(,)? } )?
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
        // (wrapper) stores and services, and registered in the store manager.
        $(
            let $sname: $crate::__rt::Store =
                $crate::__topology_store!(store_manager, $skw $({ $($sf)* })?);
            store_manager.add_store(stringify!($sname), $sname.clone());
        )*

        // Schedulers: bind the per-variant factory results under each
        // scheduler's own name (so later service references can be
        // name-checked by the compiler), then populate the action / worker
        // maps from them. `$clock` is threaded in as the scheduler clock.
        $(
            let $schname =
                $crate::__topology_scheduler!(store_manager, $clock, $schkw { $($schf)* });
            if let Some(__a) = $schname.0.clone() {
                let __a: $crate::__rt::Arc<dyn $crate::__rt::ClientStateManager> = __a;
                action_schedulers.insert(stringify!($schname).to_string(), __a);
            }
            if let Some(__w) = $schname.1.clone() {
                worker_schedulers.insert(stringify!($schname).to_string(), __w);
            }
        )*

        // Services (optional): each `@svc` references its store / scheduler
        // dependencies by their binding, so a misspelled name is a compile
        // error, while construction still resolves the names through
        // `store_manager` / the scheduler maps at runtime. The optional
        // leading string literal sets the service's instance name (default
        // `"main"`).
        let routes = $crate::__rt::Routes::builder().routes()
            $($(
                .add_service($crate::__topology_svc!(
                    store_manager, action_schedulers, worker_schedulers,
                    $crate::__topology_instance_name!($($svc_in)?),
                    $svc_kw { $($svc_f)* }
                ))
            )*)?;

        $crate::Topology {
            store_manager,
            action_schedulers,
            worker_schedulers,
            routes,
        }
    }};

    // ── Entry point (default clock) ────────────────────────────────────
    // Forwards to the explicit-clock arm with the real wall clock. Each
    // block's tokens are forwarded verbatim for that arm to parse.
    (
        stores { $($stores:tt)* }
        schedulers { $($scheds:tt)* }
        $( services { $($svcs:tt)* } )?
    ) => {
        $crate::topology! {
            clock: $crate::__rt::SystemTime::now;
            stores { $($stores)* }
            schedulers { $($scheds)* }
            $( services { $($svcs)* } )?
        }
    };
}

/// Internal: construct one named store binding.
///
/// Split out of [`topology!`] purely for readability. It is
/// `#[macro_export] #[doc(hidden)]` rather than a private `macro_rules!`
/// because [`topology!`] is itself exported and expands in other crates,
/// where only `$crate::`-reachable (i.e. exported) macros are in scope. Not
/// part of the public API and not covered by semver.
#[doc(hidden)]
#[macro_export]
macro_rules! __topology_store {
    // ── Leaf stores ────────────────────────────────────────────────────
    // The `{ … }` fields are forwarded verbatim into the underlying spec,
    // so the caller writes valid spec fields directly.
    ($sm:ident, Noop) => {
        $crate::__rt::Store::new($crate::__rt::NoopStore::new())
    };
    ($sm:ident, Memory { $($f:tt)* }) => {
        $crate::__rt::Store::new($crate::__rt::MemoryStore::new(&$crate::__rt::MemorySpec { $($f)* }))
    };
    ($sm:ident, Nix { $($f:tt)* }) => {
        $crate::__rt::Store::new($crate::__rt::NixStore::new(&$crate::__rt::NixSpec { $($f)* }).await?)
    };
    ($sm:ident, Filesystem { $($f:tt)* }) => {
        $crate::__rt::Store::new(
            <$crate::__rt::FilesystemStore>::new(&$crate::__rt::FilesystemSpec { $($f)* }).await?,
        )
    };
    ($sm:ident, Redis { $($f:tt)* }) => {
        $crate::__rt::Store::new($crate::__rt::RedisStore::new($crate::__rt::RedisSpec { $($f)* })?)
    };
    ($sm:ident, Grpc { $($f:tt)* }) => {
        $crate::__rt::Store::new($crate::__rt::GrpcStore::new(&$crate::__rt::StoreGrpcSpec { $($f)* }).await?)
    };
    ($sm:ident, Mongo { $($f:tt)* }) => {
        $crate::__rt::Store::new(
            $crate::__rt::ExperimentalMongoStore::new($crate::__rt::ExperimentalMongoSpec { $($f)* }).await?,
        )
    };
    ($sm:ident, Aws { $($f:tt)* }) => {
        $crate::__rt::Store::new(
            $crate::__rt::S3Store::new(&$crate::__rt::ExperimentalAwsSpec { $($f)* }, $crate::__rt::SystemTime::now).await?,
        )
    };
    ($sm:ident, Gcs { $($f:tt)* }) => {
        $crate::__rt::Store::new(
            $crate::__rt::GcsStore::new(&$crate::__rt::ExperimentalGcsSpec { $($f)* }, $crate::__rt::SystemTime::now).await?,
        )
    };
    ($sm:ident, OntapS3 { $($f:tt)* }) => {
        $crate::__rt::Store::new(
            $crate::__rt::OntapS3Store::new(&$crate::__rt::ExperimentalOntapS3Spec { $($f)* }, $crate::__rt::SystemTime::now).await?,
        )
    };
    ($sm:ident, OntapS3ExistenceCache { $($f:tt)* }) => {
        $crate::__rt::Store::new(
            $crate::__rt::OntapS3ExistenceCache::new(&$crate::__rt::OntapS3ExistenceCacheSpec { $($f)* }, $crate::__rt::SystemTime::now).await?,
        )
    };
    ($sm:ident, Ref { $($f:tt)* }) => {
        $crate::__rt::Store::new($crate::__rt::RefStore::new(
            &$crate::__rt::RefSpec { $($f)* },
            $crate::__rt::Arc::downgrade(&$sm),
        ))
    };

    // ── Wrapper stores ─────────────────────────────────────────────────
    // Child-store fields take the *name* of an earlier store binding; every
    // remaining spec field must be supplied verbatim after the children.
    ($sm:ident, Verify { backend: $b:ident $(, $($rest:tt)*)? }) => {
        $crate::__rt::Store::new($crate::__rt::VerifyStore::new(
            &$crate::__rt::VerifySpec { backend: $crate::placeholder_store_spec() $(, $($rest)*)? },
            $b.clone(),
        ))
    };
    ($sm:ident, Compression { backend: $b:ident $(, $($rest:tt)*)? }) => {
        $crate::__rt::Store::new($crate::__rt::CompressionStore::new(
            &$crate::__rt::CompressionSpec { backend: $crate::placeholder_store_spec() $(, $($rest)*)? },
            $b.clone(),
        )?)
    };
    ($sm:ident, ExistenceCache { backend: $b:ident $(, $($rest:tt)*)? }) => {
        $crate::__rt::Store::new($crate::__rt::ExistenceCacheStore::new(
            &$crate::__rt::ExistenceCacheSpec { backend: $crate::placeholder_store_spec() $(, $($rest)*)? },
            $b.clone(),
        ))
    };
    ($sm:ident, Dedup { index_store: $i:ident, content_store: $c:ident $(, $($rest:tt)*)? }) => {
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
    ($sm:ident, CompletenessChecking { backend: $b:ident, cas_store: $c:ident $(,)? }) => {
        $crate::__rt::Store::new($crate::__rt::CompletenessCheckingStore::new($b.clone(), $c.clone()))
    };
    ($sm:ident, FastSlow { fast: $f:ident, slow: $s:ident $(, $($rest:tt)*)? }) => {
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
    ($sm:ident, SizePartitioning { lower_store: $l:ident, upper_store: $u:ident $(, $($rest:tt)*)? }) => {
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
    ($sm:ident, Shard { stores: [ $($s:ident),* $(,)? ] $(,)? }) => {
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
    // Catch-all: an unrecognized store keyword (or one written with the
    // wrong child-store fields) produces a readable error instead of the
    // default "no rules expected this token".
    ($sm:ident, $other:ident $({ $($f:tt)* })?) => {
        ::core::compile_error!(::core::concat!(
            "`topology!`: unknown store kind `",
            ::core::stringify!($other),
            "` (or wrong fields for that store)",
        ))
    };
}

/// Internal: construct one named scheduler, yielding a
/// `SchedulerFactoryResults`
/// (`(Option<Arc<dyn ClientStateManager>>, Option<Arc<dyn WorkerScheduler>>)`).
///
/// See [`__topology_store!`] for why this is an exported-but-hidden helper.
#[doc(hidden)]
#[macro_export]
macro_rules! __topology_scheduler {
    ($sm:ident, $clock:expr, Simple { $($f:tt)* }) => {
        $crate::__rt::simple_scheduler_factory(
            &$crate::__rt::SimpleSpec { $($f)* },
            &$sm,
            $clock,
            None,
        )?
    };
    ($sm:ident, $clock:expr, Grpc { $($f:tt)* }) => {
        $crate::__rt::grpc_scheduler_factory(&$crate::__rt::SchedGrpcSpec { $($f)* })?
    };
    ($sm:ident, $clock:expr, NixProxy { ac_store: $ac:ident, cas_store: $cas:ident $(,)? }) => {
        $crate::__rt::nix_scheduler_factory(
            &$crate::__rt::NixProxySpec {
                ac_store: stringify!($ac).to_string(),
                cas_store: stringify!($cas).to_string(),
            },
            &$sm,
            $clock,
        )?
    };
    ($sm:ident, $clock:expr, CacheLookup {
        ac_store: $ac:ident,
        scheduler: $inner_kw:ident { $($inner_f:tt)* }
        $(, $($rest:tt)*)?
    }) => {{
        let nested = $crate::__topology_scheduler!($sm, $clock, $inner_kw { $($inner_f)* });
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
    ($sm:ident, $clock:expr, PropertyModifier {
        scheduler: $inner_kw:ident { $($inner_f:tt)* }
        $(, $($rest:tt)*)?
    }) => {{
        let nested = $crate::__topology_scheduler!($sm, $clock, $inner_kw { $($inner_f)* });
        $crate::__rt::property_modifier_scheduler_factory(
            &$crate::__rt::PropertyModifierSpec {
                scheduler: ::std::boxed::Box::new($crate::placeholder_sched_spec())
                $(, $($rest)*)?
            },
            nested,
        )?
    }};

    // Catch-all: an unrecognized scheduler keyword (or wrong fields)
    // produces a readable error.
    ($sm:ident, $clock:expr, $other:ident { $($f:tt)* }) => {
        ::core::compile_error!(::core::concat!(
            "`topology!`: unknown scheduler kind `",
            ::core::stringify!($other),
            "` (or wrong fields for that scheduler)",
        ))
    };
}

/// Internal: resolve a service's optional instance-name literal to a
/// concrete name, defaulting to `"main"` when omitted.
///
/// See [`__topology_store!`] for why this is an exported-but-hidden helper.
#[doc(hidden)]
#[macro_export]
macro_rules! __topology_instance_name {
    () => {
        "main"
    };
    ($name:literal) => {
        $name
    };
}

/// Internal: construct one tonic service. The leading `let _ = …;` line
/// ties every store / scheduler dependency to its `topology!` binding so a
/// misspelled name fails to compile; the constructors still resolve the
/// names through `store_manager` / the scheduler maps at runtime. `$name`
/// is the service's instance name.
///
/// See [`__topology_store!`] for why this is an exported-but-hidden helper.
#[doc(hidden)]
#[macro_export]
macro_rules! __topology_svc {
    ($sm:ident, $act:ident, $wrk:ident, $name:expr, Cas { cas_store: $cs:ident $(,)? }) => {{
        let _: &$crate::__rt::Store = &$cs;
        $crate::__rt::CasServer::new(
            &$crate::with_instance($name, $crate::__rt::CasStoreConfig {
                cas_store: stringify!($cs).to_string(),
            }),
            &$sm,
        )?
        .into_service()
    }};
    ($sm:ident, $act:ident, $wrk:ident, $name:expr, Ac { ac_store: $acs:ident, read_only: $ro:expr $(,)? }) => {{
        let _: &$crate::__rt::Store = &$acs;
        $crate::__rt::AcServer::new(
            &$crate::with_instance($name, $crate::__rt::AcStoreConfig {
                ac_store: stringify!($acs).to_string(),
                read_only: $ro,
            }),
            &$sm,
        )?
        .into_service()
    }};
    ($sm:ident, $act:ident, $wrk:ident, $name:expr, Execution { cas_store: $cs:ident, scheduler: $sch:ident $(,)? }) => {{
        let _: &$crate::__rt::Store = &$cs;
        let _ = &$sch;
        $crate::__rt::ExecutionServer::new(
            &$crate::with_instance($name, $crate::__rt::ExecutionConfig {
                cas_store: stringify!($cs).to_string(),
                scheduler: stringify!($sch).to_string(),
            }),
            &$act,
            &$sm,
        )?
        .into_service()
    }};
    ($sm:ident, $act:ident, $wrk:ident, $name:expr, Capabilities { scheduler: $sch:ident $(,)? }) => {{
        let _ = &$sch;
        $crate::__rt::CapabilitiesServer::new(
            &$crate::with_instance($name, $crate::__rt::CapabilitiesConfig {
                remote_execution: Some($crate::__rt::CapabilitiesRemoteExecutionConfig {
                    scheduler: stringify!($sch).to_string(),
                }),
            }),
            &$act,
        )
        .await?
        .into_service()
    }};
    ($sm:ident, $act:ident, $wrk:ident, $name:expr, ByteStream { cas_store: $cs:ident $(, $field:ident: $val:expr)* $(,)? }) => {{
        let _: &$crate::__rt::Store = &$cs;
        $crate::__rt::ByteStreamServer::new(
            &$crate::__rt::ByteStreamConfig {
                cas_stores: ::std::collections::HashMap::from([(
                    $name.to_string(),
                    stringify!($cs).to_string(),
                )]),
                $($field: $val,)*
                ..Default::default()
            },
            &$sm,
        )?
        .into_service()
    }};

    // Catch-all: an unrecognized service keyword (or wrong fields) produces
    // a readable error.
    ($sm:ident, $act:ident, $wrk:ident, $name:expr, $other:ident { $($f:tt)* }) => {
        ::core::compile_error!(::core::concat!(
            "`topology!`: unknown service kind `",
            ::core::stringify!($other),
            "` (or wrong fields for that service)",
        ))
    };
}
