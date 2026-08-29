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

//! Standalone (outside-sandbox) integration tests for nixception.
//!
//! These prove that nixception works against an ordinary, isolated Nix daemon —
//! not just the in-sandbox recursive-nix path — and, crucially, let us inspect
//! the resulting store and REAPI behavior (which the derivation-based checks in
//! `workspace/checks` cannot).
//!
//! The harness:
//!   1. Creates a throwaway `TEST_ROOT` and a **chroot store** at
//!      `TEST_ROOT/store` (`local?root=…`), with an isolated daemon
//!      (state/db/socket/conf all under `TEST_ROOT`, `sandbox = false`).
//!   2. Launches a standalone `nixception` pointed at that daemon via
//!      `NIX_REMOTE=unix://…` and `NIXCEPTION_STORE_ROOT=TEST_ROOT/store`. The
//!      runner it uses is fixed at compile time (baked into the binary under
//!      test), so its closure is copied into the chroot store instead of
//!      being passed as an env var; the compiler toolchain is supplied via
//!      `NIXCEPTION_EXTRA_SANDBOX_PATHS`.
//!   3. Drives `recc <compiler> …` against `127.0.0.1:50051` and asserts on the
//!      chroot store contents.
//!
//! Tools (nix, recc, gcc, the runner, the nixception binary) come from the
//! `workspace#standalone-test-fixture` derivation; their locations are passed in
//! via env vars set by `just test-standalone`. The test is gated on those env
//! vars being present, so a plain `cargo test` (without the fixture) skips it.

use std::io::Write as _;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Env var the `just` recipe sets to the fixture's `bin` directory (nix,
/// nix-store, recc, gcc, g++). Its presence gates the whole suite.
const FIXTURE_BIN: &str = "NIXCEPTION_FIXTURE_BIN";
/// Env vars pointing at the pre-built runner out-path / drv-path.
const FIXTURE_RUNNER_OUT: &str = "NIXCEPTION_FIXTURE_RUNNER_OUT";
const FIXTURE_RUNNER_DRV: &str = "NIXCEPTION_FIXTURE_RUNNER_DRV";
/// Env var pointing at the nixception binary to test.
const FIXTURE_NIXCEPTION: &str = "NIXCEPTION_FIXTURE_NIXCEPTION";
/// Env var pointing at the REAL gcc store path (`/nix/store/…/bin/gcc`).  recc
/// must be invoked with this so nixception scans it as a store reference and
/// includes the compiler in the reapi-action sandbox — a symlink outside the
/// store would leave the compiler missing inside the runner sandbox.
const FIXTURE_GCC: &str = "NIXCEPTION_FIXTURE_GCC";
/// Env var holding the colon-separated toolset (gcc/binutils/coreutils) to
/// forward as `NIXCEPTION_EXTRA_SANDBOX_PATHS` to the server under test —
/// the runner itself has no built-in toolset, so every reapi-action sandbox
/// needs this to have a shell/linker/etc. available.
const FIXTURE_EXTRA_SANDBOX_PATHS: &str = "NIXCEPTION_FIXTURE_EXTRA_SANDBOX_PATHS";

/// Skip (with a printed note) unless the fixture env is present.
macro_rules! require_fixture {
    () => {{
        match Fixture::from_env() {
            Some(f) => f,
            None => {
                eprintln!(
                    "SKIP: standalone_recc — set {FIXTURE_BIN} etc. (run via \
                     `just test-standalone`)"
                );
                return;
            }
        }
    }};
}

struct Fixture {
    bin: PathBuf,
    runner_out: String,
    runner_drv: String,
    nixception: PathBuf,
    gcc: String,
    extra_sandbox_paths: String,
}

impl Fixture {
    fn from_env() -> Option<Self> {
        Some(Self {
            bin: PathBuf::from(std::env::var(FIXTURE_BIN).ok()?),
            runner_out: std::env::var(FIXTURE_RUNNER_OUT).ok()?,
            runner_drv: std::env::var(FIXTURE_RUNNER_DRV).ok()?,
            nixception: PathBuf::from(std::env::var(FIXTURE_NIXCEPTION).ok()?),
            gcc: std::env::var(FIXTURE_GCC).ok()?,
            extra_sandbox_paths: std::env::var(FIXTURE_EXTRA_SANDBOX_PATHS).ok()?,
        })
    }

    fn tool(&self, name: &str) -> PathBuf {
        self.bin.join(name)
    }
}

/// A running isolated daemon + standalone nixception server, torn down on drop.
struct TestEnv {
    root: PathBuf,
    store: PathBuf, // <root>/store — the chroot store root
    socket: PathBuf,
    daemon: Child,
    server: Child,
    fixture: Fixture,
}

impl TestEnv {
    /// Bring up the isolated daemon (chroot store) and the standalone server.
    fn start(fixture: Fixture) -> Self {
        // Unique throwaway root.  (No mktemp: keep it simple & inspectable.)
        // pid + a per-process counter keeps roots distinct across tests even
        // though the suite runs single-threaded.
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("nixception-e2e-{}-{}", std::process::id(), seq));
        let store = root.join("store");
        let state = root.join("var/nix");
        let logdir = root.join("var/log/nix");
        let confdir = root.join("etc");
        let home = root.join("home");
        let socket = root.join("daemon-socket");
        for d in [&store, &state, &logdir, &confdir, &home] {
            std::fs::create_dir_all(d).expect("mkdir test dirs");
        }

