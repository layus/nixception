#!/usr/bin/env bash
# Hyperfine benchmark: the five data points for slow-compile's cache payoff.
#
#   in-sandbox / vanilla — nix build .#vanilla-slow-compile, plain local gcc, no nixception
#   in-sandbox / cold    — nix build .#slow-compile, REAPI action cache empty
#   in-sandbox / hot     — nix build .#slow-compile, REAPI action cache warm
#   dev-shell  / cold    — buildPhase by hand (see slow-compile-test.sh), cache empty
#   dev-shell  / hot     — buildPhase by hand, cache warm
#
# "in-sandbox" vs "dev-shell" is the same axis slow-compile-test.sh proves
# identical actions for: a real sandboxed `nix build` vs. driving the same
# derivation's buildPhase by hand from a `nix develop`-sourced environment.
# "cold" vs "hot" is nixception's whole point — first compile pays the ~5s,
# every rebuild after is a Nix-store cache hit. "in-sandbox / vanilla" is the
# reference point for both: the same slowCompileSrc/Makefile built with plain
# pkgs.stdenv — no reccStdenv, no nixception, gcc runs directly in the
# sandbox — so it isolates nixception's overhead/payoff rather than
# comparing a different build.
#
# Run from anywhere; needs `nix`, `fd`, and `hyperfine` on PATH. Resolves the
# demo flake as ../ relative to this script's own location, so it works
# whether invoked as ./slow-compile-benchmark.sh or via any other path.
#
#   demo/benchmarks/slow-compile-benchmark.sh
#
# Each scenario's --prepare resets exactly what that scenario needs reset and
# nothing else:
#   - the demo's own output path is always removed first, so `nix build`
#     can't just hand back an already-built result without asking nixception;
#     for dev-shell mode, `make clean` is the equivalent — remove the .o's so
#     the compile actually reruns.
#   - the REAPI action cache is wiped only for the cold scenarios (via the
#     flake's `cleanup` app, same one slow-compile-test.sh and the README use).
#
# Cold scenarios drop the REAPI action cache in --prepare too, so hyperfine's
# --min-runs 3 pays a fresh cache miss every iteration instead of measuring
# run 2+ as accidental cache hits — slower, but gives cold numbers a variance
# instead of a single sample. Hot scenarios keep a larger --min-runs 5 since
# each iteration is cheap.
set -euo pipefail
BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEMO_DIR="$(dirname "$BENCH_DIR")"
cd "$DEMO_DIR"

command -v hyperfine >/dev/null || { echo "FAIL: hyperfine not on PATH" >&2; exit 1; }
command -v fd >/dev/null || { echo "FAIL: fd not on PATH" >&2; exit 1; }

RESULTS_JSON="${1:-$BENCH_DIR/slow-compile-benchmark.json}"
RESULTS_MD="${RESULTS_JSON%.json}.md"

# ---- in-sandbox helpers -----------------------------------------------------

drop_output() {
  fd --maxdepth 1 --glob --exclude '*.lock' -- '*-nixception-demo-slow-compile-*' /nix/store 2>/dev/null \
    | xargs -r nix-store --delete >/dev/null 2>&1 || true
}

drop_cache() {
  nix run .#cleanup >/dev/null
}

# vanilla-slow-compile isn't touched by `nix run .#cleanup` (it deliberately
# never removes vanilla-* baselines), so it gets its own output-drop, scoped
# to its own store-path glob.
drop_vanilla_output() {
  fd --maxdepth 1 --glob --exclude '*.lock' -- '*-vanilla-slow-compile-*' /nix/store 2>/dev/null \
    | xargs -r nix-store --delete >/dev/null 2>&1 || true
}

# ---- dev-shell helpers -------------------------------------------------------
# Same environment slow-compile-test.sh builds: `nix print-dev-env` on the
# slow-compile derivation, computed once (it's pure evaluation + doesn't touch
# the REAPI cache) and reused across every dev-shell run instead of
# re-evaluated per-iteration, since a real interactive `nix develop` also
# sources its environment once and reuses it for many compiles.
drv="$(nix eval --raw .#slow-compile.drvPath)"
DEVENV="$(nix print-dev-env "$drv")"
export DEVENV

devshell_clean() {
  (cd slow-compile && make clean >/dev/null)
}

