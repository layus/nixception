#!/usr/bin/env bash
# Prove that nixception's cache crosses environments for a Bazel build too:
# the same slow_table.c compile action, dispatched once by a real `nix build`
# and once by hand from a `nix develop` shell running the identical `bazel
# build` invocation, must be the exact same REAPI action — same *.drv, not
# just a similar-looking one.
#
# Run from anywhere; needs `nix` and `fd` on PATH.
#
#   ./slow-bazel-test.sh
#
# What it took to get there:
#
# - Bazel talks to nixception directly over gRPC (see bazelRemoteExecFlags in
#   bazel/demos.nix) — there's no recc wrapper here to normalize the
#   environment, so this script has to replicate bazelPackage.nix's own
#   buildPhase by hand instead of leaning on a compiler wrapper.
#
# - `slow-bazel` is a __structuredAttrs derivation (bazelPackage.nix sets it),
#   so `preBuildPhase`/`buildPhase` are plain shell *variables* holding the
#   phase script text, not shell functions — `nix print-dev-env` flattens
#   structuredAttrs into ordinary variable assignments when sourced, so this
#   script `eval`s them directly (`eval "$preBuildPhase"; eval "$buildPhase"`)
#   instead of calling them as functions the way slow-compile-test.sh does.
#
# - The compile runs in place, right in bazel/slow-compile/ (the same
#   principle as slow-compile-test.sh: no separate copy, so this tree is by
#   construction the same input the real build compiled) — but Bazel leaves
#   behind bazel-out/, bazel-bin/, repo_cache/, MODULE.bazel.lock, and a
#   .bazelrc.user-less local output-base; none of that is checked in
#   (.gitignore'd) and this script does not clean it up, matching
#   slow-compile-test.sh leaving its .o files behind.
#
# - `-frandom-seed` matches with no extra work: Bazel derives it from the
#   compile's own *relative* output path inside bazel-out/ (see the
#   `-frandom-seed=bazel-out/.../slow_table.pic.o` flag in the action's
#   command), which is identical whether Bazel was invoked by a real nix
#   build or by hand — unlike reccStdenv's compiles, there's no dependency on
#   Nix's own $out, so no NIX_OUTPATH_USED_AS_RANDOM_SEED-style pinning is
#   needed for Bazel actions.
#
# - PATH does *not* match automatically, though: Bazel forwards its own
#   invoking-shell PATH straight into the remote action via
#   `--action_env=PATH` (see bazelRemoteExecFlags), with no recc-style
#   filtering in between. `nix develop`'s from-scratch PATH construction
#   appends a "/no-such-path" sentinel that a real sandboxed `nix build`
#   never has — left in, this leaks into the uploaded action's PATH and
#   produces a *different* (still-cached-once-uploaded, but non-identical)
#   REAPI action and .drv. This script strips it before running buildPhase,
#   the same fix nixceptionHook's shellHook used to apply for reccStdenv
#   before that filtering moved into reccStdenv's own wrapper (see
#   nixception-setup-hook.sh) — Bazel demos have no such wrapper, so it has
#   to happen here instead.
#
# - Bazel's own local build cache (under ~/.cache/bazel, keyed by the
#   `$HOME` buildPhase creates via `mktemp -d`) means each invocation needs a
#   fresh $HOME, or Bazel will report "up-to-date" without re-dispatching any
#   action at all. buildPhase already does `export HOME=$(mktemp -d)` itself,
#   so nothing extra is needed here.
#
# Unlike slow-compile-test.sh, this compares *.drv identity directly, not
# just the action manifest: with nixception v0.6.0's inputSrcs-always fix and
# the PATH sentinel stripped, the shell-mode run's action resolves to the
# very same reapi-action.drv the real build produced (verified: exactly 3
# reapi-action.drv files exist in the store after both steps — main.c,
# slow_table.c, link — with no 4th appearing from the shell-mode run). If
# that ever regresses, the manifest is still the more robust fallback signal
# — see slow-compile-test.sh's header for why.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/bazel/slow-compile"

# The store path of the newest *-reapi-action.drv whose command mentions
# slow_table.c (there may be several once main.c's and the link's actions are
# mixed in). The cache was just emptied, so every path present is from this
# test run.
latest_slow_table_drv() {
  fd --maxdepth 1 --glob --exclude '*.lock' -- '*-reapi-action.drv' /nix/store 2>/dev/null \
    | xargs -r ls -t \
    | while read -r drv; do
        manifest="$(nix derivation show "$drv" 2>/dev/null \
          | python3 -c 'import json,sys; d=json.load(sys.stdin)["derivations"]; print(list(d.values())[0]["env"]["manifest"])' 2>/dev/null)"
        if [[ "$manifest" == *slow_table.c* ]]; then
          echo "$drv"
          break
        fi
      done
}

echo "== 1/4: emptying the REAPI action cache =="
nix run ../..#cleanup

echo
echo "== 2/4: nix build ..#slow-bazel (real, sandboxed) =="
nix build ../..#slow-bazel --no-link --rebuild -L
drv_a="$(latest_slow_table_drv)"
[ -n "$drv_a" ] || { echo "FAIL: no slow_table.c reapi-action found after the real build" >&2; exit 1; }

echo
echo "== 3/4: nix develop (same derivation, by hand via buildPhase) =="
slow_bazel_drv="$(nix eval --raw ../..#slow-bazel.drvPath)"
devenv="$(nix print-dev-env "$slow_bazel_drv")"

# Build right here (no separate copy — see header). Leaves bazel-out/,
# bazel-bin/, repo_cache/, MODULE.bazel.lock behind (gitignored); that's
# expected, not something this test cleans up.
env -i HOME="$HOME" bash -c "
$devenv
PATH=\"\$(printf '%s' \"\$PATH\" | tr ':' '\n' | grep -v '^/no-such-path\$' | paste -sd: -)\"
eval \"\$preBuildPhase\"
eval \"\$buildPhase\"
"

echo
echo "== 4/4: comparing the slow_table.c REAPI action .drvs =="
drv_b="$(latest_slow_table_drv)"
[ -n "$drv_b" ] || { echo "FAIL: no slow_table.c reapi-action found after the shell-mode compile" >&2; exit 1; }

if [ "$drv_a" = "$drv_b" ]; then
  echo "PASS: nix build and nix develop produced the exact same REAPI action"
  echo "      .drv for the slow_table.c compile: $drv_a"
else
  echo "FAIL: the shell-mode compile's .drv differs from the real build's."
  echo "  real build:  $drv_a"
  echo "  shell mode:  $drv_b"
  diff <(nix derivation show "$drv_a" 2>/dev/null | python3 -m json.tool) \
       <(nix derivation show "$drv_b" 2>/dev/null | python3 -m json.tool) || true
  exit 1
fi
