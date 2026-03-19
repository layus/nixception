# tools/runner.nix
#
# The "runner" is a small bash wrapper script used as the `builder` of every
# REAPI action derivation that nixception creates.  It sets up a $PATH with
# coreutils, util-linux, and bashNonInteractive unconditionally, plus any extra
# packages supplied by the caller via `extraRuntimeInputs`.
#
# PATH ordering: writeShellApplication sets PATH to exactly
#   ${makeBinPath runtimeInputs}
# (it does not inherit the ambient PATH).  The first element in the list
# therefore wins.  extraRuntimeInputs is placed first so caller-supplied tools
# shadow the built-in defaults.
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
  # These are listed first in runtimeInputs so they take precedence over the
  # built-in defaults (coreutils, util-linux, bashNonInteractive).
  extraRuntimeInputs ? [],
}:
writeShellApplication {
  name = "runner";
  runtimeInputs = extraRuntimeInputs ++ [coreutils util-linux bashNonInteractive];
  text = ''
    exec "${bashNonInteractive}/bin/bash" -c "$*"
  '';
}