        // Minimal nix.conf: no sandbox (we are the isolated store), no
        // substituters, nix-command available.
        std::fs::write(
            confdir.join("nix.conf"),
            "build-users-group =\nsandbox = false\n\
             experimental-features = nix-command\nsubstituters =\n",
        )
        .expect("write nix.conf");

        let store_uri = format!("local?root={}", store.display());

        // Common env for every nix invocation against the isolated store.
        let nix_env = |cmd: &mut Command| {
            cmd.env("NIX_STATE_DIR", &state)
                .env("NIX_LOG_DIR", &logdir)
                .env("NIX_CONF_DIR", &confdir)
                .env("NIX_DAEMON_SOCKET_PATH", &socket)
                .env("HOME", &home)
                .env_remove("NIX_REMOTE");
        };

        // Initialise the chroot store's database.
        let mut init = Command::new(fixture.tool("nix-store"));
        nix_env(&mut init);
        let ok = init
            .args(["--init", "--store", &store_uri])
            .status()
            .expect("spawn nix-store --init")
            .success();
        assert!(ok, "nix-store --init failed");

        // Copy the runner's closure (out + drv), plus the extra-sandbox-paths
        // toolset (gcc/binutils/coreutils), from the real store into the
        // chroot store.  The reapi-action derivations reference all of these,
        // so they must be present in the test store for the daemon to build
        // an action.  The copy reads via the real system daemon (`--from
        // daemon`) — it must NOT inherit the isolated NIX_STATE_DIR, which
        // points at the empty chroot db.
        let extra_sandbox_path_list: Vec<&str> = fixture
            .extra_sandbox_paths
            .split(':')
            .filter(|s| !s.is_empty())
            .collect();
        for src in [&fixture.runner_out, &fixture.runner_drv]
            .into_iter()
            .map(String::as_str)
            .chain(extra_sandbox_path_list.iter().copied())
        {
            let ok = Command::new(fixture.tool("nix"))
                .env("HOME", &home)
                .args([
                    "--extra-experimental-features",
                    "nix-command",
                    "copy",
                    "--no-check-sigs",
                    "--from",
                    "daemon",
                    "--to",
                    &store_uri,
                    src,
                ])
                .status()
                .expect("spawn nix copy")
                .success();
            assert!(ok, "nix copy {src} into the chroot store failed");
        }

        // Start the daemon serving the chroot store.
        let mut dcmd = Command::new(fixture.tool("nix"));
        nix_env(&mut dcmd);
        let daemon = dcmd
            .args([
                "--extra-experimental-features",
                "nix-command",
                "daemon",
                "--store",
                &store_uri,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn nix daemon");
        wait_for(Duration::from_secs(30), || socket.exists());
        assert!(socket.exists(), "daemon socket never appeared");

        // Launch the standalone nixception server against the test daemon.
        let mut scmd = Command::new(&fixture.nixception);
        // NIXCEPTION_RUNNER_OUT/_DRV are compile-time constants baked into
        // the binary (see runner_info.rs) — not set here. fixture.runner_out
        // /_drv are still used above, to copy the runner's closure into the
        // chroot store the daemon serves.
        scmd.env("NIX_REMOTE", format!("unix://{}", socket.display()))
            .env("NIXCEPTION_STORE_ROOT", &store)
            .env(
                "NIXCEPTION_EXTRA_SANDBOX_PATHS",
                &fixture.extra_sandbox_paths,
            )
            .env(
                "NIXCEPTION_LOG",
                std::env::var("NIXCEPTION_E2E_LOG").unwrap_or_else(|_| "warn".into()),
            )
            .env("NIX_STATE_DIR", &state)
            .env("NIX_CONF_DIR", &confdir)
            .env("HOME", &home)
            .stdout(Stdio::null())
            // Inherit stderr so server logs surface with --nocapture when
            // NIXCEPTION_E2E_LOG is set; otherwise keep quiet.
            .stderr(if std::env::var("NIXCEPTION_E2E_LOG").is_ok() {
                Stdio::inherit()
            } else {
                Stdio::null()
            });
        let server = scmd.spawn().expect("spawn nixception");
        wait_for(Duration::from_secs(30), || {
            TcpStream::connect("127.0.0.1:50051").is_ok()
        });
        assert!(
            TcpStream::connect("127.0.0.1:50051").is_ok(),
            "nixception never listened on 127.0.0.1:50051"
        );

        Self {
            root,
            store,
            socket,
            daemon,
            server,
            fixture,
        }
    }

    /// Physical `nix/store` dir inside the chroot store.
    fn store_dir(&self) -> PathBuf {
        self.store.join("nix/store")
    }

    /// Names of store entries matching `-reapi-action` (the per-action outputs),
    /// excluding `.drv` files.
    fn reapi_action_outputs(&self) -> Vec<String> {
        let mut out = vec![];
        if let Ok(rd) = std::fs::read_dir(self.store_dir()) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.ends_with("-reapi-action") {
                    out.push(name);
                }
            }
        }
        out.sort();
        out
    }

    /// Run `recc <real-gcc> <args…>` in `cwd`, pointed at the server. The
    /// compiler is the real `/nix/store/…/bin/gcc` so nixception scans it and
    /// makes it available in the reapi-action sandbox. Returns success.
    fn recc(&self, cwd: &Path, args: &[&str]) -> bool {
        let mut cmd = Command::new(self.fixture.tool("recc"));
        cmd.current_dir(cwd)
            .arg(&self.fixture.gcc)
            .args(args)
            // Point recc at the standalone server (mirrors workspace/checks).
            .env("RECC_SERVER", "127.0.0.1:50051")
            .env("RECC_CAS_SERVER", "127.0.0.1:50051")
            .env("RECC_ACTION_CACHE_SERVER", "127.0.0.1:50051")
            .env("RECC_INSTANCE", "main")
            .env("RECC_PROJECT_ROOT", cwd)
            // Make the fixture toolchain visible to recc's dep scan.
            .env("PATH", self.fixture.bin.display().to_string());
        cmd.status().map(|s| s.success()).unwrap_or(false)
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        drop(self.server.kill());
        drop(self.server.wait());
        drop(self.daemon.kill());
        drop(self.daemon.wait());
        // Nix store files are read-only (0444); make everything writable before
        // removing so `remove_dir_all` doesn't fail on the chroot store.
        make_writable_recursive(&self.root);
        drop(std::fs::remove_dir_all(&self.root));
        let _socket = &self.socket; // keep field used
    }
}

