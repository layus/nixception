# Bazel demos for nixception.
#
# Where the reccStdenv demos swap a package's compiler for a recc-wrapped one,
# Bazel talks to nixception directly: `--remote_executor=grpc://…` dispatches
# every spawn (compile, link) to the nixception server as a Remote Execution
# action, which nixception turns into a Nix derivation built via recursive-nix.
#
# These are ports of the `bazel-*` checks in workspace/checks/overlay.nix, made
# self-contained (they consume only the pinned nixpkgs fork, no sibling paths).
#
# Each abseil target comes in two forms — `bazel-abseil-cpp` (compiles on
# nixception) and `vanilla-bazel-abseil-cpp` (compiles locally) — built from one
# shared `mkAbseil` so a timing comparison isolates nixception, not the build
# system. Both need recursive-nix (rules_nixpkgs configures the CC toolchain via
# nix-build) and network for the Bazel-dependency FOD (fetched once, cached).
{ pkgs }:
let
  inherit (pkgs) lib;

  nixceptionHook = pkgs.nixceptionHook;

  # Bazel flags that route every action to the nixception server the hook
  # starts on the sandbox loopback. `--noremote_local_fallback` makes a routing
  # failure loud rather than silently building locally.
  bazelRemoteExecFlags = [
    "--remote_executor=grpc://127.0.0.1:50051"
    "--remote_instance_name=main"
    "--spawn_strategy=remote"
    "--noremote_local_fallback"
    "--remote_download_all"
    "--action_env=PATH"
    "--verbose_failures"
  ];

  # nixpkgs' bazel_8 build-support helper: splits into a network-only
  # dependency FOD and an air-gapped build. We add the nixception wiring to the
  # latter only (via overrideAttrs), so the FOD stays a plain fetch.
  bazelPackage = pkgs.callPackage
    "${pkgs.path}/pkgs/by-name/ba/bazel_8/build-support/bazelPackage.nix"
    { };

  # Full sandbox wiring for a rules_nixpkgs Bazel build, as an overrideAttrs
  # function.
  #
  # rules_nixpkgs' cc_configure extension runs `nix-build` to materialize the CC
  # toolchain, so *every* build here — nixception or not — needs a Nix daemon in
  # the sandbox: the recursive-nix feature, `nix` on PATH, the daemon socket,
  # SSL certs. `nixception = true` adds, on top: the setup hook (starts/stops
  # the server), verbose server logs, and the CC toolchain made visible inside
  # the REAPI action sandbox via `NIXCEPTION_EXTRA_SANDBOX_PATHS`. Bazel reaches
  # the server over gRPC (see bazelRemoteExecFlags) — no compiler wrapping.
  bazelWiring =
    { nixception, extraSandboxPackages ? [ pkgs.gcc ] }:
    old: {
      requiredSystemFeatures = (old.requiredSystemFeatures or [ ]) ++ [ "recursive-nix" ];
      nativeBuildInputs =
        (old.nativeBuildInputs or [ ])
        ++ [ pkgs.nix pkgs.cacert ]
        ++ lib.optional nixception nixceptionHook;
      env = (old.env or { }) // {
        NIX_REMOTE = "unix:///build/.nix-socket";
        NIX_SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
        SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
      };
    }
    // lib.optionalAttrs nixception {
      NIXCEPTION_VERBOSE = "1";
      NIXCEPTION_EXTRA_SANDBOX_PATHS =
        lib.concatStringsSep ":" (map toString extraSandboxPackages);
    };

  # A C++ hello-world built with Bazel 8 + rules_nixpkgs, every compile action
  # dispatched to nixception over gRPC. rules_nixpkgs runs nix-build to
  # configure the CC toolchain (hence recursive-nix); the compile/link spawns
  # are the Remote Execution actions.
  bazel-hello-world =
    (bazelPackage {
      name = "nixception-demo-bazel-hello-world";
      src = ./hello-world;
      registry = pkgs.fetchFromGitHub {
        owner = "bazelbuild";
        repo = "bazel-central-registry";
        rev = "722299976c97e5191045c8016b7c8532189fc3f6";
        hash = "sha256-hi5BKI94am2LCXD93GBeT0gsODxGeSsd0OrhTwpNAgM=";
      };
      targets = [ "//src:hello-world" ];
      bazel = pkgs.bazel_8;
      commandArgs = bazelRemoteExecFlags ++ [ "--jobs=4" "--subcommands" ];
      installPhase = ''
        runHook preInstall
        mkdir -p $out/bin
        cp bazel-bin/src/hello-world $out/bin/
        chmod +x $out/bin/hello-world
        echo "running the nixception-built hello-world..."
        output=$($out/bin/hello-world)
        echo "  output: $output"
        echo "$output" | grep -q "Hello world" \
          || { echo "FAIL: unexpected output: $output"; exit 1; }
        echo "ok: Bazel + nixception hello-world runs"
        runHook postInstall
      '';
      bazelRepoCacheFOD = {
        outputHash = "sha256-50gtbhmbIw8TyDYsVmwVGNJ7qek5GYf7k0SjMjU3tT4=";
        outputHashAlgo = "sha256";
      };
    }).overrideAttrs (old:
      (bazelWiring { nixception = true; } old) // {
        meta = (old.meta or { }) // {
          description = "nixception demo: C++ hello-world via Bazel 8 + rules_nixpkgs + nixception (gRPC remote exec)";
          platforms = lib.platforms.linux;
        };
      });

  # The slow-compile demo's expensive translation unit (a ~550k-element
  # designated aggregate initializer — see demo/flake.nix's slowCompileSrc and
  # gen.py), compiled via Bazel + rules_nixpkgs instead of a recc-wrapped
  # stdenv compiler. slow_table.c and main.c here are copies of
  # demo/slow-compile/{slow_table.c,main.c}, kept in sync by hand (Bazel needs
  # its own BUILD/MODULE.bazel alongside them, so this can't just point at the
  # other demo's source tree the way reccStdenv does via slowCompileSrc).
  slow-bazel =
    (bazelPackage {
      name = "nixception-demo-slow-bazel";
      src = ./slow-compile;
      registry = pkgs.fetchFromGitHub {
        owner = "bazelbuild";
        repo = "bazel-central-registry";
        rev = "722299976c97e5191045c8016b7c8532189fc3f6";
        hash = "sha256-hi5BKI94am2LCXD93GBeT0gsODxGeSsd0OrhTwpNAgM=";
      };
      targets = [ "//src:slow_demo" ];
      bazel = pkgs.bazel_8;
      commandArgs = bazelRemoteExecFlags ++ [ "--jobs=4" "--subcommands" ];
      installPhase = ''
        runHook preInstall
        mkdir -p $out/bin
        cp bazel-bin/src/slow_demo $out/bin/
        chmod +x $out/bin/slow_demo
        echo "running the nixception-built slow_demo..."
        $out/bin/slow_demo
        runHook postInstall
      '';
      bazelRepoCacheFOD = {
        outputHash = "sha256-50gtbhmbIw8TyDYsVmwVGNJ7qek5GYf7k0SjMjU3tT4=";
        outputHashAlgo = "sha256";
      };
    }).overrideAttrs (old:
      (bazelWiring { nixception = true; } old) // {
        meta = (old.meta or { }) // {
          description = "nixception demo: the slow-compile translation unit via Bazel 8 + rules_nixpkgs + nixception (gRPC remote exec)";
          platforms = lib.platforms.linux;
        };
      });

  # abseil-cpp `//...` via Bazel 8 + rules_nixpkgs — hundreds of C++ compile
  # actions. Uses a fork that adds bzlmod + rules_nixpkgs CC-toolchain config.
  # The nixpkgs tarball is pre-fetched and handed to Bazel via --distdir (the
  # inner build is air-gapped: rules_nixpkgs fetches over plain HTTP, and
  # fixed-output derivations under recursive-nix have no network).
  #
  # `nixception = true`  → compile actions dispatched to the server over gRPC.
  # `nixception = false` → compiles run locally (the baseline). Everything else
  # — source, registry, targets, install layout, the toolchain/distdir
  # .bazelrc.user lines, the recursive-nix requirement — is identical.
  mkAbseil =
    { nixception }:
    let
      nixpkgsTarball = pkgs.fetchurl {
        url = "https://github.com/NixOS/nixpkgs/archive/refs/tags/25.11.tar.gz";
        sha256 = "bcc12f1c35344a6b5c1f3319923e6d7317cd5f52ad7126e0a5f32e08cbb0a213";
      };
      nixpkgsDistdir = pkgs.runCommand "nixpkgs-distdir" { } ''
        mkdir -p $out
        ln -s ${nixpkgsTarball} $out/25.11.tar.gz
      '';
      base = bazelPackage {
        name =
          if nixception then "nixception-demo-bazel-abseil-cpp" else "vanilla-bazel-abseil-cpp";
        src = pkgs.fetchFromGitHub {
          owner = "layus";
          repo = "abseil-cpp";
          rev = "8d39d9632bcd8261b66093cfb9cc6071f1e3985e";
          hash = "sha256-OSSrh1Ljxa/zlcPtdx5t9+hCsHwwRLYMn1ovQMQWg8A=";
        };
        registry = pkgs.fetchFromGitHub {
          owner = "bazelbuild";
          repo = "bazel-central-registry";
          rev = "566fb61e2d81fc2ec33fc625566a44d4eb618c68";
          hash = "sha256-vuUK3nP35G1Xndb390PBTu/du0J3PFB5IWEitQ9brnc=";
        };
        targets = [ "//..." ];
        bazel = pkgs.bazel_8;
        commandArgs = (lib.optionals nixception bazelRemoteExecFlags) ++ [ "--lockfile_mode=off" ];
        installPhase = ''
          runHook preInstall

          # Headers (mirrors nixpkgs: include/absl/…)
          mkdir -p $out/include
          cp -r absl $out/include/absl
          find $out/include -type f ! -name '*.h' ! -name '*.inc' -delete
          find $out/include -type d -empty -delete

          # Static libraries (mirrors nixpkgs: lib/libabsl_*.a)
          mkdir -p $out/lib
          find bazel-out/k8-fastbuild/bin/absl -name 'lib*.a' \
              -not -path '*test*' -not -path '*benchmark*' \
            | while IFS= read -r src; do
              rel="''${src#bazel-out/k8-fastbuild/bin/absl/}"
              dir="$(dirname "$rel")"; base="$(basename "$rel")"
              name="''${base#lib}"; name="''${name%.a}"
              pkg="$(echo "$dir" | tr '/' '_')"
              last="''${dir##*/}"
              if [ "$last" = "$name" ]; then dst="libabsl_''${pkg}.a"; else dst="libabsl_''${pkg}_''${name}.a"; fi
              cp "$src" "$out/lib/$dst"
            done

          runHook postInstall
        '';
        bazelRepoCacheFOD = {
          outputHash = "sha256-7mPbhXW1BLFTNtk03ZDz1JpSaEK/tB+xO2Z4aaZc0Ms=";
          outputHashAlgo = "sha256";
        };
      };
    in
    base.overrideAttrs (old:
      (bazelWiring { inherit nixception; } old)
      // {
        # Toolchain/distdir flags go in .bazelrc.user so they touch the final
        # build only, not the dependency FOD (which has no nix daemon).
        postPatch = (old.postPatch or "") + ''
          rm -f MODULE.bazel.lock
          cat >> .bazelrc.user <<EOF
          build --host_platform=@rules_nixpkgs_core//platforms:host
          build --extra_execution_platforms=@rules_nixpkgs_core//platforms:host
          build --distdir=${nixpkgsDistdir}
          EOF
        '';
        meta = (old.meta or { }) // {
          description =
            if nixception then
              "nixception demo: build abseil-cpp //... via Bazel 8 + rules_nixpkgs + nixception"
            else
              "baseline: build abseil-cpp //... via Bazel 8 + rules_nixpkgs, compiles run locally";
          platforms = lib.platforms.linux;
        };
      });
in
{
  inherit bazel-hello-world slow-bazel;

  bazel-abseil-cpp = mkAbseil { nixception = true; };

  # Same Bazel build path, compiles run locally — the baseline for
  # bazel-abseil-cpp. Exposed by the flake as `vanilla-bazel-abseil-cpp`.
  vanilla-bazel-abseil-cpp = mkAbseil { nixception = false; };
}
