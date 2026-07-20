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

use nativelink_error::{Code, Error, ResultExt, make_err};
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
    /// Obtain runner metadata, preferring pre-built runner paths supplied via
    /// the environment and otherwise building the runner ourselves through the
    /// recursive-nix daemon.
    ///
    /// The `socket_path` is the Nix daemon socket (from the `NixStore`); it is
    /// only used for the self-build path.
    pub fn discover(socket_path: &str) -> Result<Self, Error> {
        match Self::try_from_env()? {
            Some(info) => Ok(info),
            None => Self::build_with_nix(socket_path)
                .err_tip(|| "Building the nixception runner via recursive-nix"),
        }
    }

    /// Discover runner metadata from environment variables, if they are set.
    ///
    /// Returns `Ok(None)` when neither `NIXCEPTION_RUNNER_OUT` nor
    /// `NIXCEPTION_RUNNER_DRV` is set (the caller should then build the runner
    /// itself). Returns `Err` when the variables are set but malformed, or
    /// only one of the pair is present.
    ///
    /// Expected environment variables (set by the setup hook or a dev shell):
    ///
    /// - `NIXCEPTION_RUNNER_OUT` — store path of the runner output,
    ///   e.g. `/nix/store/…-runner`
    /// - `NIXCEPTION_RUNNER_DRV` — store path of the runner `.drv`,
    ///   e.g. `/nix/store/…-runner.drv`
    /// - `NIXCEPTION_SYSTEM` — optional, defaults to the build-time
    ///   target architecture
    pub fn try_from_env() -> Result<Option<Self>, Error> {
        let runner_out = std::env::var("NIXCEPTION_RUNNER_OUT").ok();
        let runner_drv = std::env::var("NIXCEPTION_RUNNER_DRV").ok();

        match (runner_out, runner_drv) {
            // Neither set: signal the caller to build the runner itself.
            (None, None) => Ok(None),
            (Some(runner_out), Some(runner_drv)) => {
                let system = std::env::var("NIXCEPTION_SYSTEM").ok();
                Self::from_paths(&runner_out, &runner_drv, system).map(Some)
            }
            // Exactly one set: almost certainly a misconfiguration.
            (Some(_), None) => Err(make_err!(
                Code::InvalidArgument,
                "NIXCEPTION_RUNNER_OUT is set but NIXCEPTION_RUNNER_DRV is not. \
                 Set both, or neither to build the runner automatically."
            )),
            (None, Some(_)) => Err(make_err!(
                Code::InvalidArgument,
                "NIXCEPTION_RUNNER_DRV is set but NIXCEPTION_RUNNER_OUT is not. \
                 Set both, or neither to build the runner automatically."
            )),
        }
    }

    /// Construct a [`RunnerInfo`] from the runner's output and `.drv` store
    /// paths, computing the derivation-hash-modulo by walking `.drv` files in
    /// the store.
    ///
    /// `system` defaults to the build-time target architecture when `None`.
    fn from_paths(
        runner_out: &str,
        runner_drv: &str,
        system: Option<String>,
    ) -> Result<Self, Error> {
        let system = system.unwrap_or_else(|| default_system().to_string());

        let builder_path = format!("{runner_out}/bin/runner");

        // Parse the .drv store path using nix-compat.
        let drv_store_path = StorePath::from_absolute_path(runner_drv.as_bytes()).map_err(|e| {
            make_err!(
                Code::InvalidArgument,
                "Bad runner .drv path ({runner_drv}): {e}"
            )
        })?;

        // Compute the hash derivation modulo by recursively walking .drv
        // files.  This terminates at fixed-output derivations (fetchurl,
        // bootstrap tarballs, etc.).
        let mut cache = HashMap::new();
        let hash_derivation_modulo = compute_hash_derivation_modulo(runner_drv, &mut cache)?;

        Ok(Self {
            builder_path,
            drv_store_path,
            hash_derivation_modulo,
            system,
        })
    }
}

// ---------------------------------------------------------------------------
// Self-build via recursive-nix
// ---------------------------------------------------------------------------

/// The runner sources, bundled into the binary so the server can build the
/// runner without any external files.  `nativelink/tools/` is the canonical
/// copy (the nixpkgs packaging mirror may drift independently).
const RUNNER_NIX: &str = include_str!("../../tools/runner.nix");
const RUNNER_CPP: &str = include_str!("../../tools/runner.cpp");

/// Default nixpkgs used to build the runner when `NIXCEPTION_NIXPKGS` is not
/// set.  Pinned to the same revision as the flake's `nixpkgs` input so the
/// self-built runner matches the one the flake's own checks build.
const DEFAULT_NIXPKGS_URL: &str =
    "https://github.com/NixOS/nixpkgs/archive/8c441601c43232976179eac52dde704c8bdf81ed.tar.gz";
