# tools/nixception-hook.nix
#
# A setup hook that starts a nixception remote-execution server before the
# build phase and stops it afterwards.  Consumers add it to nativeBuildInputs:
#
#   nativeBuildInputs = [ nixceptionHook ];
#
# To make extra tools available inside the reapi-action sandbox (e.g. a
# custom compiler), set NIXCEPTION_EXTRA_SANDBOX_PATHS (a colon-separated
# list of /nix/store/… paths) in the build environment — see
# nativelink-scheduler/src/runner_info.rs. This hook has no package-injection
# API of its own, and no runner-related configuration at all: the nixception
# binary has its runner's store paths baked in at compile time
# (NIXCEPTION_RUNNER_OUT/_DRV, set by flake.nix's nixceptionFor), so the hook
# only ever needs to start the binary — it doesn't know or care which runner
# that is.
#
# The derivation is meant to be called from the top-level flake, e.g.:
#
#   nixceptionHook = pkgs.callPackage ./tools/nixception-hook.nix {
#     inherit nixception;
#   };
#
{
  nixception,
  wait4x,
  moreutils,
  makeSetupHook,
  runCommandLocal,
  shellcheck,
}: let
  hook = makeSetupHook {
    name = "nixception-hook";

    # Pure packaging checks (no recursive-nix / no running server required), so
    # they run in ordinary CI even though the hook's *runtime* behaviour —
    # starting the nixception server around a build — needs the recursive-nix
    # feature and is covered by reccStdenv.tests.recc-hello instead.
    passthru.tests.setup-hook =
      runCommandLocal "nixception-hook-test"
        {
          nativeBuildInputs = [shellcheck];
          installedHook = "${hook}/nix-support/setup-hook";
        }
        ''
          echo "1. every @token@ substitution must be resolved (no literal @…@ left)..."
          if grep -oE '@[a-zA-Z0-9_]+@' "$installedHook"; then
            echo "FAIL: unresolved substitution token(s) remain in the setup hook"; exit 1
          fi
          echo "   ok: no residual tokens"

          echo "2. the nixception binary store path must be baked in..."
          grep -q "${nixception}/bin/nixception" "$installedHook" \
            || { echo "FAIL: nixception binary path not substituted into hook"; exit 1; }
          echo "   ok: nixception path present"

          echo "3. the setup-hook script must pass shellcheck..."
          # SC2148: no shebang — setup hooks are sourced by stdenv, not executed.
          shellcheck --shell=bash --exclude=SC2148 "$installedHook" \
            || { echo "FAIL: shellcheck reported problems"; exit 1; }
          echo "   ok: shellcheck clean"

          echo "all nixception hook packaging assertions passed"
          touch $out
        '';

    # All @name@ tokens in the hook script are replaced at fixupPhase time by
    # substituteAll with the values of the identically-named attributes below.
    substitutions = {
      # Full store path of the nixception binary.  Used as
      # @nixception@/bin/nixception in the hook so the binary does not need
      # to be on PATH separately.
      nixception = "${nixception}";

      # wait4x and moreutils (ts) are called via their full store paths so
      # they do not need to be propagated into the consumer's PATH.
      wait4x = "${wait4x}";
      moreutils = "${moreutils}";
    };
  }
  ./nixception-setup-hook.sh;
in
  hook
