# nixception 0.5.0

This release finishes untangling nixception from its NativeLink origins: the
crate is named `nixception`, the runner's identity is fixed at build time
instead of runtime, and the integration test suite now lives — and runs —
in this repo.

## Highlights

- **The crate is `nixception`, not `nativelink`.** `Cargo.toml` names the
  package after its only binary, and every flake output that used to shadow
  it under the old name is gone. Nothing downstream should have depended on
  the `nativelink` name, but if you scripted around it, switch to
  `nixception`.

- **The runner is baked into the binary, not passed at start-up.** Packaging
  (the nixpkgs `nixception` package, or this repo's own flake) builds the
  runner first and compiles its store paths directly into the server —
  `NIXCEPTION_RUNNER_OUT`/`NIXCEPTION_RUNNER_DRV` are no longer runtime
  configuration. The untested self-build fallback that used to reconstruct
  the runner via `nix build` at start-up is gone with it.

- **`NIXCEPTION_EXTRA_SANDBOX_PATHS` replaces per-caller runner
  customization.** Need a compiler or other tool inside a remote action's
  sandbox? Set this env var (colon-separated `/nix/store/…` paths) on the
  server process instead of building a custom runner via
  `nixceptionHook.withPackages`, which no longer exists.

- **The integration suite lives here now.** `recc-smoke-test`, `recc-hello`,
  `recc-spdlog`, `recc-nix`, the two Bazel checks, and
  `protoc-gen-js-with-nixception` moved from the private orchestration
  workspace into `checks/` in this repo, and test this repo's own source —
  run them all with `nix flake check`.

- **Fixed a `Permission denied` regression** on actions that execute a tool
  built by an earlier remote action (e.g. Bazel running its own
  remotely-built `protoc`) — a fix from earlier development had been
  silently lost in a later sync and is now restored.

See the [changelog](./CHANGELOG.md) for the full list.

## Licensing

The nixception sources are licensed under **Apache-2.0**. Binary distributions
of the server are additionally governed by **GPL-3.0** through the statically
linked `nix-compat` crate — see [NOTICE](./NOTICE) for full attribution and
licensing details.
