# tools/runner.nix
#
# The "runner" is a small bash wrapper script used as the `builder` of every
# REAPI action derivation that nixception creates.  It sets up a $PATH with
# coreutils, util-linux, gcc, and bash, then `exec`s bash to evaluate the
# command string passed as `$*`.
#
# This file is called from flake.nix via `pkgs.callPackage ./tools/runner.nix`.
{
  writeShellApplication,
  coreutils,
  util-linux,
  gcc,
  bash,
}:
writeShellApplication {
  name = "runner";
  runtimeInputs = [coreutils util-linux gcc bash];
  text = ''
    exec "${bash}/bin/bash" -c "$*"
  '';
}
