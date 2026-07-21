# nixception 0.4.0

This release makes nixception usable **outside a build sandbox** and adds a real
integration test suite around that.

## Highlights

- **Isolated store support.** `NIXCEPTION_STORE_ROOT` (or the `store_root`
  config field) relocates the server's store reads to `<root>/nix/store/...`, so
  nixception can run against an isolated Nix daemon whose physical store lives
  outside the real `/nix/store`. Paths on the wire and in derivations stay
  logical; it's off by default and inert when unset.

- **Standalone integration tests.** A new Rust suite spins up a throwaway chroot
  store + isolated daemon, points a standalone nixception at it, drives `recc`
  compiles, and asserts on the resulting store (a compile lands as a
  `-reapi-action` path, distinct compiles differ, an identical re-compile is a
  cache hit). This exercises the non-sandbox path and enables store-level
  assertions the derivation-based checks can't. Run with `just test-standalone`.

- **Quieter builds by default.** The recurring `nixception-hook: mem …` cgroup
  memory sampler is now off by default (set `NIXCEPTION_DEBUG_MEM=1` to
  re-enable); the on-failure OOM report still runs.

See the [changelog](./CHANGELOG.md) for the full list.

## Licensing

The nixception sources are licensed under **Apache-2.0**. Binary distributions
of the server are additionally governed by **GPL-3.0** through the statically
linked `nix-compat` crate — see [NOTICE](./NOTICE) for full attribution and
licensing details.
