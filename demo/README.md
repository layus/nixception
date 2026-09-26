# nixception demos

Real packages built through **nixception** — the Remote Execution API endpoint
that turns every build action into a Nix derivation, built via recursive-nix and
cached in the Nix store.

This flake is self-contained: it pins the [`layus/nixpkgs@recc-stdenv`][fork]
branch (which packages nixception v0.5.0) and builds standard nixpkgs packages
and Bazel projects with their compile actions routed through nixception. No
sibling checkouts, no `--impure`.

[fork]: https://github.com/layus/nixpkgs/tree/recc-stdenv

## Requirement: recursive-nix

The only prerequisite is a Nix daemon with recursive-nix enabled. Add to
`nix.conf` (`/etc/nix/nix.conf` or `~/.config/nix/nix.conf`) and restart the
daemon:

```conf
experimental-features = nix-command flakes recursive-nix
system-features = recursive-nix kvm nixos-test benchmark big-parallel
```

Without it the build fails fast with `recursive-nix daemon socket not found`.

## Run

```sh
cd demo

# reccStdenv demos — build a package with its compiler swapped for a
# recc-wrapped one, so every compile routes through nixception.
nix build .#hello          # GNU Hello   (autotools / C)
nix build .#spdlog         # spdlog      (CMake / C++, real dep closure)
nix build .#zstd           # zstd        (CMake / C, dozens of TUs)
nix build .#slow-compile   # one ~5s GCC compile (huge designated initializer)
nix build .#all            # the four above, in one build

# Bazel demos — nixception driven over gRPC (--remote_executor), not via a
# wrapped compiler. Each pulls a Bazel dependency closure (needs network once).
nix build .#bazel-hello-world   # C++ hello-world, Bazel 8 + rules_nixpkgs
nix build .#bazel-abseil-cpp    # abseil-cpp //... — hundreds of compile actions
nix build .#slow-bazel          # the slow-compile translation unit, via Bazel

# Baselines — the same build, done the ordinary way (compiles run locally, no
# nixception). Compare timings, or just grab the reference binary.
nix build .#vanilla-hello .#vanilla-spdlog .#vanilla-zstd
nix build .#vanilla-slow-compile        # the ~5s compile, plain local compiler
nix build .#vanilla-abseil-cpp          # upstream CMake abseil
nix build .#vanilla-bazel-abseil-cpp    # SAME Bazel build as bazel-abseil-cpp, compiles local

nix flake check            # every demo + every baseline, as checks

./result/bin/hello
```

The first build of a package is a cold run — every compile becomes a fresh
derivation. Build again and the REAPI actions are served from the Nix store. On
success the build prints a per-cost-center timing summary (actions, cache hits,
wall-clock).

To force a cold run, drop the cache:

```sh
nix run .#cleanup          # remove the -reapi-* action cache + demo outputs
nix run .#cleanup -- --gc  # also nix-collect-garbage (frees dead Bazel closures)
```

`cleanup` only removes the `nixception-demo-*` outputs and the REAPI action
cache — never the `vanilla-*` baselines. If a `-reapi-*` path reports "still
alive" (an interrupted build left a stale temp GC root), `nix run .#cleanup --
--gc` clears it.

Run with `nix build -L` to stream build logs. Set `NIXCEPTION_VERBOSE=1` in the
build environment for live server logs; on failure the last 200 lines of the
server log are dumped automatically regardless.

## How it works

There are two ways to route a build through nixception, one per demo kind.

### reccStdenv demos (`hello`, `spdlog`, `zstd`)

Each is one line:

```nix
viaNixception = pkg: pkg.override { stdenv = pkgs.reccStdenv; };
```

`reccStdenv` (analogous to `ccacheStdenv`) carries the whole backend:

- its `cc` is **recc-wrapped** — `cc` / `gcc` / `g++` / `clang` become
  `recc <real-driver>`, so every compile the build system runs is dispatched;
- `nixceptionHook` is in `nativeBuildInputs` — it starts the nixception server
  on the sandbox loopback (`127.0.0.1:50051`) before `configurePhase` and tears
  it down on exit;
- `requiredSystemFeatures = [ "recursive-nix" ]` — so the sandbox gets a Nix
  daemon socket, and Hydra/CI without the feature simply skips these.

`recc` uploads the action inputs, nixception scans them for `/nix/store`
references, assembles a derivation, and builds it through recursive-nix. Cached
results live in the store, so any given action is built at most once across all
consumers.

### `slow-compile` — the timing payoff, isolated

`demo/slow-compile/` is a two-file C project. `slow_table.c` is checked in
(the output of `gen.py`, not regenerated at build time — see below): a
`struct row slow_table[~550000]` with every element written as a `[i]={...}`
designator. GCC resolves each designator against the growing constructor, so
parsing that initializer is super-linear — about 5s at `-O2`. `main.c` is
trivial.

Built with `stdenv = reccStdenv`, so `slow_table.o` compiles on nixception. The
first `nix build .#slow-compile` pays the ~5s; then:

