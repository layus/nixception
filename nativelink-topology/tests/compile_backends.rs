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

//! Compile-only coverage for the backend-gated `topology!` store arms.
//!
//! `Aws` / `Gcs` / `Redis` / `OntapS3` are only available when the matching
//! `nativelink-store` feature is on (`nativelink-topology`'s `s3` / `gcs` /
//! `redis`), so this test carries `required-features = ["all-backends"]` in
//! `Cargo.toml` and is skipped by a default `cargo test`. See `compile.rs`
//! for the feature-independent arms. (`Mongo` and `OntapS3ExistenceCache`
//! have no `Default` spec and were not covered by the original test
//! either.)

use nativelink_topology::topology;

/// Exercises every backend-gated store arm in a single invocation.
#[expect(dead_code, reason = "compiled, never executed")]
async fn compile_backend_arms() -> Result<(), nativelink_error::Error> {
    let _topology = topology! {
        stores {
            nix = Nix { socket_path: None, ..Default::default() },
            redis = Redis { ..Default::default() },
            aws = Aws { ..Default::default() },
            gcs = Gcs { ..Default::default() },
            ontap = OntapS3 { ..Default::default() },
        }
        schedulers {
            nix_proxy = NixProxy { ac_store: redis, cas_store: nix },
        }
    };
    Ok(())
}

#[test]
fn it_compiles() {}
