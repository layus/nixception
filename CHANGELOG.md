# Changelog

All notable changes to nixception will be documented in this file.

The historical changelog of the NativeLink codebase this project is based on
is preserved in [CHANGELOG-nativelink.md](./CHANGELOG-nativelink.md).

## [0.6.0] - 2026-09-12

### Fixed

- **Discovered store paths are always added as `input_sources`, never
  resolved to a deriver.** Building an action's derivation used to query the
  Nix daemon for each discovered `/nix/store/…` path's deriver, and — when
  one was found — reference that deriver (as an `input_derivation`, with a
  computed `hash_derivation_modulo`) instead of the path itself. That
  precision was unreliable rather than merely unavailable: inside a
  `recursive-nix` sandboxed build (every real `nix build` action nixception
  prepares), the daemon it talks to is Nix's own `RestrictedStore`, which
  unconditionally strips the `deriver` field from every `queryPathInfo` reply
  as impure information — so a real build's actions always resolved every
  path as a plain source. Anything resolved from outside that sandbox (e.g.
  `nix develop`, which talks to the host daemon directly and does see real
  deriver info) could instead resolve some of those very same store paths to
  `input_derivations`, producing a derivation with a different structural
  shape — and hash — for what was otherwise a byte-identical action, so the
  cache never crossed the `nix build` / `nix develop` boundary for actions
  that referenced any such path (e.g. through `$PATH`). Every discovered path
  is now classified as an input source unconditionally: less precise (Nix can
  no longer distinguish "this exact build of gcc" from "any content-identical
  path already named `/nix/store/<hash>-gcc-...`"), but deterministic from
  any calling context, with no daemon round-trip involved.

## [0.5.0] - 2026-08-30

### Changed

- **The crate is now named `nixception`, not `nativelink`.** `Cargo.toml`'s
  `[package] name` matches the fork's only binary; the redundant `[[bin]]`
  section (and the upstream `nativelink` output it shadowed) are gone.
  `flake.nix`'s `packages.default`/`apps.default` build and run this binary
  directly — no more separate, silently-uncompilable `nativelink`-named alias.
- **The runner's identity is fixed at compile time, not passed at runtime.**
  `NIXCEPTION_RUNNER_OUT`/`NIXCEPTION_RUNNER_DRV` are now `env!()` constants
  baked into the binary by packaging (the nixpkgs `nixception` package builds
  the runner first, then sets these via `env.*`; this repo's own `flake.nix`
  does the same). There is no runtime way to point a built binary at a
  different runner, and no self-build fallback anymore (the old
  `RunnerInfo::build_with_nix`, which reconstructed the runner via `nix build`
  at server start-up, was untested dead code removed along with it — nothing
  ever exercised it, since the setup hook always supplied pre-built paths).
- **Extra sandbox tools go through `NIXCEPTION_EXTRA_SANDBOX_PATHS`.** A new,
  optional runtime env var (colon-separated `/nix/store/…` paths) makes
  packages available inside every REAPI action's runner sandbox. This replaces
  `nixceptionHook.withPackages`/`.override { extraRuntimeInputs = …; }`, which
  used to bake a custom package set into a dedicated runner build per caller;
  the runner itself carries no toolset opinion now — it never shells out to
  anything on its own.
- The integration checks (`recc-smoke-test`, `recc-hello`, `recc-spdlog`,
  `recc-nix`, `bazel-rules-nixpkgs-hello`, `bazel-abseil-cpp`,
  `protoc-gen-js-with-nixception`) now live in this repo, under `checks/`, and
  build against this repo's own local source (`nix flake check`, or
  `nix build .#checks.<system>.<name>` for one) instead of a nixpkgs release.

### Fixed

- **Actions that execute a tool built by an earlier remote action failed with
  `Permission denied`.** The runner's CAS-materialized inputs lost their
  executable bit — a fix for this landed in 0.4.0-era development
  (`625b1e7c`) but was silently reverted by a later "sync tools/runner.cpp up
  to the nixpkgs-fork version" commit, since that nixpkgs copy had never
  received the fix either. Restored; verified against
  `protoc-gen-js-with-nixception`, which needs its own remotely-built `protoc`
  to run a later action and was the check that failure broke.

## [0.4.0] - 2026-07-21

### Added

- chroot-store support: `NixSpec.store_root` (config) / `NIXCEPTION_STORE_ROOT`
  (env) relocate the server's own store *reads* to `<root>/nix/store/...`, so
  nixception can run against an isolated Nix daemon whose physical store lives
  outside the real `/nix/store` (paths on the wire and in derivations stay
  logical `/nix/store/...`). Off by default; unset behaves exactly as before.
- Standalone (outside-sandbox) integration tests
  (`nativelink-scheduler/tests/standalone_recc.rs`, run via
  `just test-standalone`): each spins up a throwaway chroot store + isolated Nix
  daemon, points a standalone nixception at it, drives `recc` compiles, and
  asserts on the resulting store (a compile lands as a `-reapi-action` path,
  distinct compiles differ, an identical re-compile is a cache hit). These
  exercise the non-sandbox path and enable store-level assertions the
  derivation-based checks can't.

### Changed

- The background cgroup memory sampler (the recurring `nixception-hook: mem …`
  lines) is now **off by default**. It was on by default while investigating the
  intermittent OOM SIGKILLs; set `NIXCEPTION_DEBUG_MEM=1` to re-enable it. The
  on-failure cgroup/OOM report still runs regardless.

## [0.3.0] - 2026-07-20

### Changed

