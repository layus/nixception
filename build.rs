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

//! Bakes a git-derived version string into the crate as `NIXCEPTION_VERSION`,
//! read at runtime by the `nixception` binary's `--version` flag.
//!
//! Sources are tried in order, first hit wins:
//!   1. `$NIXCEPTION_VERSION`  — explicit override. The Nix package sets this,
//!      because it builds from a `fetchFromGitHub` tarball with no `.git` dir.
//!   2. `git describe`         — local dev checkouts. On a tag: `v0.4.0`.
//!      Off-tag: `v0.4.0+345.abcdef1` (345 commits since the tag, short hash).
//!      A dirty tree appends `.dirty`.
//!   3. `CARGO_PKG_VERSION`    — last-resort fallback (e.g. `v0.7.0`).

use std::process::Command;

fn main() {
    let version = version_from_env()
        .or_else(version_from_git)
        .unwrap_or_else(|| format!("v{}", env!("CARGO_PKG_VERSION")));

    println!("cargo:rustc-env=NIXCEPTION_VERSION={version}");

    // Re-run when the override or the git HEAD moves. `.git/HEAD` covers commits
    // and checkouts; `.git/index` covers staging (for the `.dirty` marker).
    println!("cargo:rerun-if-env-changed=NIXCEPTION_VERSION");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}

fn version_from_env() -> Option<String> {
    match std::env::var("NIXCEPTION_VERSION") {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
    }
}

/// Runs `git describe --tags --long --dirty` and reshapes its output into our
/// version scheme. Returns `None` if git is absent or there is no `.git` dir
/// (e.g. inside the Nix sandbox), so callers fall through to the next source.
fn version_from_git() -> Option<String> {
    let out = Command::new("git")
        .args(["describe", "--tags", "--long", "--dirty=.dirty"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8(out.stdout).ok()?;
    Some(reshape_describe(raw.trim()))
}

/// `v0.4.0-0-gc29477e8`         -> `v0.4.0`
/// `v0.4.0-345-gabcdef1`        -> `v0.4.0+345.abcdef1`
/// `v0.4.0-345-gabcdef1.dirty`  -> `v0.4.0+345.abcdef1.dirty`
/// Anything unexpected is returned unchanged.
fn reshape_describe(desc: &str) -> String {
    // Split off a trailing `.dirty` marker so it survives reshaping.
    let (core, dirty) = match desc.strip_suffix(".dirty") {
        Some(core) => (core, ".dirty"),
        None => (desc, ""),
    };

    // Expected `git describe --long` tail: `<tag>-<count>-g<hash>`.
    // rsplitn from the right so tags containing `-` stay intact.
    let mut parts = core.rsplitn(3, '-');
    let hash = parts.next(); // `g<hash>`
    let count = parts.next(); // `<count>`
    let tag = parts.next(); // `<tag>` (may itself contain `-`)

    match (tag, count, hash) {
        (Some(tag), Some(count), Some(hash)) => {
            let hash = hash.strip_prefix('g').unwrap_or(hash);
            if count == "0" {
                // Exactly on a tag: clean version, plus dirty marker if any.
                format!("{tag}{dirty}")
            } else {
                format!("{tag}+{count}.{hash}{dirty}")
            }
        }
        // Not in the expected shape — hand it back untouched.
        _ => desc.to_string(),
    }
}
