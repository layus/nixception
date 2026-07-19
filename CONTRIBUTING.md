# Contributing to nixception

nixception welcomes contributions. The project is small and experimental, so
the process is deliberately lightweight.

## Contributions

Contributions should be made in the form of GitHub pull requests against
<https://github.com/layus/nixception>. Each pull request will be reviewed by a
maintainer and either landed on `main` or given feedback for required changes.

If you plan to work on an issue, please claim it first by commenting on the
GitHub issue, to avoid duplicated effort.

## Setting up

The flake uses git submodules (`vendor/`). Clone with:

```bash
git clone --recurse-submodules git@github.com:yourusername/nixception
cd nixception
```

Enter the development shell and build:

```bash
nix develop            # pinned Rust toolchain and tooling
cargo build --release --bin nixception
```

Or build the release artifacts directly (Nix ≥ 2.27 picks up submodules
automatically):

```bash
nix build
```

## Quality checks

Before submitting a pull request, make sure that:

```bash
cargo check --workspace --all-targets   # compiles warning-free
nix build                               # the release build succeeds
```

The development shell installs pre-commit hooks (formatting, typo and lint
checks) that run automatically on commit.

## Scope and upstream code

nixception is based on
[NativeLink](https://github.com/TraceMachina/nativelink); most of the codebase
outside `src/bin/nixception.rs`, `nativelink-scheduler/src/nix_*.rs`,
`nativelink-store/src/nix_*.rs` and `nativelink-topology/` is inherited
NativeLink infrastructure.

- Bug fixes and improvements to the **Nix-specific** parts are always welcome
  here.
- Improvements to the **general** build-cache/remote-execution infrastructure
  are usually better contributed to
  [upstream NativeLink](https://github.com/TraceMachina/nativelink) — this
  repository only incorporates upstream changes once they become available
  under the Apache License 2.0 (two years after their FSL release; see
  [README](./README.md#licensing)).

## Licensing of contributions

By contributing, you agree that your contributions are licensed under the
[Apache License 2.0](./LICENSE). Keep the existing copyright headers intact
when modifying inherited files.

## Conduct

The [Code of Conduct](./CODE_OF_CONDUCT.md) applies to all project spaces.
