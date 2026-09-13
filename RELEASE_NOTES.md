# nixception 0.6.1

This release fixes two regressions introduced by an unrelated commit back in
July whose "sync `tools/` up to the current nixpkgs-fork versions" went
backwards — the nixpkgs fork's copy was actually older, so the sync silently
reverted work that had landed just days earlier. One of the two reverted
pieces (an executable-bit fix) was caught and restored in v0.6.0; these two
were not.

## Highlights

- **Cache-hit/miss reporting was wrong for every action.** The runner had
  stopped writing `$out/timing.json`, and nixception's cache classification
  treats a missing timing record as proof of a cache hit — so with the file
  never written, every action was reported as cached, regardless of whether
  it actually executed. A freshly cache-cleared, multi-second-long compile
  would print "Execution: none (all actions were cached)" and a 100% hit
  rate. Timing instrumentation (`setup`/`task`/`wrap-up`/nix→runner latency)
  is restored, so both the cache accounting and the "Command execution"
  breakdown in the timing summary are accurate again.
- **`NIXCEPTION_LOG` was not honored.** The setup hook had been quietly
  demoted to setting only `RUST_LOG`, even though the server's own `main()`
  bridges `NIXCEPTION_LOG` into `RUST_LOG` and documents it as taking
  precedence. Anyone with `RUST_LOG` already set in their environment for
  other tooling would have that value silently used for nixception too,
  instead of `NIXCEPTION_LOG`. The hook now checks `NIXCEPTION_LOG` first,
  matching the server's documented precedence.

See the [changelog](./CHANGELOG.md) for the full list.

## Licensing

The nixception sources are licensed under **Apache-2.0**. Binary distributions
of the server are additionally governed by **GPL-3.0** through the statically
linked `nix-compat` crate — see [NOTICE](./NOTICE) for full attribution and
licensing details.
