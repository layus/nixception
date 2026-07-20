# nixception 0.3.0

This release focuses on **timing and cache observability** — making it clear
where time actually goes and how much the Nix cache is saving.

## Highlights

- **Runner-phase timing.** The runner now measures its own setup, task, and
  wrap-up phases and reports them back, so the previously opaque
  "command execution" span is broken down into the time spent in Nix (the
  **nix→runner latency**: daemon scheduling and sandbox setup) versus the
  runner's setup, the task itself, and wrap-up.

- **Honest wall-clock, parallelism, and throughput.** The summary no longer
  mislabels the *sum* of per-action spans as wall-clock. It now reports the real
  elapsed wall-clock separately from cumulative action time, and adds average /
  peak **parallelism** and **throughput** (actions per second).

- **Cache accounting.** Actions served from the Nix cache are now distinguished
  from executed ones. The summary reports the cache hit ratio, the estimated
  time the cache saved, and the resulting speedup.

- **Three-section summary.** The timing report is reorganized into
  **Preparation** (common to all actions), **Execution** (executed actions
  only, so cache hits don't dilute the real-work averages), and **Cached**
  (hits, with the benefit estimate).

See the [changelog](./CHANGELOG.md) for the full list, including the earlier
0.2.x fixes carried into this release.

## Licensing

The nixception sources are licensed under **Apache-2.0**. Binary distributions
of the server are additionally governed by **GPL-3.0** through the statically
linked `nix-compat` crate — see [NOTICE](./NOTICE) for full attribution and
licensing details.
