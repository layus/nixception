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

use core::pin::Pin;
use std::sync::Arc;
#[cfg(any(feature = "s3", feature = "gcs"))]
use std::time::SystemTime;

use futures::stream::FuturesOrdered;
use futures::{Future, TryStreamExt};
#[cfg(any(feature = "s3", feature = "gcs"))]
use nativelink_config::stores::ExperimentalCloudObjectSpec;
use nativelink_config::stores::StoreSpec;
use nativelink_error::Error;
#[cfg(not(all(feature = "s3", feature = "gcs", feature = "redis", feature = "mongo")))]
use nativelink_error::make_input_err;
use nativelink_util::health_utils::HealthRegistryBuilder;
use nativelink_util::store_trait::{Store, StoreDriver};

use crate::completeness_checking_store::CompletenessCheckingStore;
use crate::compression_store::CompressionStore;
use crate::dedup_store::DedupStore;
use crate::existence_cache_store::ExistenceCacheStore;
use crate::fast_slow_store::FastSlowStore;
use crate::filesystem_store::FilesystemStore;
#[cfg(feature = "gcs")]
use crate::gcs_store::GcsStore;
use crate::grpc_store::GrpcStore;
use crate::memory_store::MemoryStore;
#[cfg(feature = "mongo")]
use crate::mongo_store::ExperimentalMongoStore;
use crate::noop_store::NoopStore;
#[cfg(feature = "s3")]
use crate::ontap_s3_existence_cache_store::OntapS3ExistenceCache;
#[cfg(feature = "s3")]
use crate::ontap_s3_store::OntapS3Store;
#[cfg(feature = "redis")]
use crate::redis_store::RedisStore;
use crate::ref_store::RefStore;
#[cfg(feature = "s3")]
use crate::s3_store::S3Store;
use crate::nix_store::NixStore;
use crate::shard_store::ShardStore;
use crate::size_partitioning_store::SizePartitioningStore;
use crate::store_manager::StoreManager;
use crate::verify_store::VerifyStore;

type FutureMaybeStore<'a> = Box<dyn Future<Output = Result<Store, Error>> + Send + 'a>;

pub fn store_factory<'a>(
    backend: &'a StoreSpec,
    store_manager: &'a Arc<StoreManager>,
    maybe_health_registry_builder: Option<&'a mut HealthRegistryBuilder>,
) -> Pin<FutureMaybeStore<'a>> {
    Box::pin(async move {
        let store: Arc<dyn StoreDriver> = match backend {
            StoreSpec::Memory(spec) => MemoryStore::new(spec),
            #[cfg(any(feature = "s3", feature = "gcs"))]
            StoreSpec::ExperimentalCloudObjectStore(spec) => match spec {
                #[cfg(feature = "s3")]
                ExperimentalCloudObjectSpec::Aws(aws_config) => {
                    S3Store::new(aws_config, SystemTime::now).await?
                }
                #[cfg(feature = "s3")]
                ExperimentalCloudObjectSpec::Ontap(ontap_config) => {
                    OntapS3Store::new(ontap_config, SystemTime::now).await?
                }
                #[cfg(feature = "gcs")]
                ExperimentalCloudObjectSpec::Gcs(gcs_config) => {
                    GcsStore::new(gcs_config, SystemTime::now).await?
                }
                #[cfg(not(all(feature = "s3", feature = "gcs")))]
                _ => {
                    return Err(make_input_err!(
                        "This nixception build was compiled without support for the requested \
                         cloud object store backend (enable the `s3` / `gcs` feature)"
                    ));
                }
            },
            #[cfg(not(any(feature = "s3", feature = "gcs")))]
            StoreSpec::ExperimentalCloudObjectStore(_) => {
                return Err(make_input_err!(
                    "This nixception build was compiled without cloud object store support \
                     (enable the `s3` / `gcs` feature)"
                ));
            }
            StoreSpec::NixStore(spec) => NixStore::new(spec).await?,
            #[cfg(feature = "redis")]
            StoreSpec::RedisStore(spec) => RedisStore::new(spec.clone())?,
            #[cfg(not(feature = "redis"))]
            StoreSpec::RedisStore(_) => {
                return Err(make_input_err!(
                    "This nixception build was compiled without Redis store support \
                     (enable the `redis` feature)"
                ));
            }
            StoreSpec::Verify(spec) => VerifyStore::new(
                spec,
                store_factory(&spec.backend, store_manager, None).await?,
            ),
            StoreSpec::Compression(spec) => CompressionStore::new(
                &spec.clone(),
                store_factory(&spec.backend, store_manager, None).await?,
            )?,
            StoreSpec::Dedup(spec) => DedupStore::new(
                spec,
                store_factory(&spec.index_store, store_manager, None).await?,
                store_factory(&spec.content_store, store_manager, None).await?,
            )?,
            StoreSpec::ExistenceCache(spec) => ExistenceCacheStore::new(
                spec,
                store_factory(&spec.backend, store_manager, None).await?,
            ),
            #[cfg(feature = "s3")]
            StoreSpec::OntapS3ExistenceCache(spec) => {
                OntapS3ExistenceCache::new(spec, SystemTime::now).await?
            }
            #[cfg(not(feature = "s3"))]
            StoreSpec::OntapS3ExistenceCache(_) => {
                return Err(make_input_err!(
                    "This nixception build was compiled without ONTAP S3 support \
                     (enable the `s3` feature)"
                ));
            }
            StoreSpec::CompletenessChecking(spec) => CompletenessCheckingStore::new(
                store_factory(&spec.backend, store_manager, None).await?,
                store_factory(&spec.cas_store, store_manager, None).await?,
            ),
            StoreSpec::FastSlow(spec) => FastSlowStore::new(
                spec,
                store_factory(&spec.fast, store_manager, None).await?,
                store_factory(&spec.slow, store_manager, None).await?,
            ),
            StoreSpec::Filesystem(spec) => <FilesystemStore>::new(spec).await?,
            StoreSpec::RefStore(spec) => RefStore::new(spec, Arc::downgrade(store_manager)),
            StoreSpec::SizePartitioning(spec) => SizePartitioningStore::new(
                spec,
                store_factory(&spec.lower_store, store_manager, None).await?,
                store_factory(&spec.upper_store, store_manager, None).await?,
            ),
            StoreSpec::Grpc(spec) => GrpcStore::new(spec).await?,
            StoreSpec::Noop(_) => NoopStore::new(),
            #[cfg(feature = "mongo")]
            StoreSpec::ExperimentalMongo(spec) => ExperimentalMongoStore::new(spec.clone()).await?,
            #[cfg(not(feature = "mongo"))]
            StoreSpec::ExperimentalMongo(_) => {
                return Err(make_input_err!(
                    "This nixception build was compiled without MongoDB store support \
                     (enable the `mongo` feature)"
                ));
            }
            StoreSpec::Shard(spec) => {
                let stores = spec
                    .stores
                    .iter()
                    .map(|store_spec| store_factory(&store_spec.store, store_manager, None))
                    .collect::<FuturesOrdered<_>>()
                    .try_collect::<Vec<_>>()
                    .await?;
                ShardStore::new(spec, stores)?
            }
        };

        if let Some(health_registry_builder) = maybe_health_registry_builder {
            store.clone().register_health(health_registry_builder);
        }

        Ok(Store::new(store))
    })
}
