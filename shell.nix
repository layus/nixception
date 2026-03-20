{pkgs ? import <nixpkgs> {}}:
pkgs.callPackage (
  {
    mkShell,
    cargo,
    rustc,
    rustPlatform,
    pkg-config,
    openssl,
    nixd,
    vscode-json-languageserver,
    package-version-server,
    clang-tools,
    nodejs_24,
  }:
    mkShell {
      strictDeps = true;
      nativeBuildInputs = [
        cargo
        rustc
        rustPlatform.bindgenHook
        # optional: add pkg-config support
        pkg-config
        nixd
        vscode-json-languageserver
        package-version-server
        clang-tools
        nodejs_24
      ];
      buildInputs = [
        # add desired native packages
        # ...
        openssl
      ];
      # ...
    }
) {}
