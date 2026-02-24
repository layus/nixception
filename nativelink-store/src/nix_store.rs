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
use std::os::unix::net::UnixStream;
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
use nix_remote::ValidPathInfoWithPath;
use nix_remote::worker_op::Resp;
use nix_remote::worker_op::StreamingRecv;
use nix_remote::worker_op::WorkerOp;
use nix_remote::worker_op::{AddToStore, WithFramedSource};
use nix_remote::{nix_client::NixDaemonClient, stderr::Msg};

use tokio::io::AsyncReadExt;

#[derive(MetricsComponent, Debug)]
pub struct NixStore {
    #[metric(help = "The path of the daemon unix socket")]
    socket_path: String,
}

/// Describes how to stream data to the nix daemon.
enum DataSource<'a> {
    /// A fixed buffer available in memory.
    Buffer(&'a [u8]),
    /// An async stream with a known exact total size.
    Stream {
        reader: DropCloserReadHalf,
        total_size: usize,
    },
}

impl NixStore {
    pub async fn new(spec: &NixSpec) -> Result<Arc<Self>, Error> {
        Ok(Arc::new(Self {
            socket_path: spec
                .socket_path
                .clone()
                .unwrap_or_else(|| "/nix/var/nix/daemon-socket/socket".to_string()),
        }))
    }

    /// Internal method that handles the full upload-to-nix-daemon flow:
    ///  1. Connect to the daemon
    ///  2. Send the AddToStore operation
    ///  3. Stream the data (from a buffer or an async reader)
    ///  4. Read error messages and the final reply
    ///
    /// Returns the `ValidPathInfoWithPath` reply from the daemon.
    async fn upload_to_nix_daemon(
        &self,
        name: &str,
        cam_str: &str,
        refs: StorePathSet,
        data: DataSource<'_>,
    ) -> Result<ValidPathInfoWithPath, Error> {
        // Connect to the daemon socket.
        let mut client = {
            let read_socket = UnixStream::connect(self.socket_path.clone())
                .err_tip(|| format!("While opening socket '{}'", self.socket_path))?;
            let write_socket = read_socket
                .try_clone()
                .err_tip(|| "While cloning nix daemon socket")?;
            NixDaemonClient::new(read_socket, write_socket)
                .map_err(|e| make_err!(Code::Internal, "While creating a new NixClient: {}", e))
        }?;

        // Build and send the AddToStore operation.
        let add_to_store_op = AddToStore {
            name: nix_remote::StorePath(name.to_string().into()),
            cam_str: nix_remote::StorePath(cam_str.to_owned().into()),
            refs,
            repair: false,
        };
        let response_type: Resp<ValidPathInfoWithPath> = Default::default();
        let add_to_store_worker_op =
            &WorkerOp::AddToStore(WithFramedSource(add_to_store_op), response_type.clone());

        let () = client
            .send_worker_op_to_daemon(add_to_store_worker_op)
            .map_err(|e| make_err!(Code::Internal, "Sending AddToStore op to daemon: {}", e))?;

        debug_assert!(add_to_store_worker_op.requires_streaming());

        // Stream the data to the daemon.
        match data {
            DataSource::Buffer(content) => {
                let length = content.len();
                client.streaming_write_len(length as u64).unwrap();
                client.streaming_write_buff(content, length).unwrap();
            }
            DataSource::Stream {
                mut reader,
                mut total_size,
            } => {
                while total_size > 0 {
                    let bytes = reader.recv().await.unwrap();
                    let chunk_len = bytes.len();
                    client.streaming_write_len(chunk_len as u64).unwrap();
                    client
                        .streaming_write_buff(bytes.as_ref(), chunk_len)
                        .unwrap();
                    total_size -= chunk_len;
                }
            }
        }
        // Signal end-of-stream and flush.
        client.streaming_write_len(0).unwrap();
        client.flush().unwrap();

        // Read error messages from the daemon.
        loop {
            let error_message_from_builder = client.read_error_msg().unwrap();
            if error_message_from_builder == Msg::Last(()) {
                break;
            }
            dbg!(&error_message_from_builder);
        }

        // Get the final reply.
        let reply = client
            .read_build_response_from_daemon(&response_type)
            .map_err(|e| make_err!(Code::Internal, "{}", e))?;

        Ok(reply)
    }

    pub async fn add_to_store(
        self: Pin<&Self>,
        digest: String,
        content: &[u8],
        name: &str,
        inputs: &Vec<StorePath<String>>,
    ) -> Result<(), Error> {
        let refs = StorePathSet {
            paths: {
                let mut paths: Vec<nix_remote::StorePath> = inputs
                    .into_iter()
                    .map(|sp| nix_remote::StorePath(sp.to_absolute_path().into()))
                    .collect();
                paths.sort();
                paths
            },
        };

        let _reply = self
            .upload_to_nix_daemon(name, &digest, refs, DataSource::Buffer(content))
            .await?;

        // Write to nix store was successful, but we need to check that the
        // content matches the digest in the key. We do not support arbitrary
        // keys, only sh256 digests of the content.
        // let () = (_reply.info.content_address.to_string().unwrap() == digest)
        //     .then_some(())
        //     .ok_or(make_err!(
        //         Code::InvalidArgument,
        //         "Key {:?} does not match the content.",
        //         &digest
        //     ))?;

        Ok(())
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
            "Nix backend does not support arbirary strings as keys"
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
        let data = DataSource::Stream { reader, total_size };

        let reply = self
            .upload_to_nix_daemon("reapi-adapted", &nix_ca, refs, data)
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