```sh
nix run .#cleanup
nix build .#slow-compile      # same compile, now a Nix-store cache hit
```

Compare the two timing summaries — the second reports the action as cached with
no execution time. To change the row count, regenerate the checked-in file:
`python3 gen.py <rows> > slow-compile/slow_table.c`.

`slow_table.c` is static (rather than generated fresh by the Makefile) so that
every build compiles byte-identical input — required for `slow-compile-test.sh`
(below) to prove the same action is reused across environments. The
derivation's own `src` (`slowCompileSrc` in `flake.nix`) is filtered down to
exactly `Makefile` + `main.c` + `slow_table.c` — not `gen.py`, which is only a
one-off generator and never read at build time.

### `slow-compile-benchmark.sh` — timing the cache payoff, five ways

```sh
demo/benchmarks/slow-compile-benchmark.sh
```

Needs `nix`, `fd`, and [`hyperfine`](https://github.com/sharkdp/hyperfine) on
PATH. Times the same compile across the two axes this demo cares about —
in-sandbox (`nix build`) vs. dev-shell (`buildPhase` by hand, same trick
`slow-compile-test.sh` uses) crossed with cold vs. warm REAPI action cache —
plus a fifth `vanilla` scenario (`nix build .#vanilla-slow-compile`: the same
`slowCompileSrc`/Makefile built with plain `pkgs.stdenv`, gcc running directly
in the sandbox, no nixception at all) as the reference point both axes are
measured against. Writes `benchmarks/slow-compile-benchmark.{json,md,dat}` (gitignored;
regenerate on demand). Every scenario's demo output / `.o` files are dropped
between iterations (untimed, via `--prepare`) so the compile actually reruns
each time; cold scenarios also drop the REAPI action cache the same way (`nix
run .#cleanup`), so every scenario — not just the hot ones — gets a real
hyperfine sample (`--min-runs 3` for cold and vanilla, `--min-runs 5` for hot)
instead of a single data point.

The in-sandbox numbers include sandbox setup/teardown (nixceptionHook starting
and stopping the server) on top of the actual cache lookup — pass
`NIXCEPTION_VERBOSE=1 nix build .#slow-compile -L` separately to see
nixception's own timing summary and confirm how much of that wall-clock is
sandbox overhead vs. actual action time.

Don't run this alongside `bazel-abseil-benchmark.sh` (below) — both drive the
same Nix daemon and REAPI action cache, and `nix run .#cleanup` from one would
wipe cache state the other is mid-measurement on.

```sh
(cd demo/benchmarks && gnuplot slow-compile-benchmark.gnuplot)   # after a benchmark run — reads the .dat, writes slow-compile-benchmark.png
```

A bar chart of all five scenarios' means with stddev error bars, generated
from the same `.dat` the benchmark script writes at the end of its run.

### `slow-compile-test.sh` — does the cache actually cross environments?

```sh
demo/slow-compile-test.sh
```

A plain script, not a flake app — run it directly (needs `nix` and `fd` on
PATH). nixception's action cache is only useful if compiling the *same*
source through *different* entry points — a real `nix build` (sandboxed) and
`nix develop` (interactive, see below) — produces the *same* REAPI action, not
just a similar-looking one. This proves it rather than assuming it:

1. `nix run .#cleanup` — empty the REAPI action cache.
2. `nix build .#slow-compile` — the real, sandboxed build. Record the manifest
   (command, environment, input digest) of the `slow_table.c` action it
   creates.
3. Enter the same derivation's build environment by hand (`nix print-dev-env`
   + its `buildPhase` function — the same Makefile invocation the real build
   ran, so the command line matches with nothing to retype) and build right in
   `demo/slow-compile/` — no separate copy; the derivation's own filtered
   `src` is already the minimal, exact input. Leaves `*.o`/`slow_demo` behind
   (gitignored) — the test doesn't clean up after itself.
4. Record the new `slow_table.c` action's manifest and diff it against step
   2's. Identical manifest → PASS. The wrapping `*-reapi-action.drv` file
   itself is *not* compared: nixception represents an identical action
   differently depending on whether its toolchain inputs are already realized
   on disk (sandboxed build) or still need evaluating (`nix develop` shell),
   so two `.drv` files can legitimately differ for the same action — the
   manifest is the real signal.

