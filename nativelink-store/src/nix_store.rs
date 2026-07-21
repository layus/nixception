// Copyright 2022 The Turbo Cache Authors. All rights reserved.
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

use std::borrow::Cow;
use std::marker::Send;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::BytesMut;

use nativelink_config::stores::NixSpec;
use nativelink_error::ResultExt;
use nativelink_error::{Code, Error, make_err};
use nativelink_metric::MetricsComponent;
use nativelink_util::buf_channel::{DropCloserReadHalf, DropCloserWriteHalf};
use nativelink_util::common::PackedHash;
use nativelink_util::fs;
use nativelink_util::health_utils::{HealthStatus, HealthStatusIndicator};
use nativelink_util::store_trait::{StoreDriver, StoreKey, UploadSizeInfo};

use nix_compat::nixhash::CAHash;
use nix_compat::nixhash::NixHash;
use nix_compat::store_path::StorePath;
use nix_compat::store_path::build_ca_path;
use nix_remote::StorePathSet;

use tokio::io::AsyncReadExt;
use tracing::{event, Level};

use crate::nix_daemon_connection::NixDaemonConnectionPool;

#[derive(MetricsComponent, Debug)]
pub struct NixStore {
    #[metric(help = "The path of the daemon unix socket")]
    socket_path: String,

    /// Physical root of a chroot store, if any.  When `Some(root)`, the store's
    /// own filesystem reads resolve `/nix/store/X` to `<root>/nix/store/X`.
    /// `None` reads the real `/nix/store` (the default).
    store_root: Option<String>,

    /// Connection pool to the Nix daemon.  All daemon operations are
    /// routed through this object.
    connection: Arc<NixDaemonConnectionPool>,
}

impl NixStore {
    pub async fn new(spec: &NixSpec) -> Result<Arc<Self>, Error> {
        let socket_path = spec
            .socket_path
            .clone()
            .or_else(|| Self::socket_path_from_env())
            .unwrap_or_else(|| "/nix/var/nix/daemon-socket/socket".to_string());

        let store_root = spec
            .store_root
            .clone()
            .or_else(|| std::env::var("NIXCEPTION_STORE_ROOT").ok())
            .filter(|s| !s.is_empty());

        let connection = NixDaemonConnectionPool::new_default(socket_path.clone());

        Ok(Arc::new(Self {
            socket_path,
            store_root,
            connection,
        }))
    }

    /// Map a logical store path (`/nix/store/X`, as used on the wire and in
    /// derivations) to the physical filesystem path the server should read.
    /// With a chroot store this prepends `store_root`; otherwise it is the
    /// identity, so behavior is unchanged when no root is configured.
    fn physical(&self, logical_abs_path: &str) -> String {
        match &self.store_root {
            Some(root) => format!("{}{}", root.trim_end_matches('/'), logical_abs_path),
            None => logical_abs_path.to_string(),
        }
    }

    /// Try to derive the daemon socket path from the `NIX_REMOTE`
    /// environment variable.  Recognises the `unix://` scheme that
    /// the recursive-nix sandbox sets (e.g.
    /// `unix:///build/.nix-socket`).
    fn socket_path_from_env() -> Option<String> {
        std::env::var("NIX_REMOTE")
            .ok()
            .and_then(|val| val.strip_prefix("unix://").map(|path| path.to_string()))
    }

    /// Return the socket path used by this store's connection pool.
    ///
    /// Other components that need their own pool (e.g. the scheduler)
    /// can use this to create a separate [`NixDaemonConnectionPool`]
    /// pointing at the same daemon.
    pub fn socket_path(&self) -> &str {
        &self.socket_path
    }

    /// Return a clone of the store's [`NixDaemonConnectionPool`] handle.
    ///
    /// This is primarily useful for tests or components that want to
    /// share the store's pool rather than creating their own.
    pub fn connection(&self) -> Arc<NixDaemonConnectionPool> {
        Arc::clone(&self.connection)
    }
}

#[async_trait]
impl HealthStatusIndicator for NixStore {
    fn get_name(&self) -> &'static str {
        "NixStore"
    }

    async fn check_health(&self, namespace: Cow<'static, str>) -> HealthStatus {
        StoreDriver::check_health(Pin::new(self), namespace).await
    }
}

pub fn key_to_store_path(key: &StoreKey) -> Result<StorePath<String>, Error> {
    let mykey = key.borrow();
    let digest = mykey.into_digest();
    let PackedHash(hash) = digest.packed_hash();
    let ca = CAHash::Flat(NixHash::Sha256(*hash));
    build_ca_path("reapi-adapted", &ca, std::iter::empty::<&str>(), false).map_err(|e| {
        make_err!(
            Code::InvalidArgument,
            "Failed to convert key into store path: {}",
            e
        )
    })
}

pub fn key_to_ca(key: &StoreKey) -> Result<CAHash, Error> {
    let StoreKey::Digest(digest) = key else {
        return Err(make_err!(
            Code::InvalidArgument,
            "Nix backend does not support arbitrary strings as keys"
        ));
    };
    let PackedHash(hash) = *digest.packed_hash();
    Ok(CAHash::Flat(NixHash::Sha256(hash)))
}

