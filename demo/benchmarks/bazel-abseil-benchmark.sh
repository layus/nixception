#!/usr/bin/env bash
# Hyperfine benchmark: nixception vs. vanilla for the abseil-cpp //... Bazel
# build (hundreds of C++ compile actions) — the heavyweight counterpart to
# slow-compile-benchmark.sh's single translation unit.
#
#   bazel-abseil-cpp / cold  — nix build .#bazel-abseil-cpp, REAPI cache empty
#   bazel-abseil-cpp / hot   — nix build .#bazel-abseil-cpp, REAPI cache warm
#   vanilla-bazel-abseil-cpp — nix build .#vanilla-bazel-abseil-cpp (no nixception)
#
# bazel-abseil-cpp and vanilla-bazel-abseil-cpp come from the same mkAbseil in
# bazel/demos.nix — same source, registry, targets, install layout — differing
# only in whether compiles route through nixception, so the vanilla number
# isolates nixception's contribution rather than comparing different builds.
#
# There is no dev-shell/buildPhase-by-hand axis here (unlike slow-compile):
# abseil-cpp has no reccStdenv wrapper to drive by hand, and Bazel's own
# --remote_executor wiring lives entirely in the derivation's overrideAttrs —
# so this is a three-scenario, not four-scenario, benchmark.
#
# Run from anywhere; needs `nix`, `fd`, and `hyperfine` on PATH. Each cold
# and vanilla build takes several minutes (hundreds of compile actions) — a
# full run of this script is on the order of 30-40 minutes. Resolves the demo
# flake as ../ relative to this script's own location.
#
#   demo/benchmarks/bazel-abseil-benchmark.sh
set -euo pipefail
BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEMO_DIR="$(dirname "$BENCH_DIR")"
cd "$DEMO_DIR"

command -v hyperfine >/dev/null || { echo "FAIL: hyperfine not on PATH" >&2; exit 1; }
command -v fd >/dev/null || { echo "FAIL: fd not on PATH" >&2; exit 1; }

RESULTS_JSON="${1:-$BENCH_DIR/bazel-abseil-benchmark.json}"
RESULTS_MD="${RESULTS_JSON%.json}.md"

drop_output() {
  fd --maxdepth 1 --glob --exclude '*.lock' -- "$1" /nix/store 2>/dev/null \
    | xargs -r nix-store --delete >/dev/null 2>&1 || true
}

drop_nixception_output() {
  drop_output '*-nixception-demo-bazel-abseil-cpp*'
}

drop_vanilla_output() {
  drop_output '*-vanilla-bazel-abseil-cpp*'
}

drop_cache() {
  nix run .#cleanup >/dev/null
}

# ---- scenarios ---------------------------------------------------------------

echo "== bazel-abseil-cpp / cold =="
# Both the demo output and the REAPI action cache are dropped before every
# iteration, so each of the --min-runs 3 samples pays a real, fresh cache
# miss across the whole //... build rather than reusing an earlier run's
# cached actions.
hyperfine --warmup 0 --min-runs 3 \
  --export-json /tmp/hf-abseil-nixception-cold.json \
  --prepare "$(declare -f drop_output); $(declare -f drop_nixception_output); $(declare -f drop_cache); drop_nixception_output; drop_cache" \
  --command-name 'bazel-abseil-cpp (cold)' \
  'nix build .#bazel-abseil-cpp --no-link'

echo
echo "== bazel-abseil-cpp / hot =="
# cache is warm from the cold run above; only the demo output needs dropping
# so nix re-asks nixception (and hits the cache) instead of returning the
# already-built output directly.
hyperfine --warmup 1 --min-runs 3 \
  --export-json /tmp/hf-abseil-nixception-hot.json \
  --prepare "$(declare -f drop_output); $(declare -f drop_nixception_output); drop_nixception_output" \
  --command-name 'bazel-abseil-cpp (hot)' \
  'nix build .#bazel-abseil-cpp --no-link'

echo
echo "== vanilla-bazel-abseil-cpp =="
# No REAPI cache involved — compiles run locally inside the sandbox, and
# every nix build gets a fresh sandbox anyway, so only the demo output needs
# dropping between iterations (never touched by `nix run .#cleanup`, which
# explicitly leaves vanilla-* alone).
hyperfine --warmup 0 --min-runs 3 \
  --export-json /tmp/hf-abseil-vanilla.json \
  --prepare "$(declare -f drop_output); $(declare -f drop_vanilla_output); drop_vanilla_output" \
  --command-name 'vanilla-bazel-abseil-cpp' \
  'nix build .#vanilla-bazel-abseil-cpp --no-link'

echo
echo "== summary =="
python3 - "$RESULTS_JSON" "$RESULTS_MD" "$BENCH_DIR/bazel-abseil-benchmark.dat" <<'PY'
import json, sys

names = {
    "/tmp/hf-abseil-nixception-cold.json": "bazel-abseil-cpp / cold",
    "/tmp/hf-abseil-nixception-hot.json": "bazel-abseil-cpp / hot",
    "/tmp/hf-abseil-vanilla.json": "vanilla-bazel-abseil-cpp",
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

# Feeds bazel-abseil-benchmark.gnuplot — see that file to render the PNG.
dat_path = sys.argv[3]
with open(dat_path, "w") as f:
    f.write("# idx scenario mean stddev\n")
    for i, row in enumerate(rows):
        f.write(f"{i} \"{row['scenario']}\" {row['mean']:.4f} {row['stddev']:.4f}\n")
PY

echo "Wrote $RESULTS_JSON, $RESULTS_MD, and $BENCH_DIR/bazel-abseil-benchmark.dat"
echo "Render the chart: (cd $BENCH_DIR && gnuplot bazel-abseil-benchmark.gnuplot)"
