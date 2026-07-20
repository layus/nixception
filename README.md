# nixception

[![License](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
[![Test](https://github.com/layus/nixception/actions/workflows/test.yaml/badge.svg)](https://github.com/layus/nixception/actions/workflows/test.yaml)

**nixception** turns ordinary build-tool actions into **Nix builds**, using the
Nix store as a content-addressed cache. It's a server that speaks the
[Remote Execution API](https://github.com/bazelbuild/remote-apis) (REAPI) and
translates each remote action it receives — a single `gcc` invocation sent by
[`recc`](https://gitlab.com/BuildGrid/recc), a [Bazel](https://bazel.build)
rule — into a Nix derivation, and builds it through the **recursive-nix** daemon.
Cached results live in the Nix store, so any action is built at most once
across all consumers.

```
 recc / bazel ──REAPI──▶  nixception server  ──recursive-nix──▶  /nix/store
 (compiler/rule)          (NixStore +                            (CAS + action
                           NixScheduler +                          cache)
                           NixWorker)
```

Because the Nix store is content-addressed, identical actions are built once
and reused across runs and across projects. The win is a shared, reproducible,
deduplicated cache for fine-grained build actions (individual compiles, Bazel
rules) — not just whole packages.

nixception is built on top of
[NativeLink](https://github.com/TraceMachina/nativelink), an efficient,
high-performance build cache and remote execution system. See
[Relationship to NativeLink](#relationship-to-nativelink--licensing) below for
how the two projects and their licenses relate.

> **Status**: experimental. Interfaces (server topology, setup hook contract,
> derivation encoding) are still moving.

## How it works

The `nixception` binary (`src/bin/nixception.rs`) is a NativeLink server
assembled from a custom topology:

- Exposes a REAPI gRPC endpoint on `0.0.0.0:50051` with the **CAS**, **AC**
  (action cache), **Execution**, **Capabilities** and **ByteStream** services.
- Backs them with a `NixStore` — the Nix store used directly as the
  content-addressed store — and a `NixScheduler`.

The translation logic lives in `nativelink-scheduler/src/`:

- **`nix_scheduler.rs`** — receives actions and manages a connection pool to
  the Nix daemon to avoid per-action overhead and daemon-connection deadlocks.
- **`nix_worker.rs`** — the heart of the translation. For each action it scans
  every input for `/nix/store/...` references to discover the action's real
  store dependencies, prepares a derivation that runs the action's command
  through a small bash *runner*, realises it via recursive-nix, and collects
  the outputs back to the client.
- **`nix_stats.rs`** — lock-free timing statistics per cost center (scanning,
  preparation, upload, execution, collection), printed on shutdown.

Because every action is realised as a derivation *from inside a Nix build*,
the server relies on **recursive-nix**: the ability of a build to talk back to
the Nix daemon (via `/build/.nix-socket`) and realise further derivations.

## Building

The flake uses git submodules (`vendor/`). On Nix ≥ 2.27 they're picked up
automatically:

```sh
nix build
./result/bin/nixception
```

On older Nix, pass the submodules flag explicitly:

```sh
nix build ".?submodules=1#"
```

A development shell with the pinned Rust toolchain is available through
`nix develop`, and plain `cargo build --release --bin nixception` works inside
it.

## Using it in a Nix build

The intended consumer interface is a nixpkgs **setup hook**
(`tools/nixception-hook.nix` + `tools/nixception-setup-hook.sh`). Added to
`nativeBuildInputs`, it starts the server before `configurePhase` and tears it
down when the build exits:

```nix
nativeBuildInputs = [ nixceptionHook ];
# or, to inject tools into the runner sandbox:
nativeBuildInputs = [ (nixceptionHook.withPackages [ myCompiler ]) ];
```

The consuming derivation needs `requiredSystemFeatures = [ "recursive-nix" ]`.
Useful environment variables:

- `NIXCEPTION_VERBOSE=1` — stream timestamped server output to stderr (by
  default the log is kept quiet and dumped only on failure).
- `NIXCEPTION_STATS_FILE` — where the server writes its timing summary.
- `NIXCEPTION_LOG` — log level / filter (same syntax as `RUST_LOG`); honored
  if set.

## Relationship to NativeLink & licensing

nixception is a friendly fork of
[NativeLink](https://github.com/TraceMachina/nativelink) by Trace Machina,
Inc. and the NativeLink authors. All credit for the underlying build-cache and
remote-execution infrastructure — the stores, schedulers, services and the
REAPI implementation this project is assembled from — belongs to them. If you
need a production-grade build cache or remote execution at scale, use
[NativeLink](https://github.com/TraceMachina/nativelink); this project serves
a different, Nix-specific niche.

### Licensing

- This repository is licensed under the
  **[Apache License 2.0](./LICENSE)**. It's based on the last Apache-2.0
  licensed commit of upstream NativeLink; the upstream copyright notices are
  preserved in the source headers, and attribution notices are collected in
  [NOTICE](./NOTICE).
- Upstream NativeLink has since moved to dual licensing under the
  **Functional Source License 1.1 with an Apache 2.0 future grant**
  (FSL-1.1-Apache-2.0): each upstream release converts to Apache 2.0 two
  years after its publication.
- Consequently, nixception tracks upstream **with a lag of two years**: an
  upstream change is only incorporated here once its FSL grace period has
  lapsed and it's available under Apache 2.0. Until then, this repository
  only carries the Apache-licensed base plus the nixception-specific work
  developed here.
- The vendored [`nix-compat`](./vendor/tvix) crate (from the tvix project) is
  **GPL-3.0** and is statically linked into the `nixception` executable, so
  **binary distributions of `nixception` are governed by the GPLv3** even
  though the code in this repository is Apache 2.0. See [NOTICE](./NOTICE)
  for details.

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md). Security reports: see
[SECURITY.md](./SECURITY.md).