Two environment gaps had to be closed for step 3 to actually match — both
noted inline in the script:

- **PATH**: a from-scratch env build appends a `/no-such-path` sentinel the
  real sandboxed build never has. Stripped after sourcing.
- **`-frandom-seed`**: nixpkgs' reproducible-builds hook bakes this from the
  *building* derivation's own `$out`. `nix develop`'s synthetic shell-env
  derivation has an unrelated `$out`, and the (wrong) seed is baked into the
  printed script at `nix print-dev-env` time — setting the env var beforehand
  doesn't reach it. Fixed by overwriting `NIX_CFLAGS_COMPILE`'s seed after
  sourcing, with the one the real build actually used.

### Bazel demos (`bazel-hello-world`, `bazel-abseil-cpp`, `slow-bazel`)

Bazel talks to nixception directly — no wrapped compiler. The build is a
`bazelPackage` (from the nixpkgs fork's `bazel_8` build support) with the
nixception wiring added by `overrideAttrs` (`demo/bazel/demos.nix`):

- `--remote_executor=grpc://127.0.0.1:50051` (+ `--spawn_strategy=remote`,
  `--noremote_local_fallback`) — every compile/link spawn becomes a nixception
  REAPI action;
- `nixceptionHook` + `nix` + `cacert` in `nativeBuildInputs`, plus
  `NIX_REMOTE=unix:///build/.nix-socket` — the hook starts the server, and
  `rules_nixpkgs` runs `nix-build` to configure the CC toolchain (hence
  recursive-nix);
- `NIXCEPTION_EXTRA_SANDBOX_PATHS` puts the CC toolchain inside the action
  sandbox.

`bazelPackage` splits into a network-only dependency FOD and an air-gapped
build; the nixception wiring is on the latter only, so the FOD stays a plain
fetch. The FOD hashes are pinned in `demos.nix` — if a Bazel registry pin
drifts, `nix build` reports the mismatch and prints the new hash.

`bazel-abseil-cpp` and `vanilla-bazel-abseil-cpp` come from one `mkAbseil`
function, differing only in `bazelWiring { nixception = true | false }`: the
baseline keeps the Nix daemon (rules_nixpkgs runs `nix-build` to configure the
CC toolchain, so `recursive-nix` and `nix` on PATH are needed either way) but
drops the remote executor, the hook, and the verbose/sandbox-path env — so its
compile actions run locally. Same sources, registry, targets, and install
layout, which is the point: comparing their build times isolates nixception,
not Bazel.

### `bazel-abseil-benchmark.sh` — timing nixception against vanilla, at scale

```sh
demo/benchmarks/bazel-abseil-benchmark.sh
```

Needs `nix`, `fd`, and `hyperfine` on PATH. The heavyweight counterpart to
`slow-compile-benchmark.sh`: instead of one translation unit, this times the
whole `abseil-cpp //...` build (hundreds of compile actions) three ways —
`bazel-abseil-cpp` cold, `bazel-abseil-cpp` hot, and `vanilla-bazel-abseil-cpp`
— writing `benchmarks/bazel-abseil-benchmark.{json,md,dat}` (gitignored; regenerate on
demand). `bazel-abseil-cpp` and the vanilla baseline come from the same
`mkAbseil` in `demos.nix`, so the vanilla number isolates nixception's
contribution rather than comparing a different build.

There's no dev-shell/buildPhase-by-hand axis here — unlike `slow-compile`,
abseil-cpp has no reccStdenv wrapper to drive outside the sandbox, so this is
a three-scenario, not four-scenario, benchmark. Cold and vanilla runs use
`--min-runs 3`, dropping the relevant demo output (and, for cold, the REAPI
action cache via `nix run .#cleanup`) before every iteration so each sample
pays a real cache miss / local compile rather than reusing an earlier run.
Each cold or vanilla build is several minutes — a full run of this script is
on the order of half an hour. Don't run it alongside `slow-compile-benchmark.sh`
(above) — both drive the same Nix daemon and REAPI action cache.

```sh
(cd demo/benchmarks && gnuplot bazel-abseil-benchmark.gnuplot)   # after a benchmark run — reads the .dat, writes bazel-abseil-benchmark.png
```

Same bar-chart-with-error-bars treatment as `slow-compile-benchmark.gnuplot`.

### `spdlog-benchmark.sh` — timing nixception against vanilla, for a real dependency closure

```sh
demo/benchmarks/spdlog-benchmark.sh
```

Needs `nix`, `fd`, and `hyperfine` on PATH. The `spdlog` counterpart to
`bazel-abseil-benchmark.sh`'s three-scenario shape — `spdlog` cold, `spdlog`
hot, `vanilla-spdlog` — writing `benchmarks/spdlog-benchmark.{json,md,dat}` (gitignored;
regenerate on demand). `spdlog` and `vanilla-spdlog` both build from
`pkgs.spdlog` (see `reccDemos.spdlog` / `vanillaSources.spdlog` in
`flake.nix`), differing only in stdenv, so the vanilla number isolates
nixception's contribution.

`vanilla-spdlog` needs different handling than the other vanilla baselines:
it's `pkgs.spdlog` completely unmodified — not renamed/tagged the way
`vanilla-slow-compile` or `vanilla-bazel-abseil-cpp` are — so its store path
is shared with anything else already depending on plain spdlog, and it's
already in the public binary cache. Glob-deleting it between iterations would
risk touching an unrelated build's dependency and wouldn't even force a real
compile (`nix build` would just substitute it back from the cache). Instead
this scenario runs `nix build .#vanilla-spdlog --rebuild`, which forces an
actual from-source build-and-compare every time without ever deleting the
shared path — and since no REAPI cache is involved on the vanilla side either
way, there's no cold/hot split for it, just one `--min-runs 3` sample.

```sh
(cd demo/benchmarks && gnuplot spdlog-benchmark.gnuplot)   # after a benchmark run — reads the .dat, writes spdlog-benchmark.png
```

Same bar-chart-with-error-bars treatment as the other two `.gnuplot` scripts.

`slow-bazel` (`demo/bazel/slow-compile/`) compiles the same expensive
`slow_table.c` translation unit as the `slow-compile` reccStdenv demo — copies
of `main.c`/`slow_table.c`, kept in sync by hand alongside a Bazel-specific
`BUILD`/`MODULE.bazel` — but through Bazel's own C compile action instead of a
recc-wrapped compiler. It exists mainly so `slow-bazel-test.sh` (below) can
prove cache identity crosses `nix build`/`nix develop` for the gRPC-driven
path too, not just the recc-wrapped one.

### `slow-bazel-test.sh` — same proof, for the Bazel path

```sh
demo/slow-bazel-test.sh
```

The Bazel counterpart to `slow-compile-test.sh`, run the same way (needs `nix`
and `fd` on PATH). Since Bazel talks to nixception directly over gRPC — no
recc wrapper to normalize the environment — this script replicates
`bazelPackage.nix`'s own `buildPhase` by hand from a `nix develop`-sourced
shell, in place in `demo/bazel/slow-compile/` (same no-separate-copy
principle), and compares the resulting `slow_table.c` REAPI action against
the one a real `nix build .#slow-bazel` produces.

Two gaps, closed in the script itself rather than in a shared wrapper (there
is no reccStdenv here to hold them):

- **PATH**: Bazel forwards its invoking shell's PATH straight into the
  action via `--action_env=PATH`, so the same `/no-such-path` sentinel
  `nix develop` appends leaks through untouched. Stripped before running
  `buildPhase`.
- **`-frandom-seed`**: unlike reccStdenv's compiles, Bazel derives this from
  the action's own relative output path inside `bazel-out/`, which is
  identical in both environments — nothing to fix here.

With both the PATH fix and nixception v0.6.0's `inputSrcs`-always fix in
place, this one goes a step further than `slow-compile-test.sh`: it compares
the actual `*-reapi-action.drv` store path, not just the manifest, and they
match exactly — `nix build` and the hand-run `buildPhase` produce the *same*
derivation, byte for byte.

## Interactive shell

```sh
nix develop
nixception &                         # server on 127.0.0.1:50051
echo 'int main(){return 0;}' > t.c
recc gcc -c -O2 -o t.o t.c           # this compile goes through nixception
```

## Adding a demo

**reccStdenv:** pick any nixpkgs package that builds with a C/C++ `stdenv` and
add it to `reccDemos` in `flake.nix`:

```nix
mypkg = demo "mypkg" (viaNixception pkgs.mypkg);
```

If the build needs extra compiler flags (e.g. to keep bootstrap libs out of the
output), chain an `overrideAttrs` as `spdlog` does.

**Bazel:** add a `bazelPackage` to `demo/bazel/demos.nix` and end it with
`.overrideAttrs (old: (bazelNixception { } old) // { ... })`. Leave
`bazelRepoCacheFOD.outputHash` as `lib.fakeHash` for the first build, then paste
in the hash `nix build` reports.
