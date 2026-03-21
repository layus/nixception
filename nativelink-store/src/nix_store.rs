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
use nativelink_util::store_trait::{RemoveItemCallback, StoreDriver, StoreKey, UploadSizeInfo};

use nix_compat::nixhash::CAHash;
use nix_compat::nixhash::NixHash;
use nix_compat::store_path::StorePath;
use nix_compat::store_path::build_ca_path;
use nix_remote::StorePathSet;

use tokio::io::AsyncReadExt;

use crate::nix_daemon_connection::NixDaemonConnection;

#[derive(MetricsComponent, Debug)]
pub struct NixStore {
    #[metric(help = "The path of the daemon unix socket")]
    socket_path: String,

    /// Shared connection handle to the Nix daemon.  All daemon
    /// operations are routed through this object.
    connection: Arc<NixDaemonConnection>,
}

impl NixStore {
    pub async fn new(spec: &NixSpec) -> Result<Arc<Self>, Error> {
        let socket_path = spec
            .socket_path
            .clone()
            .or_else(|| Self::socket_path_from_env())
            .unwrap_or_else(|| "/nix/var/nix/daemon-socket/socket".to_string());

        let connection = NixDaemonConnection::new(socket_path.clone());

        Ok(Arc::new(Self {
            socket_path,
            connection,
        }))
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

    /// Return a clone of the shared [`NixDaemonConnection`] handle.
    ///
    /// Other components (e.g. the scheduler/worker) should obtain the
    /// connection via this method and call its APIs directly, rather
    /// than going through the store's `StoreDriver` trait.
    pub fn connection(&self) -> Arc<NixDaemonConnection> {
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
                Path::new(&sp.to_absolute_path())
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

        let total_size: usize = match upload_size {
            UploadSizeInfo::ExactSize(size) => size
                .try_into()
                .map_err(|_| make_err!(Code::Internal, "Cannot convert {} to usize", size)),
            UploadSizeInfo::MaxSize(_) => Err(make_err!(
                Code::Unimplemented,
                "Size approximations are not supported by NixStore"
            )),
        }?;

        let refs = StorePathSet { paths: vec![] };

        let reply = self
            .connection
            .upload_to_nix_daemon("reapi-adapted", &nix_ca, refs, reader, total_size)
            .await?;

        // Write to nix store was successful, but we need to check that the
        // content matches the digest in the key. We do not support arbitrary
        // keys, only sh256 digests of the content.
        let () = (reply.info.content_address.to_string().unwrap() == nix_ca)
            .then_some(())
            .ok_or(make_err!(
                Code::InvalidArgument,
                "Key {:?} does not match the content.",
                &digest
            ))?;

        Ok(())
    }

    async fn get_part(
        self: Pin<&Self>,
        key: StoreKey<'_>,
        writer: &mut DropCloserWriteHalf,
        offset: u64,
        length: Option<u64>,
    ) -> Result<(), Error> {
        let sp = key_to_store_path(&key)?.to_absolute_path();
        let limit = length.unwrap_or(u64::MAX);
        let mut file = fs::open_file(sp.clone(), offset, limit).await?;
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

    fn register_remove_callback(
        self: Arc<Self>,
        callback: Arc<dyn RemoveItemCallback>,
    ) -> Result<(), Error> {
        drop(callback);
        Ok(())
    }
}
