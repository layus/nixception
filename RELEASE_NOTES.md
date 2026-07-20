# nixception 0.2.1

nixception is a tool that turns REAPI build-tool actions into Nix builds. This
release makes a bare `nixception` binary self-sufficient — it now **builds its
own runner** through the recursive-nix daemon when the runner environment
variables aren't set, so it no longer refuses to start outside the setup hook.
Default logging is also much quieter (per-action and per-request chatter moved
to `debug`), and the server's log level is now configured with `NIXCEPTION_LOG`
instead of `RUST_LOG`.

0.2.1 fixes a build regression that prevented 0.2.0 from producing release
artifacts (the embedded runner sources were dropped by the flake source filter).
See the [changelog](./CHANGELOG.md) for the full list.

## What's nixception?

nixception turns ordinary build-tool actions into **Nix builds**, using the Nix
store as a content-addressed cache.

It's a server that speaks the **Remote Execution API** (REAPI) — the same
protocol used by [Bazel](https://bazel.build),
[`recc`](https://gitlab.com/BuildGrid/recc), and other build tools to offload
work to a remote executor. Instead of running each action on a pool of remote
workers, nixception translates every action it receives — a single compiler
invocation, a Bazel rule — into a **Nix derivation** and realises it through the
**recursive-nix** daemon.

```
 build tool ──REAPI──▶  nixception  ──recursive-nix──▶  /nix/store
 (bazel, recc, …)                                       (content-addressed cache)
```

Because the Nix store is content-addressed, identical actions are built exactly
**once** and reused across runs, across projects, and across machines that share
the store. The result is a shared, reproducible, deduplicated cache at the
granularity of individual build actions — not just whole packages.

Point any REAPI-speaking build tool at a nixception endpoint and its
fine-grained actions become cacheable Nix builds, with no change to the build
tool itself.

## Using it

- Build the server from source with Nix (`nix build`), or run it from the
  released static Linux binary attached below.
- The intended integration for Nix builds is a nixpkgs **setup hook** that
  starts the server before a derivation's configure phase and tears it down on
  exit. See the [README](./README.md) for details.

## Status

nixception is **experimental**. The server topology, the setup-hook contract,
and the derivation encoding are still evolving; interfaces may change between
releases.

## Credits and licensing

nixception is built on top of
[NativeLink](https://github.com/TraceMachina/nativelink) by Trace Machina, Inc.
and the NativeLink authors, and derives from its last Apache-2.0 licensed
commit. All credit for the underlying build-cache and remote-execution
infrastructure belongs to them.

The nixception sources are licensed under **Apache-2.0**. Binary distributions
of the server are additionally governed by **GPL-3.0** through the statically
linked `nix-compat` crate — see [NOTICE](./NOTICE) for full attribution and
licensing details.
