{
  description = "nixception";
  # NOTE: This flake uses git submodules (vendor/). `inputs.self.submodules`
  # makes plain `nix build` include them; on Nix < 2.27 build with:
  #   nix build ".?submodules=1#<target>"

  inputs = {
    self.submodules = true;
    # Pinned to the upstream nixpkgs base the integration checks (and the
    # nixpkgs `nixception` fork) were written against — recc-nix needs
    # `overrideAllMesonComponents`, the bazel checks need
    # bazel_8/build-support/bazelPackage.nix, and protoc-gen-js's newer
    # packaging, all of which postdate the previous nixos-unstable pin.
    nixpkgs.url = "github:NixOS/nixpkgs/f4220f112a5a6bdc03a69ce1633d099005174edc";
    flake-parts.url = "github:hercules-ci/flake-parts";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane = {
      url = "github:ipetkov/crane";
    };
  };

  outputs = inputs @ {
    self,
    flake-parts,
    crane,
    rust-overlay,
    ...
  }:
    flake-parts.lib.mkFlake {inherit inputs;} {
      systems = [
        "x86_64-linux"
        "x86_64-darwin"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      perSystem = {
        config,
        pkgs,
        system,
        lib,
        ...
      }: let
        # The Rust toolchain, with every musl/gnu target the cross builds
        # below need. (Formerly provided by upstream NativeLink's
        # local-remote-execution overlay.)
        rustVersion = "1.96.1";
        rustFor = p:
          p.rust-bin.stable.${rustVersion}.default.override {
            targets = [
              "aarch64-unknown-linux-gnu"
              "aarch64-unknown-linux-musl"
              "x86_64-unknown-linux-gnu"
              "x86_64-unknown-linux-musl"
            ];
          };
        stable-rust = rustFor pkgs;

        craneLibFor = p: (crane.mkLib p).overrideToolchain rustFor;

        src = pkgs.lib.cleanSourceWith {
          src = (craneLibFor pkgs).path ./.;
          filter = path: type:
            (builtins.match "^.*(examples/.+\.json5|data/.+|nativelink-config/README\.md)" path != null)
            || ((craneLibFor pkgs).filterCargoSources path type);
        };

        # Warning: The different usages of `p` and `pkgs` are intentional as we
        # use crosscompilers and crosslinkers whose packagesets collapse with
        # the host's packageset. If you change this take care that you don't
        # accidentally explode the global closure size.
        commonArgsFor = p: let
          isLinuxBuild = p.stdenv.buildPlatform.isLinux;
          isLinuxTarget = p.stdenv.targetPlatform.isLinux;
          # Map the nix system to the Rust target triple that we'd want to target
          # by default.
          targetArch =
            (
              nixSystem:
                {
                  "x86_64-linux" = "x86_64-unknown-linux-musl";
                  "aarch64-linux" = "aarch64-unknown-linux-musl";
                  "x86_64-darwin" = "x86_64-apple-darwin";
                  "aarch64-darwin" = "aarch64-apple-darwin";
                }
                  .${
                  nixSystem
                } or (throw "Unsupported Nix host platform: ${nixSystem}")
            )
            p.stdenv.targetPlatform.system;

          # Full path to the linker for CARGO_TARGET_XXX_LINKER
          linkerPath =
            if isLinuxBuild && isLinuxTarget
            then "${pkgs.mold}/bin/ld.mold"
            else "${pkgs.llvmPackages_20.lld}/bin/ld.lld";

          linkerEnvVar = "CARGO_TARGET_${
            pkgs.lib.toUpper (pkgs.lib.replaceStrings ["-"] ["_"] targetArch)
          }_LINKER";
        in
          {
            inherit src;
            stdenv = q:
              if q.stdenv.targetPlatform.isLinux
              then q.pkgsMusl.stdenv
              else q.stdenv;
            strictDeps = true;
            buildInputs =
              [
                p.cacert
              ]
              ++ pkgs.lib.optionals p.stdenv.targetPlatform.isDarwin [
                p.darwin.apple_sdk.frameworks.Security
                p.libiconv
              ];
            nativeBuildInputs =
              (
                if isLinuxBuild
                then [pkgs.mold]
                else [pkgs.llvmPackages_20.lld]
              )
              ++ pkgs.lib.optionals p.stdenv.targetPlatform.isDarwin [
                p.darwin.apple_sdk.frameworks.Security
                p.libiconv
              ];
            CARGO_BUILD_TARGET = targetArch;
          }
          // (pkgs.lib.optionalAttrs isLinuxTarget {
            CARGO_BUILD_RUSTFLAGS = "-C target-feature=+crt-static";
            ${linkerEnvVar} = linkerPath;
          });

        # Additional target for external dependencies to simplify caching.
        cargoArtifactsFor = p: (craneLibFor p).buildDepsOnly (commonArgsFor p);

        # The runner is built once per host platform (it never needs to be
        # cross-compiled the way the server itself does): a plain
        # pkgs.callPackage build of tools/runner/runner.nix.  Only nixception
        # is in charge of the runner — consumers never build or configure
        # their own; extra sandbox tools go through
        # NIXCEPTION_EXTRA_SANDBOX_PATHS instead (see nixceptionHookFor).
        runner = pkgs.callPackage ./tools/runner/runner.nix {};

        nixceptionFor = p:
          ((craneLibFor p).buildPackage (
            (commonArgsFor p)
            // {
              cargoArtifacts = cargoArtifactsFor p;
              cargoExtraArgs = "--bin nixception";
              # The runner's store paths, baked into the binary at *compile*
              # time via Rust's env!() (see
              # nativelink-scheduler/src/runner_info.rs) — not a runtime env
              # var, so nixceptionHook (or anyone else running this binary)
              # has no say in which runner it uses.
              NIXCEPTION_RUNNER_OUT = "${runner}";
              NIXCEPTION_RUNNER_DRV = "${runner.drvPath}";
            }
          ))
          // {
            passthru.runner = runner;
          };

        # nixceptionHookFor builds the setup hook for a given nixception binary.
        # The hook starts a nixception server before the build phase and stops
        # it afterwards, wiring in the runner nixception itself owns (built
        # once above) — nixceptionHook carries no runner-related configuration
        # of its own.
        nixceptionHookFor = p:
          pkgs.callPackage ./tools/nixception-hook.nix {
            nixception = nixceptionFor p;
          };

        nativeTargetPkgs =
          if pkgs.system == "x86_64-linux"
          then pkgs.pkgsCross.musl64
          else if pkgs.system == "aarch64-linux"
          then pkgs.pkgsCross.aarch64-multiplatform-musl
          else pkgs;

        nixception = nixceptionFor nativeTargetPkgs;
        nixceptionHook = nixceptionHookFor nativeTargetPkgs;

        # The integration suite (checks/overlay.nix), applied against this
        # flake's own `pkgs` with the local-source nixceptionHook above (not
        # one fetched from nixpkgs) — `final`/`_prev` are both `pkgs` since the
        # overlay never actually reads `_prev`.  Its own output is
        # `{ nixceptionChecks = <the per-test attrset>; }`; unwrap that one key.
        nixceptionChecks =
          ((import ./checks/overlay.nix {inherit nixceptionHook;})
            pkgs
            pkgs)
          .nixceptionChecks;

        # Fixture for the standalone (outside-sandbox) integration tests in
        # nativelink-scheduler/tests/standalone_recc.rs: a directory of symlinks
        # to every tool the Rust harness needs, plus the pre-built runner (out +
        # drv) and the extra-sandbox-paths toolset.  The test is gated on env
        # vars pointing into this fixture, so run it with e.g.:
        #
        #   fixture=$(nix build --impure --no-link --print-out-paths \
        #     '.?submodules=1#standalone-test-fixture')
        #   export NIXCEPTION_FIXTURE_BIN="$fixture/bin"
        #   export NIXCEPTION_FIXTURE_RUNNER_OUT="$(cat "$fixture/runner-out")"
        #   export NIXCEPTION_FIXTURE_RUNNER_DRV="$(cat "$fixture/runner-drv")"
        #   export NIXCEPTION_FIXTURE_NIXCEPTION="$(cat "$fixture/nixception")"
        #   export NIXCEPTION_FIXTURE_GCC="$(cat "$fixture/gcc")"
        #   export NIXCEPTION_FIXTURE_EXTRA_SANDBOX_PATHS="$(cat "$fixture/extra-sandbox-paths")"
        #   cargo test --workspace --test standalone_recc -- --test-threads=1 --nocapture
        standalone-test-fixture = let
          # The runner must match the one nixception's hook uses so the
          # reapi-action derivations (and their cache) line up — the same
          # nixception.passthru.runner the hook wires in, no test-specific
          # runner build.  The test compiler toolchain instead goes through
          # NIXCEPTION_EXTRA_SANDBOX_PATHS, exactly as a real consumer would
          # configure it.
          runner = nixception.passthru.runner;
          extraSandboxPaths = [pkgs.gcc pkgs.binutils pkgs.coreutils];
        in
          pkgs.runCommand "nixception-standalone-fixture" {} ''
            mkdir -p $out/bin
            ln -s ${pkgs.nix}/bin/nix            $out/bin/nix
            ln -s ${pkgs.nix}/bin/nix-store      $out/bin/nix-store
            ln -s ${pkgs.buildbox}/bin/recc      $out/bin/recc
            # Record the runner output + derivation paths for RunnerInfo.
            echo -n "${runner}"          > $out/runner-out
            echo -n "${runner.drvPath}"  > $out/runner-drv
            # The nixception binary under test: the one this flake builds from
            # local source.
            echo -n "${nixception}/bin/nixception" > $out/nixception
            # The REAL compiler store paths.  recc must be invoked with these (not
            # a symlink) so nixception scans them as /nix/store references and
            # includes the compiler in the reapi-action sandbox.
            echo -n "${pkgs.gcc}/bin/gcc"   > $out/gcc
            echo -n "${pkgs.gcc}/bin/g++"   > $out/g++
            # The toolset for NIXCEPTION_EXTRA_SANDBOX_PATHS: colon-separated,
            # matching the env var's own format.
            echo -n "${lib.concatMapStringsSep ":" toString extraSandboxPaths}" \
              > $out/extra-sandbox-paths
          '';

        # These two can be built by all build platforms. This is not true for
        # darwin targets which are only buildable via native compilation.
        nixception-aarch64-linux = nixceptionFor pkgs.pkgsCross.aarch64-multiplatform-musl;
        nixception-x86_64-linux = nixceptionFor pkgs.pkgsCross.musl64;

      in rec {
        _module.args.pkgs = import self.inputs.nixpkgs {
          inherit system;
          overlays = [
            (import rust-overlay)
            (import ./tools/rust-overlay-cut-libsecret.nix)
          ];
        };
        apps = {
          default = {
            type = "app";
            program = "${nixception}/bin/nixception";
          };
        };
        packages =
          rec {
            inherit
              nixception
              nixceptionHook
              standalone-test-fixture
              nixception-aarch64-linux
              nixception-x86_64-linux
              ;

            default = nixception;

          }
          // (
            # It's not possible to crosscompile to darwin, not even between
            # x86_64-darwin and aarch64-darwin. We create these targets anyways
            # To keep them uniform with the linux targets if they're buildable.
            if pkgs.stdenv.system == "aarch64-darwin"
            then {
              nixception-aarch64-darwin = nixception;
            }
            else if pkgs.stdenv.system == "x86_64-darwin"
            then {
              nixception-x86_64-darwin = nixception;
            }
            else {}
          );
        # The recc/reccStdenv/Bazel integration checks (checks/overlay.nix),
        # built against this repo's own local-source nixception/nixceptionHook.
        # Run them all with `nix flake check`, or one at a time with
        # `nix build .#checks.<system>.<name>`.  This repo's coverage of the
        # standalone (outside-sandbox) path is separate — see
        # standalone-test-fixture and nativelink-scheduler/tests/standalone_recc.rs,
        # which is driven by cargo, not `nix flake check`.
        checks = nixceptionChecks;
        devShells.default = pkgs.mkShell {
          packages = [
            pkgs.git
            stable-rust
            pkgs.rust-analyzer
            pkgs.nil
            pkgs.protobuf
          ];
        };
      };
    };
}