- Reworked timing statistics to break down the previously opaque "command
  execution" span. The runner now measures its own **setup**, **task**
  (the command it runs), and **wrap-up** phases and writes them to
  `$out/timing.json`; the server reads them back and also derives the
  **nix→runner latency** — the time between requesting the build from the
  daemon and the runner actually starting inside the sandbox. The timing
  summary now nests these under "Command execution," and the per-action
  "Action timing breakdown" debug log carries the same split. Reading the
  runner's timing is best-effort, so builds whose runner predates this change
  still work.
- Fixed the mislabeled "Total wall-clock" line, which was actually the *sum* of
  every action's span and so overcounted under parallelism. The summary now
  reports the real elapsed **wall-clock** (server start → shutdown) separately
  from the renamed **cumulative action time**, and adds **parallelism**
  (cumulative / wall, average and peak) and **throughput** (actions per second).
  The execution / overhead percentages are now stated as a fraction of the
  cumulative time.
- Distinguish **cached vs executed** actions. An action is detected as a cache
  hit when its `$out/timing.json` is absent or stale (its recorded runner start
  predates the server's build request — the runner didn't run for this build).
  The summary reports the cache hit ratio, the **estimated time the cache
  saved** (the cached record's runtime plus a conservative 300 ms sandbox-setup
  floor, minus what the hit actually cost), and the resulting **cache speedup**.
- The timing summary is now split into three sections: **Preparation** (common
  to all actions — scanning, derivation prep, upload, output collection, and the
  sub-steps), **Execution** (executed actions only — the command-execution span
  and its runner phase breakdown, averaged over executed actions), and
  **Cached** (cache hits — average actual vs avoided cost and the speedup). This
  keeps cache hits from diluting the real-work averages; a fully-cached run
  shows an empty execution section.

## [0.2.1] - 2026-07-20

### Fixed

- The clean `nix build` (and therefore the release build) failed to compile
  after 0.2.0 because the flake's source filter dropped `tools/runner.nix` and
  `tools/runner.cpp`, which the scheduler embeds via `include_str!`. They're now
  kept in the build source. (0.2.0 produced no release artifacts as a result;
  0.2.1 is the first buildable release of this line.)

## [0.2.0] - 2026-07-20

### Added

- The server can now **build its own runner** via the recursive-nix daemon.
  When `NIXCEPTION_RUNNER_OUT` / `NIXCEPTION_RUNNER_DRV` aren't set, it builds
  the bundled `runner.nix` (embedded in the binary) with `nix build` against the
  daemon, instead of refusing to start. A bare `nixception` binary is therefore
  self-sufficient — the setup hook is no longer required just to supply the
  runner.
- `NIXCEPTION_NIXPKGS` environment variable: overrides the nixpkgs used to build
  the runner (a path or reference importable as `import <ref> {}`). Defaults to a
  pinned `nixos-unstable` tarball matching the flake's `nixpkgs` input, so the
  self-built runner is cache-compatible with the hook-built one.

### Changed

- Runner discovery now prefers the environment variables when both are set
  (unchanged behaviour for the setup hook and the `native-reccStdenv` dev shell)
  and only self-builds when they aren't set. Setting exactly one of the pair is
  now a clear configuration error.
- The startup error messages no longer reference the nonexistent
  `nixceptionWrapped` package.
- Much quieter default logging. Per-action and per-request operational messages
  are now logged at `debug` instead of `info`: the REAPI service handlers'
  full-response tracing (`CAS`, `AC`, `ByteStream`, `Capabilities`, and the
  others — via `#[instrument(ret)]`), derivation upload, build completion, output
  collection, store-path resolution, per-client connection, the periodic and
  peak in-flight gauges, the cumulative timing summary, and the `NixStore` upload
  trace. `info` is now reserved for lifecycle milestones: server ready,
  build-succeeded-after-retry, and orderly shutdown (SIGTERM handling, which was
  previously logged at `warn`). Set `NIXCEPTION_LOG=debug` to restore the
  per-action detail. The human-readable timing summary is unaffected — the setup
  hook still prints it from `NIXCEPTION_STATS_FILE`.
- The server log level is now configured with `NIXCEPTION_LOG` instead of
  `RUST_LOG` (same filter syntax; `NIXCEPTION_LOG` takes precedence when both are
  set). The setup hook sets `NIXCEPTION_LOG` accordingly.

## [0.1.1] - 2026-07-20

### Changed

- Release artifacts now contain only the `nixception` binary. The upstream
  `nativelink` binary is still built by the flake but is no longer attached to
  GitHub releases.

## [0.1.0] - 2026-07-20

Initial release.

### Added

- The `nixception` server binary: a NativeLink-based REAPI endpoint (CAS,
  Action Cache, Execution, Capabilities, ByteStream on `0.0.0.0:50051`) that
  translates every remote action into a Nix derivation and realises it
  through the recursive-nix daemon, using the Nix store as the
  content-addressed cache.
- `NixStore`: the Nix store used directly as CAS backing store.
- `NixScheduler` / `NixWorker`: store-path scanning of action inputs,
  derivation preparation with a minimal bash runner, execution via a pooled
  connection to the Nix daemon, and output collection.
- `NixceptionStats`: per-cost-center timing statistics, printed on shutdown.
- `nativelink-topology`: a `topology!` macro to assemble server topologies
  concisely (used by the nixception binary and in tests).
- A nixpkgs setup hook (`tools/nixception-hook.nix`) that starts the server
  before `configurePhase` inside a consuming derivation and tears it down on
  exit.

### Notes

- Based on the last Apache-2.0 licensed commit of
  [NativeLink](https://github.com/TraceMachina/nativelink)
  (`d39eeb62`, v0.7.0 lineage), with the full nixception development history
  rebased on top and the Rust dependency set brought up to date.
- Licensed under Apache-2.0; binary distributions are governed by GPLv3 via
  the statically linked `nix-compat` crate (see [NOTICE](./NOTICE)).