devshell_build() {
  env -i HOME="$HOME" bash -c "
$DEVENV
cd slow-compile
buildPhase
"
}

# ---- scenarios ---------------------------------------------------------------

echo "== in-sandbox / vanilla =="
# No REAPI cache involved — gcc runs directly in a fresh sandbox each build.
# Only the vanilla output needs dropping between iterations.
hyperfine --warmup 0 --min-runs 3 \
  --export-json /tmp/hf-vanilla.json \
  --prepare "$(declare -f drop_vanilla_output); drop_vanilla_output" \
  --command-name 'in-sandbox / vanilla' \
  'nix build .#vanilla-slow-compile --no-link'

echo
echo "== in-sandbox / cold =="
hyperfine --warmup 0 --min-runs 3 \
  --export-json /tmp/hf-sandbox-cold.json \
  --prepare "$(declare -f drop_output); $(declare -f drop_cache); drop_output; drop_cache" \
  'nix build .#slow-compile --no-link'

echo
echo "== in-sandbox / hot =="
# cache is warm from the cold run above; only the output needs dropping so
# nix actually re-asks nixception instead of returning the cached derivation
# output directly.
hyperfine --warmup 1 --min-runs 5 \
  --export-json /tmp/hf-sandbox-hot.json \
  --prepare "$(declare -f drop_output); drop_output" \
  'nix build .#slow-compile --no-link'

echo
echo "== dev-shell / cold =="
hyperfine --warmup 0 --min-runs 3 \
  --export-json /tmp/hf-devshell-cold.json \
  --prepare "$(declare -f drop_cache); $(declare -f devshell_clean); drop_cache; devshell_clean" \
  --command-name 'buildPhase (cold)' \
  "$(declare -f devshell_build); devshell_build"

echo
echo "== dev-shell / hot =="
# cache is warm from the cold run above; only the local .o's need dropping
# between iterations (untimed, via --prepare) so buildPhase actually reruns
# the compile instead of make finding up-to-date objects and doing nothing.
hyperfine --warmup 1 --min-runs 5 \
  --export-json /tmp/hf-devshell-hot.json \
  --prepare "$(declare -f devshell_clean); devshell_clean" \
  --command-name 'buildPhase (hot)' \
  "$(declare -f devshell_build); devshell_build"

echo
echo "== summary =="
python3 - "$RESULTS_JSON" "$RESULTS_MD" "$BENCH_DIR/slow-compile-benchmark.dat" <<'PY'
import json, sys

names = {
    "/tmp/hf-vanilla.json": "in-sandbox / vanilla",
    "/tmp/hf-sandbox-cold.json": "in-sandbox / cold",
    "/tmp/hf-sandbox-hot.json": "in-sandbox / hot",
    "/tmp/hf-devshell-cold.json": "dev-shell / cold",
    "/tmp/hf-devshell-hot.json": "dev-shell / hot",
}

rows = []
for path, label in names.items():
    with open(path) as f:
        r = json.load(f)["results"][0]
    rows.append({
        "scenario": label,
        "mean": r["mean"],
        "stddev": r["stddev"] or 0.0,
        "min": r["min"],
        "max": r["max"],
        "runs": len(r["times"]),
    })

out_json, out_md = sys.argv[1], sys.argv[2]
with open(out_json, "w") as f:
    json.dump(rows, f, indent=2)

with open(out_md, "w") as f:
    f.write("| scenario | mean (s) | stddev | min | max | runs |\n")
    f.write("|---|---|---|---|---|---|\n")
    for row in rows:
        f.write(
            f"| {row['scenario']} | {row['mean']:.3f} | {row['stddev']:.3f} "
            f"| {row['min']:.3f} | {row['max']:.3f} | {row['runs']} |\n"
        )

print(open(out_md).read())

# Feeds slow-compile-benchmark.gnuplot — see that file to render the PNG.
dat_path = sys.argv[3]
with open(dat_path, "w") as f:
    f.write("# idx scenario mean stddev\n")
    for i, row in enumerate(rows):
        f.write(f"{i} \"{row['scenario']}\" {row['mean']:.4f} {row['stddev']:.4f}\n")
PY

echo "Wrote $RESULTS_JSON, $RESULTS_MD, and $BENCH_DIR/slow-compile-benchmark.dat"
echo "Render the chart: (cd $BENCH_DIR && gnuplot slow-compile-benchmark.gnuplot)"