#[async_trait]
impl StoreDriver for NixStore {
    async fn has_with_results(
        self: Pin<&Self>,
        keys: &[StoreKey<'_>],
        results: &mut [Option<u64>],
    ) -> Result<(), Error> {
        for (key, result) in keys.iter().zip(results.iter_mut()) {
            *result = key_to_store_path(key).ok().and_then(|sp|
                // XXX: Use is_valid_path instead
                Path::new(&self.physical(&sp.to_absolute_path()))
                    .symlink_metadata()
                    .map(|m| m.len()).ok())
        }
        Ok(())
    }

    async fn update(
        self: Pin<&Self>,
        digest: StoreKey<'_>,
        reader: DropCloserReadHalf,
        upload_size: UploadSizeInfo,
    ) -> Result<(), Error> {
        let nix_ca = key_to_ca(&digest)?.to_nix_nixbase32_string();
        let expected_store_path = key_to_store_path(&digest)
            .map(|sp| sp.to_absolute_path())
            .unwrap_or_else(|_| "<unknown>".to_string());

        let total_size: usize = match upload_size {
            UploadSizeInfo::ExactSize(size) => size
                .try_into()
                .map_err(|_| make_err!(Code::Internal, "Cannot convert {} to usize", size)),
            UploadSizeInfo::MaxSize(_) => Err(make_err!(
                Code::Unimplemented,
                "Size approximations are not supported by NixStore"
            )),
        }?;

        // Check if the path already exists before uploading.
        let exists_before = self
            .connection
            .query_path_info(&expected_store_path)
            .await
            .map(|info| info.is_some())
            .unwrap_or(false);
        let exists_on_disk_before = Path::new(&self.physical(&expected_store_path)).exists();

        event!(
            Level::DEBUG,
            store_path = %expected_store_path,
            ca = %nix_ca,
            size = total_size,
            exists_before,
            exists_on_disk_before,
            "NixStore::update – uploading to nix daemon"
        );

        let refs = StorePathSet { paths: vec![] };

        let upload_result = self
            .connection
            .upload_to_nix_daemon("reapi-adapted", &nix_ca, refs, reader, total_size)
            .await;

        match upload_result {
            Ok(reply) => {
                // Write to nix store was successful, but we need to check that the
                // content matches the digest in the key. We do not support arbitrary
                // keys, only sh256 digests of the content.
                let () = (reply.info.content_address.to_string().unwrap() == nix_ca)
                    .then_some(())
                    .ok_or_else(|| {
                        let err = make_err!(
                            Code::InvalidArgument,
                            "Key {:?} does not match the content. expected ca={}, got ca={:?}, store_path={}",
                            &digest,
                            nix_ca,
                            reply.info.content_address.to_string(),
                            expected_store_path
                        );
                        event!(
                            Level::ERROR,
                            store_path = %expected_store_path,
                            expected_ca = %nix_ca,
                            actual_ca = ?reply.info.content_address.to_string(),
                            "NixStore::update – content-address mismatch"
                        );
                        err
                    })?;
                Ok(())
            }
            Err(e) => {
                // Check if the path exists after the failed upload.
                let exists_after = self
                    .connection
                    .query_path_info(&expected_store_path)
                    .await
                    .map(|info| info.is_some())
                    .unwrap_or(false);
                let exists_on_disk_after = Path::new(&self.physical(&expected_store_path)).exists();

                if exists_after || exists_on_disk_after {
                    // TEMPORARY: tolerate the error if the path exists in the
                    // store — the content is already available.
                    event!(
                        Level::WARN,
                        store_path = %expected_store_path,
                        ca = %nix_ca,
                        size = total_size,
                        exists_before,
                        exists_on_disk_before,
                        exists_after,
                        exists_on_disk_after,
                        error = %e,
                        "NixStore::update – upload failed but path exists, ignoring"
                    );
                    Ok(())
                } else {
                    event!(
                        Level::ERROR,
                        store_path = %expected_store_path,
                        ca = %nix_ca,
                        size = total_size,
                        exists_before,
                        exists_on_disk_before,
                        exists_after,
                        exists_on_disk_after,
                        error = %e,
                        "NixStore::update – upload failed and path does not exist"
                    );
                    Err(e)
                }
            }
        }
    }

    async fn get_part(
        self: Pin<&Self>,
        key: StoreKey<'_>,
        writer: &mut DropCloserWriteHalf,
        offset: u64,
        length: Option<u64>,
    ) -> Result<(), Error> {
        let sp = self.physical(&key_to_store_path(&key)?.to_absolute_path());
        let limit = length.unwrap_or(u64::MAX);
        let mut file = fs::open_file(sp.clone(), offset, limit)
            .await
            .err_tip(|| format!("NixStore::get_part opening {sp}"))?;
        loop {
            let mut buf = BytesMut::with_capacity(4096);
            file.read_buf(&mut buf)
                .await
                .err_tip(|| "Failed to read data in filesystem store")?;
            if buf.is_empty() {
                break; // EOF.
            }
            writer
                .send(buf.freeze())
                .await
                .err_tip(|| "Failed to send chunk in filesystem store get_part")?;
        }
        writer
            .send_eof()
            .err_tip(|| "Filed to send EOF in filesystem store get_part")?;

        Ok(())
    }

    fn inner_store(&self, _digest: Option<StoreKey>) -> &'_ dyn StoreDriver {
        self
    }

    fn as_any<'a>(&'a self) -> &'a (dyn std::any::Any + Sync + Send + 'static) {
        self
    }

    fn as_any_arc(self: Arc<Self>) -> Arc<dyn std::any::Any + Sync + Send + 'static> {
        self
    }

}
