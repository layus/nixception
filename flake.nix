{
  description = "nativelink";
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
    git-hooks = {
      url = "github:cachix/git-hooks.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane = {
      url = "github:ipetkov/crane";
    };
    nix2container = {
      # TODO(SchahinRohani): Use a specific commit hash until nix2container is stable.
      url = "github:nlewo/nix2container/cc96df7c3747c61c584d757cfc083922b4f4b33e";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs @ {
    self,
    flake-parts,
    crane,
    rust-overlay,
    nix2container,
    ...
  }:
    flake-parts.lib.mkFlake {inherit inputs;} {
      systems = [
        "x86_64-linux"
        "x86_64-darwin"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      imports = [
        inputs.git-hooks.flakeModule
        ./local-remote-execution/flake-module.nix
        ./tools/darwin/flake-module.nix
        ./tools/nixos/flake-module.nix
        ./flake-module.nix
      ];
      flake = {
        flakeModules = {
          default = ./flake-module.nix;
          darwin = ./tools/darwin/flake-module.nix;
          lre = ./local-remote-execution/flake-module.nix;
          nixos = ./tools/nixos/flake-module.nix;
        };
        overlays = {
          lre = import ./local-remote-execution/overlays/default.nix {inherit nix2container;};
          tools = import ./tools/public/default.nix {inherit nix2container;};
        };
        # TODO(jaroeichler): Keep template inputs on upstream.
        templates = {
          bazel = {
            path = ./templates/bazel;
            description = "Local remote execution with Bazel";
            welcomeText = ''
              # Getting started

              Enter the Nix environment with `nix develop`.
              Get your credentials for NativeLink and paste them into `user.bazelrc`.
              Run `bazel build hello-world` to build the example with local
              remote execution.

              See <https://www.nativelink.com/docs/explanations/lre> for further
              details on local remote execution.
            '';
          };
        };
      };
      perSystem = {
        config,
        pkgs,
        system,
        lib,
        ...
      }: let
        craneLibFor = p: (crane.mkLib p).overrideToolchain pkgs.lre.stableRustFor;
        nightlyCraneLibFor = p: (crane.mkLib p).overrideToolchain pkgs.lre.nightlyRustFor;

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
        nightlyCargoArtifactsFor = p: (craneLibFor p).buildDepsOnly (commonArgsFor p);

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

        # `nativelink` is the pre-single-binary name for this same output:
        # Cargo.toml now declares only the `nixception` [[bin]] (192fa2cf), so
        # nativelinkFor without `--bin nixception` already built the identical
        # binary — just without the runner env vars nixceptionFor now sets,
        # which broke it. Reuse nixceptionFor's build directly instead of
        # compiling it twice under two names.
        # TODO: fold the `nativelink`-named outputs below into their
        # `nixception` equivalents and drop this alias.
        nativelinkFor = nixceptionFor;

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

        nativelink = nativelinkFor nativeTargetPkgs;
        nixception = nixceptionFor nativeTargetPkgs;
        nixceptionHook = nixceptionHookFor nativeTargetPkgs;

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
        nativelink-aarch64-linux = nativelinkFor pkgs.pkgsCross.aarch64-multiplatform-musl;
        nativelink-x86_64-linux = nativelinkFor pkgs.pkgsCross.musl64;

        nativelink-is-executable-test = pkgs.callPackage ./tools/nativelink-is-executable-test.nix {
          inherit nativelink;
        };

        generate-toolchains = pkgs.callPackage ./tools/generate-toolchains.nix {};

        build-chromium-tests = pkgs.writeShellScriptBin "build-chromium-tests" ./deploy/chromium-example/build_chromium_tests.sh;

        docs = pkgs.callPackage ./tools/docs.nix {rust = pkgs.lre.stable-rust;};

        inherit (nix2container.packages.${system}.nix2container) pullImage;
        inherit (nix2container.packages.${system}.nix2container) buildImage;

        # TODO(palfrey): Allow "crosscompiling" this image. At the moment
        #                    this would set a wrong container architecture. See:
        #                    https://github.com/nlewo/nix2container/issues/138.
        nativelink-image = let
          nativelinkForImage =
            if pkgs.stdenv.isx86_64
            then nativelink-x86_64-linux
            else nativelink-aarch64-linux;
        in
          buildImage {
            name = "nativelink";
            copyToRoot = [
              (pkgs.buildEnv {
                name = "nativelink-buildEnv";
                paths = [nativelinkForImage];
                pathsToLink = ["/bin"];
              })
            ];
            config = {
              Entrypoint = [(pkgs.lib.getExe' nativelinkForImage "nativelink")];
              Labels = {
                "org.opencontainers.image.description" = "An RBE compatible, high-performance cache and remote executor.";
                "org.opencontainers.image.documentation" = "https://github.com/TraceMachina/nativelink";
                "org.opencontainers.image.licenses" = "FSL-1.1-Apache-2.0";
                "org.opencontainers.image.revision" = "${self.rev or self.dirtyRev or "dirty"}";
                "org.opencontainers.image.source" = "https://github.com/TraceMachina/nativelink";
                "org.opencontainers.image.title" = "NativeLink";
                "org.opencontainers.image.vendor" = "Trace Machina, Inc.";
              };
            };
          };

        nativelink-worker-init = pkgs.callPackage ./tools/nativelink-worker-init.nix {
          inherit buildImage self nativelink-image;
        };

        createWorker = pkgs.nativelink-tools.lib.createWorker self;

        buck2-toolchain = let
          buck2-nightly-rust-version = "2025-04-08";
          buck2-nightly-rust = pkgs.rust-bin.nightly.${buck2-nightly-rust-version};
          buck2-rust = buck2-nightly-rust.default.override {extensions = ["rust-src"];};
        in
          pkgs.callPackage ./tools/create-worker-experimental.nix {
            inherit buildImage self;
            imageName = "buck2-toolchain";
            packagesForImage = [
              pkgs.coreutils
              pkgs.bash
              pkgs.go
              pkgs.diffutils
              pkgs.gnutar
              pkgs.gzip
              pkgs.python3Full
              pkgs.unzip
              pkgs.zstd
              pkgs.cargo-bloat
              pkgs.mold-wrapped
              pkgs.reindeer
              pkgs.lld_16
              pkgs.clang_16
              buck2-rust
            ];
          };
        siso-chromium = buildImage {
          name = "siso-chromium";
          fromImage = pullImage {
            imageName = "gcr.io/chops-public-images-prod/rbe/siso-chromium/linux";
            imageDigest = "sha256:26de99218a1a8b527d4840490bcbf1690ee0b55c84316300b60776e6b3a03fe1";
            sha256 = "sha256-v2wctuZStb6eexcmJdkxKcGHjRk2LuZwyJvi/BerMyw=";
            tlsVerify = true;
            arch = "amd64";
            os = "linux";
          };
        };
        toolchain-drake = buildImage {
          name = "toolchain-drake";
          # imageDigest and sha256 are generated by toolchain-drake.sh for non-reproducible builds.
          fromImage = pullImage {
            imageName = "localhost:5001/toolchain-drake";
            imageDigest = ""; # DO NOT COMMIT DRAKE IMAGE_DIGEST VALUE
            sha256 = ""; # DO NOT COMMIT DRAKE SHA256 VALUE
            tlsVerify = false;
            arch = "amd64";
            os = "linux";
          };
        };
        toolchain-buck2 = buildImage {
          name = "toolchain-buck2";
          # imageDigest and sha256 are generated by toolchain-buck2.sh for non-reproducible builds.
          fromImage = pullImage {
            imageName = "localhost:5001/toolchain-buck2";
            imageDigest = ""; # DO NOT COMMIT BUCK2 IMAGE_DIGEST VALUE
            sha256 = ""; # DO NOT COMMIT BUCK2 SHA256 VALUE
            tlsVerify = false;
            arch = "amd64";
            os = "linux";
          };
        };

        nativelinkCoverageFor = p: let
          coverageArgs =
            (commonArgsFor p)
            // {
              # TODO(palfrey): For some reason we're triggering an edgecase where
              #                    mimalloc builds against glibc headers in coverage
              #                    builds. This leads to nonexistend __memcpy_chk and
              #                    __memset_chk symbols if fortification is enabled.
              #                    Our regular builds also have this issue, but we
              #                    should investigate further.
              hardeningDisable = ["fortify"];
            };
        in
          (nightlyCraneLibFor p).cargoLlvmCov (
            coverageArgs
            // {
              cargoArtifacts = nightlyCargoArtifactsFor p;
              cargoExtraArgs = builtins.concatStringsSep " " [
                "--all"
                "--locked"
                "--features nix"
                "--branch"
                "--ignore-filename-regex '.*(genproto|vendor-cargo-deps|crates).*'"
              ];
              cargoLlvmCovExtraArgs = "--html --output-dir $out";
            }
          );

        nativelinkCoverageForHost = nativelinkCoverageFor pkgs;
      in rec {
        _module.args.pkgs = import self.inputs.nixpkgs {
          inherit system;
          config.allowUnfreePredicate = pkg:
            builtins.elem (lib.getName pkg) [
              "mongodb"
            ];
          overlays = [
            self.overlays.lre
            self.overlays.tools
            (import rust-overlay)
            (import ./tools/rust-overlay-cut-libsecret.nix)
          ];
        };
        apps = {
          default = {
            type = "app";
            program = "${nativelink}/bin/nativelink";
          };
          native = {
            type = "app";
            program = "${pkgs.nativelink-tools.native-cli}/bin/native";
          };
        };
        packages =
          rec {
            inherit
              nativelink
              nixception
              nixceptionHook
              standalone-test-fixture
              nativelinkCoverageForHost
              nativelink-aarch64-linux
              nativelink-image
              nativelink-is-executable-test
              nativelink-worker-init
              nativelink-x86_64-linux
              ;

            # Used by the CI
            inherit (pkgs.nativelink-tools) local-image-test publish-ghcr;

            default = nativelink;

            nativelink-worker-lre-cc = createWorker pkgs.lre.lre-cc.image;
            lre-java = pkgs.callPackage ./local-remote-execution/lre-java.nix {inherit buildImage;};
            rbe-autogen-lre-java = pkgs.rbe-autogen lre-java;
            nativelink-worker-lre-java = createWorker lre-java;
            nativelink-worker-lre-rs = createWorker pkgs.lre.lre-rs.image;
            nativelink-worker-siso-chromium = createWorker siso-chromium;
            nativelink-worker-toolchain-drake = createWorker toolchain-drake;
            nativelink-worker-toolchain-buck2 = createWorker toolchain-buck2;
            nativelink-worker-buck2-toolchain = buck2-toolchain;
            image = nativelink-image;

            inherit
              (pkgs)
              buildstream
              buildbox
              buck2
              mongodb
              wait4x
              bazelisk
              ;
            buildstream-with-nativelink-test =
              pkgs.callPackage integration_tests/buildstream/buildstream-with-nativelink-test.nix
              {
                inherit nativelink buildstream buildbox;
              };
            mongo-with-nativelink-test =
              pkgs.callPackage integration_tests/mongo/mongo-with-nativelink-test.nix
              {
                inherit
                  nativelink
                  mongodb
                  wait4x
                  bazelisk
                  ;
              };
            rbe-toolchain-with-nativelink-test = pkgs.callPackage toolchain-examples/rbe-toolchain-test.nix {
              inherit nativelink bazelisk;
            };
            buck2-with-nativelink-test =
              pkgs.callPackage integration_tests/buck2/buck2-with-nativelink-test.nix
              {
                inherit nativelink buck2;
              };

            generate-bazel-rc = pkgs.callPackage tools/generate-bazel-rc/build.nix {
              craneLib = craneLibFor pkgs;
            };
          }
          // (
            # It's not possible to crosscompile to darwin, not even between
            # x86_64-darwin and aarch64-darwin. We create these targets anyways
            # To keep them uniform with the linux targets if they're buildable.
            if pkgs.stdenv.system == "aarch64-darwin"
            then {
              nativelink-aarch64-darwin = nativelink;
            }
            else if pkgs.stdenv.system == "x86_64-darwin"
            then {
              nativelink-x86_64-darwin = nativelink;
            }
            else {}
          );
        checks = {
          # The recc / reccStdenv integration checks live in the nixpkgs
          # `nixception` fork (reccStdenv.tests.*, recc.passthru.tests.*,
          # nixceptionHook.passthru.tests.*), built against the fork's real
          # reccStdenv.  This repo's own coverage of the end-to-end path is the
          # standalone (outside-sandbox) test — see standalone-test-fixture and
          # nativelink-scheduler/tests/standalone_recc.rs — which is driven by
          # cargo, not `nix flake check`.
        };
        pre-commit.settings = {
          hooks = import ./tools/pre-commit-hooks.nix {
            inherit pkgs;
            inherit (packages) generate-bazel-rc;
            nightly-rust = pkgs.rust-bin.nightly.${pkgs.lre.nightly-rust.meta.version};
          };
        };
        lre = {
          Env = with pkgs.lre;
            if pkgs.stdenv.isDarwin
            then lre-rs.meta.Env # C++ doesn't support Darwin yet.
            else (lre-cc.meta.Env ++ lre-rs.meta.Env);
          prefix =
            if pkgs.stdenv.isDarwin
            then "macos"
            else "linux";
        };
        nixos.path = with pkgs; [
          "/run/current-system/sw/bin"
          "${binutils.bintools}/bin"
          "${pkgs.lre.clang}/bin"
          "${git}/bin"

          # In the lre-rs image these are copied to `/bin` by the create-worker
          # function,
          #
          # Since we set `--incompatible_strict_action_env` in our .bazelrc we
          # default to `PATH=/bin:/usr/bin:/usr/local/bin` on non-NixOS systems.
          #
          # On NixOS we override that path with what we have in this list. We
          # could add `/bin` here, but using the explicit store paths adds
          # another layer of safety so that we don't mix local and remote tools
          # in cases where platform resolution doesn't behave as intended.
          #
          # Ideally, these shouldn't be in create-worker at all, and instead
          # should be their own lre-shell toolchain "below" lre-cc, rather than
          # a bolted-on-top layer in the final output.
          #
          # Note that these packages must be the same as the ones used in
          # `create-worker.nix`.
          "${bash}/bin"
          "${coreutils}/bin"
          "${gnused}/bin"
        ];
        devShells.default = pkgs.mkShell {
          packages = let
            bazel = pkgs.writeShellScriptBin "bazel" ''
              unset TMPDIR TMP
              exec ${pkgs.bazelisk}/bin/bazelisk "$@"
            '';
          in
            [
              # Development tooling
              pkgs.git
              pkgs.pre-commit
              pkgs.git-cliff
              pkgs.buck2

              # Rust
              bazel
              pkgs.lre.stable-rust
              pkgs.lre.lre-rs.lre-rs-configs-gen
              pkgs.rust-analyzer

              ## Infrastructure
              pkgs.awscli2
              pkgs.google-cloud-sdk
              pkgs.skopeo
              pkgs.dive
              pkgs.cosign
              pkgs.kubectl
              pkgs.kubernetes-helm
              pkgs.cilium-cli
              pkgs.vale
              pkgs.trivy
              pkgs.docker-client
              pkgs.kind
              pkgs.tektoncd-cli
              pkgs.pulumi
              pkgs.pulumiPackages.pulumi-go
              pkgs.fluxcd
              pkgs.go
              pkgs.kustomize
              pkgs.kubectx

              # Web
              pkgs.bun
              pkgs.lychee
              pkgs.nodejs_22 # For pagefind search
              pkgs.playwright-driver
              pkgs.playwright-test

              # Additional tools from within our development environment.
              build-chromium-tests
              docs
              generate-toolchains
              pkgs.lre.clang
              pkgs.nil
              pkgs.nixd
              pkgs.lre.lre-cc.lre-cc-configs-gen
              pkgs.nativelink-tools.local-image-test
              pkgs.nativelink-tools.native-cli
              pkgs.nativelink-tools.create-local-image

              # Tools for nix backend
              pkgs.protobuf
              pkgs.protoc-gen-rust
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
              pkgs.darwin.apple_sdk.frameworks.CoreFoundation
              pkgs.darwin.apple_sdk.frameworks.Security
              pkgs.libiconv
            ]
            ++ pkgs.lib.optionals (pkgs.stdenv.system != "x86_64-darwin") [
              # Old darwin systems are incompatible with deno.
              pkgs.deno
            ];

          shellHook =
            ''
              # Generate the .pre-commit-config.yaml symlink when entering the
              # development shell.
              ${config.pre-commit.installationScript}

              # Generate local-remote-execution.bazelrc which configures LRE toolchains when
              # running in the nix environment.
              ${config.lre.installationScript}

              # Generate nativelink.bazelrc which gives Bazel invocations access
              # to NativeLink's read-only cache.
              ${config.nativelink.installationScript}

              # If on NixOS, generate nixos.bazelrc, which adds the required
              # NixOS binary paths to the bazel environment.
              ${config.nixos.installationScript}

              # If on Darwin, generate darwin.bazelrc, which configures darwin
              # libs and frameworks.
              ${config.darwin.installationScript}

              # The Bazel and Cargo builds in nix require a Clang toolchain.
              # TODO(palfrey): The Bazel build currently uses the
              #                    irreproducible host C++ toolchain. Provide
              #                    this toolchain via nix for bitwise identical
              #                    binaries across machines.
              export CC=clang
            ''
            # TODO(palfrey): Generalize this.
            + pkgs.lib.optionalString (system == "x86_64-linux") ''
              export CC_x86_64_unknown_linux_gnu=customClang
            '';
        };
      };
    };
}
