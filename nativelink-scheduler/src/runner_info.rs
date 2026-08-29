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

    /// Extra `/nix/store/…` paths to make available in every reapi-action
    /// sandbox, from `NIXCEPTION_EXTRA_SANDBOX_PATHS` (colon-separated).
    /// Merged into each action's discovered store paths and resolved the
    /// same way (deriver lookup + hash-derivation-modulo, or a plain input
    /// source) — see [`NixWorker::resolve_discovered_store_paths`].
    pub extra_sandbox_paths: Vec<StorePath<String>>,
}

/// The runner's output and `.drv` store paths, baked into the binary at
/// compile time via `env!()` (Cargo re-evaluates this automatically if the
/// variable changes — no `build.rs` needed).  There is no runtime knob for
/// these: exactly one nixception build is paired with exactly one runner
/// build, wired together by packaging (the nixpkgs `nixception` package, or
/// this repo's own flake), never by the user.
///
/// Compile-time (rather than runtime env vars) also means a missing/invalid
/// value is a build failure, not something discovered only when the server
/// first tries to schedule an action.
const RUNNER_OUT: &str = env!(
    "NIXCEPTION_RUNNER_OUT",
    "NIXCEPTION_RUNNER_OUT must be set at build time to the runner's store \
     path (e.g. by the nixpkgs `nixception` package.nix, which builds the \
     runner as passthru.runner and sets this via `env.*`)."
);
const RUNNER_DRV: &str = env!(
    "NIXCEPTION_RUNNER_DRV",
    "NIXCEPTION_RUNNER_DRV must be set at build time to the runner's .drv \
     store path, alongside NIXCEPTION_RUNNER_OUT."
);

impl RunnerInfo {
    /// Obtain runner metadata: the runner's own paths are fixed at compile
    /// time ([`RUNNER_OUT`]/[`RUNNER_DRV`]); the hash-derivation-modulo is
    /// computed now by reading `.drv` files from the store (which may not
    /// have existed yet at compile time), and the rest comes from the
    /// process environment.
    ///
    /// Environment variables read:
    ///
    /// - `NIXCEPTION_SYSTEM` — optional, defaults to the build-time target
    ///   architecture
    /// - `NIXCEPTION_EXTRA_SANDBOX_PATHS` — optional, colon-separated
    ///   `/nix/store/…` paths to add to every action sandbox; this one *is*
    ///   meant to vary per server invocation (deployment config), unlike the
    ///   runner identity
    pub fn discover() -> Result<Self, Error> {
        let system = std::env::var("NIXCEPTION_SYSTEM")
            .ok()
            .unwrap_or_else(|| default_system().to_string());
        let extra_sandbox_paths = std::env::var("NIXCEPTION_EXTRA_SANDBOX_PATHS")
            .ok()
            .map(|v| parse_extra_sandbox_paths(&v))
            .transpose()?
            .unwrap_or_default();

        let builder_path = format!("{RUNNER_OUT}/bin/runner");

        // Parse the .drv store path using nix-compat.
        let drv_store_path = StorePath::from_absolute_path(RUNNER_DRV.as_bytes()).map_err(|e| {
            make_err!(
                Code::InvalidArgument,
                "Bad runner .drv path ({RUNNER_DRV}): {e}"
            )
        })?;

        // Compute the hash derivation modulo by recursively walking .drv
        // files.  This terminates at fixed-output derivations (fetchurl,
        // bootstrap tarballs, etc.).
        let mut cache = HashMap::new();
        let hash_derivation_modulo = compute_hash_derivation_modulo(RUNNER_DRV, &mut cache)?;

        Ok(Self {
            builder_path,
            drv_store_path,
            hash_derivation_modulo,
            system,
            extra_sandbox_paths,
        })
    }
}

/// Parse `NIXCEPTION_EXTRA_SANDBOX_PATHS`: colon-separated `/nix/store/…`
/// paths. Empty segments (e.g. a trailing `:`) are ignored.
fn parse_extra_sandbox_paths(value: &str) -> Result<Vec<StorePath<String>>, Error> {
    value
        .split(':')
        .filter(|s| !s.is_empty())
        .map(|s| {
            StorePath::from_absolute_path(s.as_bytes()).map_err(|e| {
                make_err!(
                    Code::InvalidArgument,
                    "Bad path in NIXCEPTION_EXTRA_SANDBOX_PATHS ({s}): {e}"
                )
            })
        })
        .collect()
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

/// Map a logical store path (`/nix/store/...`) to the physical filesystem path
/// to read, honoring the optional `NIXCEPTION_STORE_ROOT` chroot-store root.
/// Identity (returns the input unchanged) when the root is unset — so behavior
/// is unchanged in a normal deployment.  Mirrors `NixStore::physical`.
pub(crate) fn physical_store_path(logical_abs_path: &str) -> String {
    match std::env::var("NIXCEPTION_STORE_ROOT") {
        Ok(root) if !root.is_empty() => {
            format!("{}{}", root.trim_end_matches('/'), logical_abs_path)
        }
        _ => logical_abs_path.to_string(),
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

    // Read and parse the .drv file.  `drv_abs_path` is the logical
    // `/nix/store/...` path (used as the cache key and for recursion); the
    // physical read is relocated under a chroot store when one is configured.
    let drv_bytes = std::fs::read(physical_store_path(drv_abs_path))
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
            extra_sandbox_paths: Vec::new(),
        }
    }
}
