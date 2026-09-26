#!/usr/bin/env bash
# Prove that nixception's cache actually crosses environments: the same
# slow_table.c compile, done once by a real `nix build` and once by hand in
# `nix develop`, must be the *same* REAPI action (same digest), not just a
# similar-looking one.
#
# Run from anywhere; needs `nix` and `fd` on PATH (e.g. inside `nix develop`
# in this directory, or `nix shell nixpkgs#fd`).
#
#   ./slow-compile-test.sh
#
# What it took to get there:
#
# - The compile's argv (output filename included) must match byte-for-byte —
#   recc's action digest covers the full command. Solved by calling the
#   derivation's own `buildPhase` function (same Makefile invocation the real
#   build ran) instead of retyping the compile line by hand, right in
#   demo/slow-compile/ — its src (slowCompileSrc in flake.nix) is already
#   filtered to exactly Makefile + main.c + slow_table.c, so this tree is by
#   construction the same input the real build compiled.
#
#   `buildPhase` only exists as a shell *function*, and functions don't
#   survive `nix develop -c CMD` — CMD always runs as a fresh exec, not inside
#   the shell that sourced the dev-env, so it can't see them (true whether CMD
#   is `buildPhase` itself, `bash -c '...buildPhase...'`, or anything else —
#   verified all three). The literal user workflow this mimics — a real,
#   interactive `nix develop`, typing `buildPhase` at the prompt — does have
#   it in scope; driving that with `expect` was tried and works, but adds a
#   dependency and prompt-matching fragility (a user's own ~/.bashrc noise —
#   direnv, keychain, whatever — can garble what expect sees) for no stronger
#   a guarantee. `env -i` sourcing `nix print-dev-env`'s output builds the
#   *same* dev-env deterministically instead — no PTY, no prompt to match.
#
# - PATH matches a real sandboxed build's out of the box: nixceptionHook's
#   shellHook (which this sourced script runs) strips the "/no-such-path"
#   sentinel `nix develop` appends when it builds $PATH from scratch — see
#   nixception-setup-hook.sh in the nixpkgs fork.
#
# - `-frandom-seed` matches too: nixpkgs' reproducible-builds setup hook
#   normally bakes it from the *building* derivation's own $out, which for
#   `nix develop`'s synthetic shell-env derivation is unrelated to the real
#   package's — so an otherwise byte-identical compile would hash to two
#   different actions depending on which one produced it. reccStdenv pins
#   NIX_OUTPATH_USED_AS_RANDOM_SEED to a fixed constant instead (see
#   pkgs/by-name/re/reccStdenv/package.nix in the nixpkgs fork), so nothing
#   needs fixing up here.
#
# The wrapping *-reapi-action.drv Nix builds around each action is NOT a
# stable fingerprint of the action itself: nixception represents the same
# toolchain closure as already-realized inputSrcs when the caller already has
# them on disk (the real sandboxed build) vs. as inputDrvs needing evaluation
# (a `nix develop` shell) — so two runs of the byte-identical action can get
# two different .drv files. The actual cache-hit signal recc/nixception act
# on is the action *manifest* (command, environment, input digest) baked into
# that .drv's `manifest` env var. Compare that instead of the .drv path.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

# Print the manifest of the newest *-reapi-action.drv whose command mentions
# slow_table.c (there may be several once main.c's own action is mixed in).
# The cache was just emptied, so every path present is from this test run.
latest_slow_table_manifest() {
  fd --maxdepth 1 --glob --exclude '*.lock' -- '*-reapi-action.drv' /nix/store 2>/dev/null \
    | xargs -r ls -t \
    | while read -r drv; do
        manifest="$(nix derivation show "$drv" 2>/dev/null \
          | python3 -c 'import json,sys; d=json.load(sys.stdin)["derivations"]; print(list(d.values())[0]["env"]["manifest"])')"
        if [[ "$manifest" == *slow_table.c* ]]; then
          echo "$manifest"
          break
        fi
      done
}

echo "== 1/4: emptying the REAPI action cache =="
nix run .#cleanup

echo
echo "== 2/4: nix build .#slow-compile (real, sandboxed) =="
nix build .#slow-compile --no-link --print-out-paths -L
manifest_a="$(latest_slow_table_manifest)"
[ -n "$manifest_a" ] || { echo "FAIL: no slow_table.c reapi-action found after the real build" >&2; exit 1; }

echo
echo "== 3/4: nix develop (same derivation, by hand via buildPhase) =="
drv="$(nix eval --raw .#slow-compile.drvPath)"
devenv="$(nix print-dev-env "$drv")"

# Build right here (no separate copy — see header). Leaves slow_table.o /
# main.o / slow_demo behind (gitignored); that's expected, not something this
# test cleans up.
env -i HOME="$HOME" bash -c "
$devenv
cd slow-compile
buildPhase
"

echo
echo "== 4/4: comparing the slow_table.c action manifests =="
manifest_b="$(latest_slow_table_manifest)"
[ -n "$manifest_b" ] || { echo "FAIL: no slow_table.c reapi-action found after the shell-mode compile" >&2; exit 1; }

if [ "$manifest_a" = "$manifest_b" ]; then
  echo "PASS: the shell-mode compile produced the exact same REAPI action"
  echo "      (identical manifest: command, environment, input digest)."
else
  echo "FAIL: the shell-mode compile's action manifest differs from the real build's."
  diff <(echo "$manifest_a" | python3 -m json.tool) <(echo "$manifest_b" | python3 -m json.tool) || true
  exit 1
fi
