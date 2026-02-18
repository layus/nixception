{pkgs ? import <nixpkgs> {}}:
pkgs.callPackage (
  {
    mkShell,
    cargo,
    rustc,
    rustPlatform,
    pkg-config,
    openssl,
  }:
    mkShell {
      strictDeps = true;
      nativeBuildInputs = [
        cargo
        rustc
        rustPlatform.bindgenHook
        # optional: add pkg-config support
        pkg-config
      ];
      buildInputs = [
        # add desired native packages
        # ...
        openssl
      ];
      # ...
    }
) {}
