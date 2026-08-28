# tools/runner.nix
#
# The "runner" is a small C++ program used as the `builder` of every REAPI
# action derivation that nixception creates.  It reads a JSON manifest (passed
# via Nix's passAsFile mechanism) describing the action's inputs, environment,
# command, and expected outputs, then executes the action using direct system
# calls — avoiding the overhead of generating and evaluating a bash script.
#
# The actual build/install steps live in tools/Makefile so they can also be
# run outside Nix (e.g. `make -C tools` while hacking on runner.cpp).
#
# The derivation produces:
#
#   $out/bin/runner      – the C++ binary (used as the derivation builder)
#   $out/nix-support/sandbox-inputs
#                        – a file whose content references every runtimeInput
#                          store path, ensuring they remain in the runner's
#                          closure.  The Nix sandbox for reapi-action
#                          derivations (which use the runner as their builder)
#                          therefore includes these paths, making the tools
#                          available to executed commands.
#
# extraRuntimeInputs is placed first so caller-supplied tools shadow the
# built-in defaults (coreutils, util-linux, bashNonInteractive).
#
# This file is called from flake.nix via `pkgs.callPackage ./tools/runner.nix`.
{
  stdenv,
  nlohmann_json,
  lib,
  coreutils,
  util-linux,
  tree,
  bashNonInteractive,
  # Additional packages whose store paths must be available inside the Nix
  # sandbox when the runner executes an action.  These are listed first so
  # they take precedence over the built-in defaults.
  extraRuntimeInputs ? [],
}: let
  # All packages that should be reachable in the reapi-action sandbox.
  # The runner binary itself does not invoke them — they are for the
  # command being executed (e.g. a compiler wrapper).
  sandboxInputs = extraRuntimeInputs ++ [coreutils util-linux bashNonInteractive tree];
in
  stdenv.mkDerivation {
    name = "runner";
    src = lib.fileset.toSource {
      root = ./.;
      fileset = lib.fileset.unions [./Makefile ./runner.cpp];
    };

    # nlohmann_json is header-only; we only need its include path at compile
    # time.  Putting it in buildInputs lets the CC wrapper find the headers
    # automatically via NIX_CFLAGS_COMPILE / -isystem.
    buildInputs = [nlohmann_json];

    dontConfigure = true;

    # $out is already exported into the build environment; the Makefile picks
    # it up directly (`out ?= ...` isn't needed since make reads env vars).
    postInstall = ''
      # Reference every sandbox-input store path so the Nix scanner keeps
      # them in the runner's closure.  Without this the sandbox for
      # reapi-action derivations (which declare the runner as an input
      # derivation) would not contain these tools.
      mkdir -p $out/nix-support
      echo "${lib.concatMapStringsSep " " toString sandboxInputs}" \
        > $out/nix-support/sandbox-inputs
    '';

    meta = {
      description = "C++ runner for nixception REAPI action derivations";
      platforms = lib.platforms.linux;
    };
  }
