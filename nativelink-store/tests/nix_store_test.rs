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

use nativelink_config::stores::NixSpec;
use nativelink_error::Error;
use nativelink_macro::nativelink_test;
use nativelink_store::nix_store::NixStore;
use nativelink_util::common::DigestInfo;
use nativelink_util::digest_hasher::{DigestHasher, DigestHasherFunc};
use nativelink_util::store_trait::{StoreKey, StoreLike};
use pretty_assertions::assert_eq;

const SOCKET_PATH: &str = "/nix/var/nix/daemon-socket/socket";

const VALID_HASH1: &str = "0123456789abcdef000000000000000000010000000000000123456789abcdef";
// $ echo foo > reapi-adapted
// $ sha256sum reapi-adapted #=> b5bb9d8014a0f9b1d61e21e796d78dccdf1352f23cd32812f4850b878ae4944c
// $ nix store add_file reapi-adapted # To ensure the test succeeds
const FOO_HASH: &str = "b5bb9d8014a0f9b1d61e21e796d78dccdf1352f23cd32812f4850b878ae4944c";
const BASH_STORE_PATH: &str = "/nix/store/lw117lsr8d585xs63kx5k233impyrq7q-bash-5.3p3";

#[nativelink_test]
async fn insert_simple() -> Result<(), Error> {
    let store = NixStore::new(
        &(NixSpec {
            socket_path: Some(SOCKET_PATH.to_string()),
            ..Default::default()
        }),
    )
    .await?;
    store
        .update_oneshot(
            StoreKey::Digest(DigestInfo::try_new(FOO_HASH, 64)?),
            "foo\n".into(),
        )
        .await
}

#[nativelink_test]
async fn read_simple() -> Result<(), Error> {
    let store = NixStore::new(
        &(NixSpec {
            socket_path: Some(SOCKET_PATH.to_string()),
            ..Default::default()
        }),
    )
    .await?;
    assert_eq!(
        Ok("foo\n".into()),
        store
            .get_part_unchunked(
                StoreKey::Digest(DigestInfo::try_new(FOO_HASH, 64)?),
                0,
                None
            )
            .await
    );
    Ok(())
}

#[nativelink_test]
async fn read_part() -> Result<(), Error> {
    let store = NixStore::new(
        &(NixSpec {
            socket_path: Some(SOCKET_PATH.to_string()),
            ..Default::default()
        }),
    )
    .await?;
    assert_eq!(
        Ok("oo".into()),
        store
            .get_part_unchunked(
                StoreKey::Digest(DigestInfo::try_new(FOO_HASH, 64)?),
                1,
                Some(2)
            )
            .await
    );
    Ok(())
}

#[nativelink_test]
async fn simple_has_object_found() -> Result<(), Error> {
    let store = NixStore::new(
        &(NixSpec {
            socket_path: Some(SOCKET_PATH.to_string()),
            ..Default::default()
        }),
    )
    .await?;
    let digest = DigestInfo::try_new(FOO_HASH, 100).unwrap();
    let result = store.has(digest).await;
    assert_eq!(
        result,
        Ok(Some(4)),
        "Expected to find item, got: {result:?}"
    );
    Ok(())
}

#[nativelink_test]
async fn simple_has_object_not_found() -> Result<(), Error> {
    let store = NixStore::new(
        &(NixSpec {
            socket_path: Some(SOCKET_PATH.to_string()),
            ..Default::default()
        }),
    )
    .await?;
    let digest = DigestInfo::try_new(VALID_HASH1, 100).unwrap();
    let result = store.has(digest).await;
    assert_eq!(
        result,
        Ok(None),
        "Expected to not find item, got: {result:?}"
    );
    Ok(())
}

#[nativelink_test]
async fn query_path_info_found() -> Result<(), Error> {
    let store = NixStore::new(
        &(NixSpec {
            socket_path: Some(SOCKET_PATH.to_string()),
            ..Default::default()
        }),
    )
    .await?;
    let connection = store.connection();
    let result = connection.query_path_info(BASH_STORE_PATH).await?;
    assert!(
        result.is_some(),
        "Expected to find path info for bash, got None"
    );
    let info = result.unwrap();
    dbg!(&info);
    assert!(
        info.nar_size > 0,
        "Expected non-zero nar_size, got {}",
        info.nar_size
    );
    Ok(())
}

#[nativelink_test]
async fn roundtrip() -> Result<(), Error> {
    let store = NixStore::new(
        &(NixSpec {
            socket_path: Some(SOCKET_PATH.to_string()),
            ..Default::default()
        }),
    )
    .await?;
    let text = "foobalabala";
    let key = StoreKey::Digest({
        let mut hasher = DigestHasherFunc::Sha256.hasher();
        hasher.update(text.as_bytes());
        hasher.finalize_digest()
    });
    store.update_oneshot(key.borrow(), text.into()).await?;

    assert_eq!(
        Ok(text.into()),
        store.get_part_unchunked(key.borrow(), 0, None).await
    );
    Ok(())
}