const DEFAULT_NIXPKGS_SHA256: &str = "16r7z12kmznnbw7w1fq39f3n0g8a6mqnny2y73m7qivjxilfcqxb";

impl RunnerInfo {
    /// Build the runner ourselves through the recursive-nix daemon and return
    /// its metadata.
    ///
    /// The bundled `runner.nix` / `runner.cpp` are written to a private
    /// temporary directory and built with the `nix` CLI, pointed at the
    /// recursive-nix daemon via `NIX_REMOTE=unix://<socket_path>`. nixpkgs is
    /// taken from `NIXCEPTION_NIXPKGS` when set (a path or flake-style
    /// reference usable as `import <ref> {}`), otherwise from a pinned
    /// `fetchTarball` matching the flake's nixpkgs.
    pub fn build_with_nix(socket_path: &str) -> Result<Self, Error> {
        // Write the bundled sources to a private temp dir. runner.nix does
        // `src = ./runner.cpp`, so the two files must sit side by side.
        let tmp = std::env::temp_dir().join(format!("nixception-runner-{}", std::process::id()));
        std::fs::create_dir_all(&tmp)
            .map_err(|e| make_err!(Code::Internal, "Creating runner build dir: {e}"))?;
        std::fs::write(tmp.join("runner.nix"), RUNNER_NIX)
            .map_err(|e| make_err!(Code::Internal, "Writing runner.nix: {e}"))?;
        std::fs::write(tmp.join("runner.cpp"), RUNNER_CPP)
            .map_err(|e| make_err!(Code::Internal, "Writing runner.cpp: {e}"))?;

        let runner_nix_path = tmp.join("runner.nix");
        let runner_nix_path = runner_nix_path.to_string_lossy();

        // The nixpkgs to build against. When NIXCEPTION_NIXPKGS is set we
        // import it directly (a store path, a channel path, or anything else
        // `import` accepts); otherwise fetch the pinned tarball purely.
        let nixpkgs_expr = match std::env::var("NIXCEPTION_NIXPKGS") {
            Ok(reference) if !reference.is_empty() => format!("import ({reference}) {{}}"),
            _ => format!(
                "import (builtins.fetchTarball {{ \
                     url = \"{DEFAULT_NIXPKGS_URL}\"; \
                     sha256 = \"{DEFAULT_NIXPKGS_SHA256}\"; \
                 }}) {{}}"
            ),
        };
        let expr = format!("({nixpkgs_expr}).callPackage {runner_nix_path} {{}}");

        // Build the runner and get its output path.
        //
        // `--impure` is required because the runner source lives in a temp dir
        // outside the store: pure evaluation forbids reading it (and forbids
        // `builtins.currentSystem`, needed by the default nixpkgs import). The
        // runner *build* itself remains deterministic — only source-path
        // resolution is impure — and the whole reccStdenv flow already runs
        // under `--impure` for the same reason.
        let out_output = std::process::Command::new("nix")
            .args([
                "build",
                "--no-link",
                "--print-out-paths",
                "--impure",
                "--extra-experimental-features",
                "nix-command",
                "--expr",
                &expr,
            ])
            .env("NIX_REMOTE", format!("unix://{socket_path}"))
            .output()
            .map_err(|e| make_err!(Code::Internal, "Spawning `nix build` for the runner: {e}"))?;
        let runner_out = check_nix_output(out_output, "nix build (runner)")?;

        // Get the runner's .drv path.
        let drv_output = std::process::Command::new("nix")
            .args([
                "path-info",
                "--derivation",
                "--extra-experimental-features",
                "nix-command",
                &runner_out,
            ])
            .env("NIX_REMOTE", format!("unix://{socket_path}"))
            .output()
            .map_err(|e| make_err!(Code::Internal, "Spawning `nix path-info` for the runner: {e}"))?;
        let runner_drv = check_nix_output(drv_output, "nix path-info --derivation (runner)")?;

        // Clean up the temp sources; ignore failures (they are harmless).
        drop(std::fs::remove_dir_all(&tmp));

        Self::from_paths(&runner_out, &runner_drv, None)
    }
}

/// Interpret the output of a `nix` invocation: require success, then return the
/// single trimmed line of stdout. Non-zero exit surfaces the captured stderr.
fn check_nix_output(output: std::process::Output, what: &str) -> Result<String, Error> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(make_err!(
            Code::Internal,
            "{what} failed ({}): {}",
            output.status,
            stderr.trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let path = stdout.trim();
    if path.is_empty() {
        return Err(make_err!(Code::Internal, "{what} produced no output path"));
    }
    // `--print-out-paths` can print several lines if there are multiple
    // outputs; the runner has a single `out`, so take the first line.
    Ok(path.lines().next().unwrap_or(path).trim().to_string())
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
