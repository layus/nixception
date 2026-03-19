# tools/nixception-hook.nix
#
# Produces a setup-hook derivation that, when added to nativeBuildInputs,
# automatically starts a nixception Remote Execution server before buildPhase
# and stops it after installPhase.
#
# The hook is self-contained: unless an explicit `runner` is passed it builds
# the default runner (coreutils, util-linux, bashNonInteractive – no extra
# inputs) and wraps nixception to export the runner environment variables
# expected by RunnerInfo::from_env().
#
# Callers that need extra tools in the runner's PATH can build a custom runner
# via tools/runner.nix and pass it as the `runner` argument:
#
#   nixceptionHook = pkgs.callPackage ../../tools/nixception-hook.nix {
#     inherit nixception runner;
#   };
#
# Callers that are happy with the bare default runner simply omit `runner`:
#
#   nixceptionHook = pkgs.callPackage ../../tools/nixception-hook.nix {
#     inherit nixception;
#   };
#
# In both cases, adding the resulting derivation to nativeBuildInputs is all
# that is required – no manual start/stop code is needed in buildPhase.
#
{
  # Custom packages – not in nixpkgs, must be supplied by the caller.
  nixception,
  # Standard nixpkgs packages – auto-wired by callPackage.
  wait4x,
  moreutils,
  callPackage,
  makeSetupHook,
  writeShellScriptBin,
  # Optional: a pre-built runner derivation.  When null (the default) a
  # minimal runner is built automatically using tools/runner.nix with no
  # extraRuntimeInputs.
  runner ? null,
}: let
  # Use the caller-supplied runner, or fall back to the bare default.
  resolvedRunner =
    if runner != null
    then runner
    else callPackage ./runner.nix {};

  # Wrapper that injects the runner store paths as environment variables so
  # nixception's RunnerInfo::from_env() can discover the runner at startup.
  nixceptionWrapper = writeShellScriptBin "nixception" ''
    export NIXCEPTION_RUNNER_OUT=${resolvedRunner}
    export NIXCEPTION_RUNNER_DRV=${resolvedRunner.drvPath}
    exec ${nixception}/bin/nixception "$@"
  '';
in
  makeSetupHook {
    name = "nixception-hook";
    substitutions = {
      nixceptionBin = "${nixceptionWrapper}/bin/nixception";
      wait4xBin = "${wait4x}/bin/wait4x";
      tsBin = "${moreutils}/bin/ts";
    };
  }
  ./nixception-setup-hook.sh
