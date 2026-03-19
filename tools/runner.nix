# tools/runner.nix
#
# The "runner" is a small bash wrapper script used as the `builder` of every
# REAPI action derivation that nixception creates.  It sets up a $PATH with
# coreutils, util-linux, and bashNonInteractive unconditionally, then appends
# any extra packages supplied by the caller via `extraRuntimeInputs`.
#
# bashNonInteractive is used by default because runner actions are
# non-interactive by nature.  Callers that genuinely need an interactive shell
# (e.g. for debugging) can supply bashInteractive via `extraRuntimeInputs`.
#
# This file is called from flake.nix via `pkgs.callPackage ./tools/runner.nix`.
{
  writeShellApplication,
  coreutils,
  util-linux,
  bashNonInteractive,
  # Additional packages to place on PATH when the runner executes an action.
  # coreutils, util-linux and bashNonInteractive are always included regardless
  # of this list.  Callers should pass compiler wrappers or any other tools
  # needed at runtime.
  extraRuntimeInputs ? [],
}:
writeShellApplication {
  name = "runner";
  runtimeInputs = extraRuntimeInputs ++ [coreutils util-linux bashNonInteractive];
  text = ''
    exec "${bashNonInteractive}/bin/bash" -c "$*"
  '';
}