/// Recursively add owner-write to every file/dir under `path` so a read-only
/// Nix store can be deleted.  Best-effort.
fn make_writable_recursive(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }
    if let Ok(perms) = std::fs::metadata(path).map(|m| m.permissions()) {
        let mut perms = perms;
        perms.set_mode(perms.mode() | 0o200);
        drop(std::fs::set_permissions(path, perms));
    }
    if meta.is_dir() {
        if let Ok(rd) = std::fs::read_dir(path) {
            for e in rd.flatten() {
                make_writable_recursive(&e.path());
            }
        }
    }
}

/// Poll `cond` until true or `timeout` elapses.
fn wait_for(timeout: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Write a trivial C source into `dir/<name>.c` and return its path.
fn write_c(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(format!("{name}.c"));
    let mut f = std::fs::File::create(&p).expect("write source");
    f.write_all(body.as_bytes()).expect("write source");
    p
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn compile_lands_in_store() {
    let fixture = require_fixture!();
    let env = TestEnv::start(fixture);

    // The store starts with no reapi-action outputs.
    assert!(
        env.reapi_action_outputs().is_empty(),
        "store should start empty of reapi-action outputs"
    );

    let work = env.root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    write_c(&work, "foo", "int foo(void){return 42;}\n");

    let ok = env.recc(&work, &["-c", "foo.c", "-o", "foo.o"]);
    assert!(ok, "recc gcc -c foo.c failed");

    // The action's output landed in the chroot store as a -reapi-action path.
    let outputs = env.reapi_action_outputs();
    assert!(
        !outputs.is_empty(),
        "expected a -reapi-action output in the store, found none"
    );

    // And the object file exists and is non-empty.
    let obj = work.join("foo.o");
    let meta = std::fs::metadata(&obj).expect("foo.o should exist");
    assert!(meta.len() > 0, "foo.o is empty");
}

#[test]
fn distinct_actions_distinct_store_paths() {
    let fixture = require_fixture!();
    let env = TestEnv::start(fixture);

    let work = env.root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    write_c(&work, "a", "int a(void){return 1;}\n");
    write_c(&work, "b", "int b(void){return 2;}\n");

    assert!(env.recc(&work, &["-c", "a.c", "-o", "a.o"]));
    let after_a = env.reapi_action_outputs();
    assert!(env.recc(&work, &["-c", "b.c", "-o", "b.o"]));
    let after_b = env.reapi_action_outputs();

    assert!(
        after_b.len() > after_a.len(),
        "a second, different compile should add a new -reapi-action path \
         (before={after_a:?}, after={after_b:?})"
    );
}

#[test]
fn identical_action_reuses_store_path() {
    let fixture = require_fixture!();
    let env = TestEnv::start(fixture);

    let work = env.root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    write_c(&work, "c", "int c(void){return 3;}\n");

    assert!(env.recc(&work, &["-c", "c.c", "-o", "c.o"]));
    let first = env.reapi_action_outputs();
    // Re-run the identical compile: the reapi-action derivation is already
    // built, so the store gains no new -reapi-action path (cache hit).
    assert!(env.recc(&work, &["-c", "c.c", "-o", "c.o"]));
    let second = env.reapi_action_outputs();

    assert_eq!(
        first, second,
        "an identical re-compile should not add a new -reapi-action path"
    );
}
