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

use std::collections::HashMap;

use nativelink_error::{Code, Error, make_err};
use nix_compat::derivation::Derivation;
use nix_compat::store_path::StorePath;

/// Everything nixception needs to construct action derivations that use the
/// runner as their builder.
///
/// Created once at startup from environment variables and then shared
/// (via `Arc`) with every [`NixWorker`](crate::nix_worker::NixWorker).
#[derive(Debug)]
pub struct RunnerInfo {
    /// Absolute path to the runner binary
    /// (e.g. `"/nix/store/…-runner/bin/runner"`).
    pub builder_path: String,

    /// The `.drv` store path of the runner derivation.
    pub drv_store_path: StorePath<String>,

    /// The derivation-hash-modulo of the runner `.drv`.
    /// Required by `Derivation::calculate_output_paths`.
    pub hash_derivation_modulo: [u8; 32],

    /// The Nix `system` string (e.g. `"x86_64-linux"`, `"aarch64-linux"`).
    pub system: String,
}

impl RunnerInfo {
    /// Discover runner metadata from environment variables set by the Nix
    /// wrapper and compute the derivation hash modulo by walking `.drv`
    /// files in the store.
    ///
    /// Expected environment variables (set by the wrapper script created
    /// in `flake.nix`):
    ///
    /// - `NIXCEPTION_RUNNER_OUT` — store path of the runner output,
    ///   e.g. `/nix/store/…-runner`
    /// - `NIXCEPTION_RUNNER_DRV` — store path of the runner `.drv`,
    ///   e.g. `/nix/store/…-runner.drv`
    /// - `NIXCEPTION_SYSTEM` — optional, defaults to the build-time
    ///   target architecture
    pub fn from_env() -> Result<Self, Error> {
        let runner_out = std::env::var("NIXCEPTION_RUNNER_OUT").map_err(|_| {
            make_err!(
                Code::InvalidArgument,
                "NIXCEPTION_RUNNER_OUT not set. \
                 Use the nixceptionWrapped package or set the variable manually."
            )
        })?;
        let runner_drv = std::env::var("NIXCEPTION_RUNNER_DRV").map_err(|_| {
            make_err!(
                Code::InvalidArgument,
                "NIXCEPTION_RUNNER_DRV not set. \
                 Use the nixceptionWrapped package or set the variable manually."
            )
        })?;
        let system =
            std::env::var("NIXCEPTION_SYSTEM").unwrap_or_else(|_| default_system().to_string());

        let builder_path = format!("{runner_out}/bin/runner");

        // Parse the .drv store path using nix-compat.
        let drv_store_path = StorePath::from_absolute_path(runner_drv.as_bytes()).map_err(|e| {
            make_err!(
                Code::InvalidArgument,
                "Bad NIXCEPTION_RUNNER_DRV ({runner_drv}): {e}"
            )
        })?;

        // Compute the hash derivation modulo by recursively walking .drv
        // files.  This terminates at fixed-output derivations (fetchurl,
        // bootstrap tarballs, etc.).
        let mut cache = HashMap::new();
        let hash_derivation_modulo = compute_hash_derivation_modulo(&runner_drv, &mut cache)?;

        Ok(Self {
            builder_path,
            drv_store_path,
            hash_derivation_modulo,
            system,
        })
    }
}

/// Return the Nix system string for the current compilation target.
fn default_system() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x86_64-linux"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64-linux"
    } else {
        "x86_64-linux"
    }
}

/// Recursively compute the derivation-hash-modulo for the `.drv` at
/// `drv_abs_path` by reading it and all its input `.drv` files from
/// `/nix/store/`.
///
/// Results are memoised in `cache` so each `.drv` is read at most once.
///
/// Because [`Derivation::hash_derivation_modulo`] requires a `Fn` closure
/// (not `FnMut`), we pre-compute hashes for all direct input derivations
/// *before* calling it, so the closure only needs shared (`&`) access to
/// the cache.
///
/// This function is `pub(crate)` so that
/// [`NixWorker`](crate::nix_worker::NixWorker) can compute hashes for
/// dynamically-discovered input derivations at action time (e.g. store
/// paths referenced in command arguments or environment variables that
/// were resolved by rules_nixpkgs).
pub(crate) fn compute_hash_derivation_modulo(
    drv_abs_path: &str,
    cache: &mut HashMap<String, [u8; 32]>,
) -> Result<[u8; 32], Error> {
    // Return cached result if available.
    if let Some(hash) = cache.get(drv_abs_path) {
        return Ok(*hash);
    }

    // Read and parse the .drv file.
    let drv_bytes = std::fs::read(drv_abs_path)
        .map_err(|e| make_err!(Code::Internal, "Reading {drv_abs_path}: {e}"))?;
    let drv = Derivation::from_aterm_bytes(&drv_bytes)
        .map_err(|e| make_err!(Code::Internal, "Parsing {drv_abs_path}: {e:?}"))?;

    // Pre-compute hashes for every direct input derivation so we can
    // hand the closure an immutable reference to `cache`.
    let input_abs_paths: Vec<String> = drv
        .input_derivations
        .keys()
        .map(|sp| sp.to_absolute_path())
        .collect();
    for input_abs_path in &input_abs_paths {
        compute_hash_derivation_modulo(input_abs_path, cache)?;
    }

    // Now every input is in `cache`.  The closure only reads.
    let hash = drv.hash_derivation_modulo(|input_drv_store_path| {
        let input_abs_path = input_drv_store_path.to_absolute_path();
        *cache
            .get(&input_abs_path)
            .unwrap_or_else(|| panic!("BUG: input hash for {input_abs_path} not pre-computed"))
    });

    cache.insert(drv_abs_path.to_string(), hash);
    Ok(hash)
}

// ---------------------------------------------------------------------------
// Test helper
// ---------------------------------------------------------------------------

impl RunnerInfo {
    /// A dummy instance for tests that don't actually build derivations.
    ///
    /// All paths are synthetic and will not resolve in a real Nix store,
    /// but the struct is valid enough for scheduler state-management tests
    /// that never call `prepare_derivation`.
    pub fn dummy() -> Self {
        Self {
            builder_path: "/nix/store/00000000000000000000000000000000-dummy-runner/bin/runner"
                .into(),
            drv_store_path: StorePath::from_bytes(
                b"00000000000000000000000000000000-dummy-runner.drv",
            )
            .expect("valid dummy store path"),
            hash_derivation_modulo: [0u8; 32],
            system: "x86_64-linux".into(),
        }
    }
}
