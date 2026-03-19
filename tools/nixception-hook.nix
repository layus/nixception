# tools/nixception-hook.nix
#
# A setup hook that starts a nixception remote-execution server before the
# build phase and stops it afterwards.  Consumers add it to nativeBuildInputs:
#
#   nativeBuildInputs = [ nixceptionHook ];
#
# To inject extra tools into the runner sandbox (e.g. a custom compiler),
# use withPackages, which is a thin convenience wrapper around .override:
#
#   nativeBuildInputs = [ (nixceptionHook.withPackages [ gppSleeper ]) ];
#
# That is equivalent to:
#
#   nativeBuildInputs = [ (nixceptionHook.override { extraRuntimeInputs = [ gppSleeper ]; }) ];
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
  callPackage,
  # Additional packages to place on PATH inside the nixception runner sandbox.
  # These are forwarded to runner.nix, which lists them before the built-in
  # defaults (coreutils, util-linux, bashNonInteractive) so they take
  # precedence.
  extraRuntimeInputs ? [],
}: let
  runner = callPackage ./runner.nix {inherit extraRuntimeInputs;};
in
  makeSetupHook {
    name = "nixception-hook";

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

      # The runner is the tiny bash wrapper that nixception uses to execute
      # each remote action.  Its output path and .drv path are both baked in
      # so the hook can pass them to the nixception server at start-up.
      runnerOut = "${runner}";
      runnerDrv = "${runner.drvPath}";
    };
  }
  ./nixception-setup-hook.sh
