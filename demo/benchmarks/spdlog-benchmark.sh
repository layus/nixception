#!/usr/bin/env bash
# Hyperfine benchmark: nixception vs. vanilla for the spdlog demo (CMake/C++,
# a real dependency closure — fmt, catch2_3 — plus a multi-file compile).
#
#   spdlog / cold   — nix build .#spdlog, REAPI action cache empty
#   spdlog / hot    — nix build .#spdlog, REAPI action cache warm
#   vanilla-spdlog  — nix build .#vanilla-spdlog --rebuild (no nixception)
#
# spdlog and vanilla-spdlog both come from pkgs.spdlog (see reccDemos.spdlog
# and vanillaSources.spdlog in flake.nix) — same source, same nixpkgs
# derivation, differing only in stdenv (reccStdenv vs. plain), so the vanilla
# number isolates nixception's contribution rather than comparing a different
# build.
#
# vanilla-spdlog is NOT renamed/tagged the way the demo outputs are (unlike
# vanilla-slow-compile or vanilla-bazel-abseil-cpp) — it's pkgs.spdlog
# verbatim, so its store path (spdlog-1.17.0, or whatever version is pinned)
# is shared with every other build in the store that depends on spdlog, and
# it's already in the public binary cache. That makes two of the usual tricks
# unsafe/meaningless here:
#   - glob-deleting its output would touch a path other things may reference,
#     and isn't scoped to this demo the way nixception-demo-spdlog-* is;
#   - dropping it wouldn't even force a real local build — `nix build` would
#     just substitute it back from the binary cache, timing a download, not
#     a compile.
# So vanilla-spdlog uses `nix build --rebuild` instead: forces a real,
# from-source build-and-compare every time, without ever deleting the shared
# store path. There is no cold/hot split on the vanilla side either — no
# REAPI cache is involved, and --rebuild always recompiles regardless.
#
# There is no dev-shell/buildPhase-by-hand axis here (unlike slow-compile):
# this is a three-scenario benchmark, the same shape as
# bazel-abseil-benchmark.sh.
#
# Run from anywhere; needs `nix`, `fd`, and `hyperfine` on PATH. Resolves the
# demo flake as ../ relative to this script's own location.
#
#   demo/benchmarks/spdlog-benchmark.sh
set -euo pipefail
BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEMO_DIR="$(dirname "$BENCH_DIR")"
cd "$DEMO_DIR"

command -v hyperfine >/dev/null || { echo "FAIL: hyperfine not on PATH" >&2; exit 1; }
command -v fd >/dev/null || { echo "FAIL: fd not on PATH" >&2; exit 1; }

RESULTS_JSON="${1:-$BENCH_DIR/spdlog-benchmark.json}"
RESULTS_MD="${RESULTS_JSON%.json}.md"

drop_output() {
  fd --maxdepth 1 --glob --exclude '*.lock' -- '*-nixception-demo-spdlog-*' /nix/store 2>/dev/null \
    | xargs -r nix-store --delete >/dev/null 2>&1 || true
}

drop_cache() {
  nix run .#cleanup >/dev/null
}

# ---- scenarios ---------------------------------------------------------------

echo "== spdlog / cold =="
# Both the demo output and the REAPI action cache are dropped before every
# iteration, so each of the --min-runs 3 samples pays a real, fresh cache
# miss across the whole build (configure probes + multi-file compile) rather
# than reusing an earlier run's cached actions.
hyperfine --warmup 0 --min-runs 3 \
  --export-json /tmp/hf-spdlog-nixception-cold.json \
  --prepare "$(declare -f drop_output); $(declare -f drop_cache); drop_output; drop_cache" \
  --command-name 'spdlog (cold)' \
  'nix build .#spdlog --no-link'

echo
echo "== spdlog / hot =="
# cache is warm from the cold run above; only the demo output needs dropping
# so nix re-asks nixception (and hits the cache) instead of returning the
# already-built output directly.
hyperfine --warmup 1 --min-runs 5 \
  --export-json /tmp/hf-spdlog-nixception-hot.json \
  --prepare "$(declare -f drop_output); drop_output" \
  --command-name 'spdlog (hot)' \
  'nix build .#spdlog --no-link'

echo
echo "== vanilla-spdlog =="
# --rebuild forces a real from-source compile every iteration without ever
# deleting vanilla-spdlog's output — that store path is shared with anything
# else in the store that depends on plain pkgs.spdlog, so it's never a
# glob-delete target the way the demo's own tagged outputs are.
hyperfine --warmup 0 --min-runs 10 \
  --export-json /tmp/hf-spdlog-vanilla.json \
  --command-name 'vanilla-spdlog' \
  'nix build .#vanilla-spdlog --rebuild --no-link'

echo
echo "== summary =="
python3 - "$RESULTS_JSON" "$RESULTS_MD" "$BENCH_DIR/spdlog-benchmark.dat" <<'PY'
import json, sys

names = {
    "/tmp/hf-spdlog-nixception-cold.json": "spdlog / cold",
    "/tmp/hf-spdlog-nixception-hot.json": "spdlog / hot",
    "/tmp/hf-spdlog-vanilla.json": "vanilla-spdlog",
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

# Feeds spdlog-benchmark.gnuplot — see that file to render the PNG.
dat_path = sys.argv[3]
with open(dat_path, "w") as f:
    f.write("# idx scenario mean stddev\n")
    for i, row in enumerate(rows):
        f.write(f"{i} \"{row['scenario']}\" {row['mean']:.4f} {row['stddev']:.4f}\n")
PY

echo "Wrote $RESULTS_JSON, $RESULTS_MD, and $BENCH_DIR/spdlog-benchmark.dat"
echo "Render the chart: (cd $BENCH_DIR && gnuplot spdlog-benchmark.gnuplot)"
