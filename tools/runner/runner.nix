# tools/runner/runner.nix
#
# The "runner" is a small C++ program used as the `builder` of every REAPI
# action derivation that nixception creates.  It reads a JSON manifest (passed
# via Nix's passAsFile mechanism) describing the action's inputs, environment,
# command, and expected outputs, then executes the action using direct system
# calls — avoiding the overhead of generating and evaluating a bash script.
#
# A standalone tools/runner/Makefile also exists for building runner.cpp
# outside Nix (e.g. `make -C tools/runner` while hacking on it), but this
# derivation does not use it — it compiles directly so it has no external
# Makefile dependency.
#
# The runner itself never shells out to any external tool (it execvpe()s the
# command from the manifest directly), so this derivation carries no runtime
# toolset opinion — the sandbox for the *executed command* gets whatever the
# nixception server is configured with via NIXCEPTION_EXTRA_SANDBOX_PATHS, not
# anything baked in here.
#
# This file is called from flake.nix via `pkgs.callPackage ./tools/runner/runner.nix`.
{
  stdenv,
  nlohmann_json,
  lib,
}:
  stdenv.mkDerivation {
    name = "runner";
    src = ./runner.cpp;

    # nlohmann_json is header-only; we only need its include path at compile
    # time.  Putting it in buildInputs lets the CC wrapper find the headers
    # automatically via NIX_CFLAGS_COMPILE / -isystem.
    buildInputs = [nlohmann_json];

    # Single source file — no configure step or build system needed.
    dontUnpack = true;
    dontConfigure = true;

    buildPhase = ''
      runHook preBuild
      $CXX -std=c++17 -O2 -Wall -Wextra -o runner $src
      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall
      mkdir -p $out/bin
      cp runner $out/bin/runner
      runHook postInstall
    '';

    meta = {
      description = "C++ runner for nixception REAPI action derivations";
      platforms = lib.platforms.linux;
    };
  }
