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

//! Compile-only coverage for every `topology!` arm.
//!
//! The functions below are never executed — they exist purely so the
//! compiler type-checks every store and scheduler arm of the macro (the
//! arms are only validated when expanded). Building this test crate also
//! confirms the hermetic `$crate::__rt` paths resolve from an external
//! crate that imports nothing but the macro itself.

use nativelink_topology::topology;

/// Exercises every store and scheduler arm in a single invocation.
#[expect(dead_code, reason = "compiled, never executed")]
async fn compile_all_arms() -> Result<(), nativelink_error::Error> {
    let _topology = topology! {
        stores {
            // Leaf stores (children for the wrappers below).
            mem = Memory { eviction_policy: None },
            nop = Noop,
            nix = Nix { socket_path: None },
            redis = Redis { ..Default::default() },
            reference = Ref { ..Default::default() },
            aws = Aws { ..Default::default() },
            gcs = Gcs { ..Default::default() },
            ontap = OntapS3 { ..Default::default() },
            // Wrapper stores referencing the leaves by name.
            verify           = Verify { backend: mem, verify_size: false, verify_hash: false },
            existence        = ExistenceCache { backend: mem, eviction_policy: None },
            fast_slow        = FastSlow {
                fast: mem,
                slow: nop,
                fast_direction: Default::default(),
                slow_direction: Default::default(),
            },
            size_partition   = SizePartitioning { lower_store: mem, upper_store: nop, size: 0u64 },
            completeness     = CompletenessChecking { backend: mem, cas_store: nop },
            shard            = Shard { stores: [mem, nop] },
        }
        schedulers {
            simple    = Simple { ..Default::default() },
            nix_proxy = NixProxy { ac_store: mem, cas_store: nix },
            cache     = CacheLookup { ac_store: mem, scheduler: Simple { ..Default::default() } },
            modifier  = PropertyModifier {
                scheduler: Simple { ..Default::default() },
                modifications: vec![],
            },
        }
    };
    Ok(())
}

/// Exercises every `services` arm.
#[expect(dead_code, reason = "compiled, never executed")]
async fn compile_all_services() -> Result<(), nativelink_error::Error> {
    let _topology = topology! {
        stores {
            mem = Memory { eviction_policy: None },
            nop = Noop,
            nix = Nix { socket_path: None },
        }
        schedulers {
            nix_scheduler = NixProxy { ac_store: nop, cas_store: nix },
        }
        services {
            cas:          Cas { cas_store: mem },
            ac:           Ac { ac_store: nop, read_only: false },
            execution:    Execution { cas_store: mem, scheduler: nix_scheduler },
            capabilities: Capabilities { scheduler: nix_scheduler },
            bytestream:   ByteStream {
                cas_store: mem,
                max_bytes_per_stream: 0,
                persist_stream_on_disconnect_timeout: 0,
            },
        }
    };
    Ok(())
}

/// Exercises the optional `clock:` parameter with a non-capturing mock
/// clock (the scheduler factories take a bare `fn() -> SystemTime`, so the
/// clock must be a free function, not a state-capturing closure).
#[expect(dead_code, reason = "compiled, never executed")]
async fn compile_with_mock_clock() -> Result<(), nativelink_error::Error> {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn mock_now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    let _topology = topology! {
        clock: mock_now;
        stores {
            nop = Noop,
        }
        schedulers {
            simple = Simple { ..Default::default() },
        }
    };
    Ok(())
}

#[test]
fn it_compiles() {}
