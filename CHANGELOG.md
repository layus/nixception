# Changelog

All notable changes to nixception will be documented in this file.

The historical changelog of the NativeLink codebase this project is based on
is preserved in [CHANGELOG-nativelink.md](./CHANGELOG-nativelink.md).

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
