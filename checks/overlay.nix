# Single overlay defining every nixception integration check.
#
# Each check is expressed as the *delta* over a real nixpkgs package wherever
# one exists:
#
#   recc-hello                    → hello
#   recc-spdlog                   → spdlog
#   recc-nix                      → nix   (via passthru.overrideAllMesonComponents)
#   protoc-gen-js-with-nixception → protoc-gen-js
#
# The remaining three (recc-smoke-test, bazel-rules-nixpkgs-hello,
# bazel-abseil-cpp) have no nixpkgs equivalent, but still route through the
# shared nixception wiring helpers below so the *only* code in each check is
# what is genuinely specific to that test.
#
# The shared wiring — recursive-nix system feature, the setup hook, the RECC_*
# endpoints, the bazel remote-exec flags, the build-timing snippet — lives in
# the `let` block once, so every per-check definition reads as a minimal diff.
#
# Consumed as the last overlay in the pkgset; the checks land under
# `pkgs.nixceptionChecks`.  The nixception server and its setup hook are the
# ones this repo's own flake.nix builds from local source (nixceptionFor /
# nixceptionHookFor), passed in as `nixceptionHook` below — not fetched from
# nixpkgs.
{nixceptionHook}: final: _prev: let
  inherit (final) lib;

  # ── extra sandbox tools ───────────────────────────────────────────────────
  # nixceptionHook has no package-injection API of its own (see
  # NIXCEPTION_EXTRA_SANDBOX_PATHS in nativelink-scheduler/src/runner_info.rs):
  # recc never uploads /nix/store paths, so a check's compiler/headers must
  # already exist in the *remote* runner sandbox, which only this env var (or
  # a store-path reference discoverable in the action itself) can reach — the
  # host build's PATH has no effect there. `withReccPackages` sets it from a
  # list of packages, replacing what used to be `nixceptionHook.withPackages`.
  withReccPackages = reccPackages: {
    NIXCEPTION_EXTRA_SANDBOX_PATHS = lib.concatMapStringsSep ":" toString reccPackages;
  };

  # ── shared wiring ─────────────────────────────────────────────────────────

  # recc client env: how any recc invocation (CC, cmake launcher, recc-gcc
  # wrapper) reaches the nixception server the hook starts.  Without the explicit
  # CAS / action-cache overrides recc defaults to localhost:8085.  Sources always
  # unpack under /build, so that is the project root.
  reccEnv = {
    RECC_INSTANCE = "main";
    RECC_SERVER = "127.0.0.1:50051";
    RECC_CAS_SERVER = "127.0.0.1:50051";
    RECC_ACTION_CACHE_SERVER = "127.0.0.1:50051";
    RECC_PROJECT_ROOT = "/build";
  };

  # Bazel flags that dispatch every spawn action to nixception via remote exec.
  bazelRemoteExecFlags = [
    "--remote_executor=grpc://127.0.0.1:50051"
    "--remote_instance_name=main"
    "--spawn_strategy=remote"
    "--noremote_local_fallback"
    "--remote_download_all"
    "--action_env=PATH"
    "--verbose_failures"
  ];

  # recc treats /nix/store include paths as "global" and never uploads them, so
  # the gcc-wrapper's NIX_* flags must be forwarded to the runner sandbox.  This
  # snippet (run in preConfigure) collects every NIX_* var and appends it to
  # RECC_ENV_TO_READ.  Only the "recc $CC" path (reccCompiler, used by recc-nix)
  # needs it — when recc runs the cc-wrapper remotely; the reccStdenv compiler
  # bakes flags onto argv locally instead.
  reccEnvToReadSnippet = ''
    nix_vars=$(env | sed -n 's/^\(NIX_[^=]*\)=.*/\1/p' | sort -u | tr '\n' ',')
    export RECC_ENV_TO_READ="PATH,SOURCE_DATE_EPOCH,''${nix_vars%,}"
    echo "nixception: RECC_ENV_TO_READ=$RECC_ENV_TO_READ" >&2
  '';

  # buildPhase timing, streamed to the build log for CI visibility.  Merges with
  # any existing pre/postBuild.
  buildTiming = old: {
    preBuild = (old.preBuild or "") + "\nBUILD_START=$SECONDS\n";
    postBuild =
      (old.postBuild or "")
      + ''
        BUILD_END=$SECONDS
        echo "buildPhase completed in $((BUILD_END - BUILD_START)) seconds"
      '';
  };

  # ── nixception backend wiring (shared core) ──────────────────────────────
  # Run the build through the nixception remote-execution backend: expose the
  # Nix daemon socket (recursive-nix), start the server via the hook (placing
  # `reccPackages` in the runner sandbox), configure recc to reach it (reccEnv +
  # RECC_ENV_TO_READ), time the build, and force verbose server logs.  This is
  # build-tool-agnostic: it does NOT decide the compiler — see reccCompiler.
  #
  # TEMP (OOM/retry experiment): NIXCEPTION_VERBOSE="1" streams the server's
  # retry events and in-flight gauge live into the build log.  Revert after.
  #
  # The shared core: nixceptionStdenvWrapper applies it through a wrapped
  # mkDerivation, and recc-nix applies it per meson component.  It reads its
  # argument's attrs with `or` defaults so it can merge onto any base.
  nixceptionWiring = {reccPackages ? []}: old:
    reccEnv
    // (buildTiming old)
    // (withReccPackages reccPackages)
    // {
      requiredSystemFeatures = (old.requiredSystemFeatures or []) ++ ["recursive-nix"];
      nativeBuildInputs = (old.nativeBuildInputs or []) ++ [nixceptionHook];
      NIXCEPTION_VERBOSE = "1";
    };

  # ── recc compiler routing, "recc $CC" form ───────────────────────────────
  # Prepend recc to the existing $CC/$CXX (the stdenv's cc-wrapper), so recc runs
  # *that wrapper* remotely.  Because the wrapper then reads its salted NIX_*
  # flags in the runner sandbox, RECC_ENV_TO_READ must forward them.
  #
  # The proper reccStdenv (below) instead bakes recc into the compiler itself, so
  # the wrapper runs locally and the flags land on argv — no env forwarding.
  # This form is kept only for recc-nix, whose meson components are built by a
  # fixed stdenv we cannot swap the cc on (the cc-wrapper salt must match).
  # The hook starts the server in a preConfigurePhase, so recc is reachable when
  # this preConfigure runs at the top of configurePhase.
  reccCompiler = old: {
    preConfigure =
      (old.preConfigure or "")
      + ''
        export CC="${final.buildbox}/bin/recc $CC"
        export CXX="${final.buildbox}/bin/recc $CXX"
      ''
      + reccEnvToReadSnippet;
  };

  # ── recc-wrapped compiler (the proper reccStdenv.cc) ─────────────────────
  # Mirrors nixpkgs' ccacheStdenv: replace the *unwrapped* compiler inside a
  # cc-wrapper with a tree whose compiler tools (cc/gcc/g++/…) are recc wrappers
  # — `recc <real-tool>` — and whose every other file (libs, libexec, include,
  # nix-support, the remaining bin/* tools) is symlinked straight from the real
  # unwrapped compiler.  Keeping the outer cc-wrapper means NIX_* handling and
  # the suffix-salt are unchanged; only the underlying binary now goes through
  # recc.  Because the wrapper runs locally, NIX flags are already on the command
  # line by the time recc dispatches the raw compiler to the runner.
  reccLinks = unwrappedCC:
    final.runCommand "${unwrappedCC.name}-recc" {
      passthru =
        {
          isClang = unwrappedCC.isClang or false;
          isGNU = unwrappedCC.isGNU or false;
        }
        // builtins.intersectAttrs {
          hardeningUnsupportedFlagsByTargetPlatform = null;
          hardeningUnsupportedFlags = null;
        }
        unwrappedCC;
      lib = lib.getLib unwrappedCC;
      nativeBuildInputs = [final.makeWrapper];
      meta = {inherit (unwrappedCC.meta) mainProgram;};
    } (
      let
        targetPrefix =
          lib.optionalString
          (unwrappedCC ? targetConfig && unwrappedCC.targetConfig != null && unwrappedCC.targetConfig != "")
          "${unwrappedCC.targetConfig}-";
      in ''
        mkdir -p $out/bin

        wrap() {
          local cname="${targetPrefix}$1"
          if [ -x "${unwrappedCC}/bin/$cname" ]; then
            makeWrapper ${final.buildbox}/bin/recc $out/bin/$cname \
              --add-flags ${unwrappedCC}/bin/$cname
          fi
        }

        wrap cc
        wrap c++
        wrap gcc
        wrap g++

        # Symlink every remaining tool and every non-bin file straight through.
        for executable in $(ls ${unwrappedCC}/bin); do
          [ -x "$out/bin/$executable" ] || ln -s ${unwrappedCC}/bin/$executable $out/bin/$executable
        done
        for file in $(ls ${unwrappedCC} | grep -vw bin); do
          ln -s ${unwrappedCC}/$file $out/$file
        done
      ''
    );

  # Turn a cc-wrapper into one whose underlying compiler routes through recc.
  reccWrapCC = cc: cc.override {cc = reccLinks cc.cc;};

  # ── stdenv adapters ──────────────────────────────────────────────────────
  # In the spirit of nixpkgs' stdenvAdapters (and Nix's own layered stdenvs),
  # two wrappers:
  #
  #   nixceptionStdenvWrapper  – build through the nixception backend (hook,
  #                              recursive-nix, runner sandbox, recc client env,
  #                              verbose, timing).  Compiler-agnostic; its cc is
  #                              the plain stdenv cc.
  #   reccStdenvWrapper        – the above, but with cc replaced by a recc-wrapped
  #                              compiler (reccWrapCC), so every compile routes
  #                              through recc with no per-build CC plumbing.
  #
  # Which packages land in the runner sandbox (so remote actions can find their
  # compiler + headers) is taken from the derivation's own arguments, resolved
  # against the fixpoint: pass an explicit `reccPackages` (in the mkDerivation
  # args or via a later overrideAttrs) to override, otherwise it defaults to the
  # build's final inputs — the stdenv compiler plus the dev outputs of
  # buildInputs + propagatedBuildInputs (recc never uploads /nix/store paths, so
  # they must already exist remotely).  An args-level `reccPackages` is stripped
  # so it never reaches the underlying derivation; one added via overrideAttrs
  # is honoured but, being the outermost layer, survives as a harmless env var.
  # Only VALUES read finalAttrs, so the attr-name set is fixpoint-safe.
  nixceptionStdenvWrapper = stdenv:
    stdenv
    // {
      mkDerivation = argsOrFn:
        stdenv.mkDerivation (finalAttrs: let
          base =
            if builtins.isFunction argsOrFn
            then argsOrFn finalAttrs
            else argsOrFn;
          reccPackages =
            finalAttrs.reccPackages
            or base.reccPackages
            or (
              [stdenv.cc]
              ++ map lib.getDev (
                lib.filter (x: x != null)
                ((finalAttrs.buildInputs or []) ++ (finalAttrs.propagatedBuildInputs or []))
              )
            );
        in
          (builtins.removeAttrs base ["reccPackages"])
          // (nixceptionWiring {inherit reccPackages;} base));
    };

  # recc on top of the nixception backend: swap the stdenv's cc for a recc-wrapped
  # one *before* applying the backend wrapper (overriding cc must happen on the
  # real stdenv; the backend wrapper is a `//` overlay that .override can't see
  # through).  reccStdenv.cc is therefore the recc-wrapped compiler.  As in
  # nixpkgs' overrideCC, allowedRequisites must be cleared: the recc-wrapped cc
  # drags the recc binary (buildbox) into the stdenv closure, which the bootstrap
  # stdenv's requisite allow-list would otherwise reject.
  reccStdenvWrapper = stdenv:
    nixceptionStdenvWrapper (stdenv.override {
      cc = reccWrapCC stdenv.cc;
      allowedRequisites = null;
    });

  # nixceptionStdenv: backend, plain compiler (used by the bespoke-compiler smoke
  # test).  reccStdenv: backend + recc as the compiler (autotools/cmake tests).
  nixceptionStdenv = nixceptionStdenvWrapper final.stdenv;
  reccStdenv = reccStdenvWrapper final.stdenv;

  # Common nixception bits for *bazel* tests: recursive-nix, the hook plus a
  # full `nix` + `cacert` in the build, and the recursive-nix daemon socket /
  # SSL certs wired into the environment.  Bazel reaches nixception over gRPC
  # (see bazelRemoteExecFlags), so no recc compiler wrapping is needed.
  bazelNixception = {reccPackages ? [final.gcc]}: old:
    (withReccPackages reccPackages)
    // {
      requiredSystemFeatures = (old.requiredSystemFeatures or []) ++ ["recursive-nix"];
      nativeBuildInputs =
        (old.nativeBuildInputs or [])
        ++ [
          nixceptionHook
          final.nix
          final.cacert
        ];
      NIXCEPTION_VERBOSE = "1";
      env =
        (old.env or {})
        // {
          NIX_REMOTE = "unix:///build/.nix-socket";
          NIX_SSL_CERT_FILE = "${final.cacert}/etc/ssl/certs/ca-bundle.crt";
          SSL_CERT_FILE = "${final.cacert}/etc/ssl/certs/ca-bundle.crt";
        };
    };

  # nixpkgs' bazel_8 build-support helper, used by the two custom bazel tests.
  bazelPackage = final.callPackage "${final.path}/pkgs/by-name/ba/bazel_8/build-support/bazelPackage.nix" {};

  # ── checks ────────────────────────────────────────────────────────────────
  checks = {
    # recc + tiny hand-rolled C++ project.  A g++ wrapper sleeps 10s on real
    # compiles (but not on recc's local dep-scan invocations), making uncached
    # remote runs visibly slower than cached ones.  No nixpkgs equivalent.
    recc-smoke-test = let
      gppSleeper = final.writeShellScriptBin "g++" ''
        case " $* " in
          *\ -M\ *|*\ -MM\ *|*\ -MF\ *|*\ -MD\ *|*\ -MMD\ *)
            ;;
          *)
            sleep 10
            ;;
        esac
        exec ${final.gcc}/bin/g++ "$@"
      '';
    in
      # nixceptionStdenv (backend only): the compiler is bespoke (gppSleeper),
      # so we route CC ourselves rather than via reccStdenv.
      nixceptionStdenv.mkDerivation {
        # gppSleeper (not a buildInput) is the only thing the runner needs, so
        # set reccPackages explicitly rather than letting it default to inputs.
        reccPackages = [gppSleeper];
        name = "recc-recursive-nix-test";
        src = ./recc/smoke-test;
        enableParallelBuilding = true;

        # cc-wrapper's setup hook unconditionally exports CC=gcc/CXX=g++, so we
        # re-export in preConfigure (after all setup hooks).  The multi-word
        # value word-splits like "ccache g++" → recc invoked with g++.
        preConfigure = ''
          export CC="${final.buildbox}/bin/recc ${gppSleeper}/bin/g++"
          export CXX="${final.buildbox}/bin/recc ${gppSleeper}/bin/g++"
        '';

        makeFlags = ["CPPFLAGS=-DBUILD_CONSTANT=42"];
        doCheck = true;
        installPhase = ''
          runHook preInstall
          mkdir -p $out
          cp demo_app $out/
          echo "Test passed" > $out/result.txt
          runHook postInstall
        '';
      };

    # The real nixpkgs GNU Hello.  reccStdenv's cc is recc-wrapped, so autotools
    # picks it up as $CC and every compile (configure probes included — the
    # server is up by preConfigurePhase) routes through recc.  Nothing else is
    # test-specific: the whole delta is the stdenv.
    recc-hello = final.hello.override {stdenv = reccStdenv;};

    # The real nixpkgs spdlog (CMake).  reccStdenv's cc is itself recc-wrapped,
    # so cmake picks it up as the compiler and every compile routes through recc
    # with no launcher flags or CC plumbing.  nixpkgs already pins the same
    # src/version and SPDLOG_BUILD_TESTS=ON etc.; the delta is just -static-libgcc
    # (keep bootstrap libgcc out of the output) and the remote-env debug flag.
    # The default reccPackages covers the runner: the (recc-wrapped) compiler,
    # whose closure carries the real gcc, plus dev outputs of buildInputs
    # (catch2_3) and propagatedBuildInputs (fmt).
    recc-spdlog = (final.spdlog.override {stdenv = reccStdenv;})
      .overrideAttrs (_: {
      NIX_CFLAGS_COMPILE = "-static-libgcc";
      RECC_REMOTE_ENV_NIX_DEBUG = "1";
    });

    # The whole nixpkgs Nix package set (all ~20 meson components), each compile
    # dispatched through recc.  overrideAllMesonComponents applies the overlay
    # below to every component; we build .nix-cli (links every Nix lib, so all
    # components compile).  See the long-form rationale that previously lived in
    # recc/nix.nix for the mesonBuildType="plain" (LTO off / no -g) choice.
    recc-nix = let
      # Same nixceptionWiring + reccCompiler functions as reccStdenv, but applied
      # per meson component (overrideAllMesonComponents takes an overlay, not a
      # stdenv, so the adapters can't be used directly here).  The hook injects,
      # per component, the compiler + the dev output of every dependency into the
      # runner sandbox so remote compiles find their (non-uploaded) headers.
      reccOverlay = _finalAttrs: prevAttrs: let
        deps =
          lib.filter (x: x != null)
          ((prevAttrs.buildInputs or []) ++ (prevAttrs.propagatedBuildInputs or []));
        backend = nixceptionWiring {reccPackages = [final.stdenv.cc] ++ map lib.getDev deps;} prevAttrs;
      in
        backend
        # reccCompiler appends the CC/CXX recc routing onto the backend's
        # preConfigure (which already carries the RECC_ENV_TO_READ snippet).
        // (reccCompiler backend)
        // {
          # "plain": LTO off (so remote actions do real codegen, not just GIMPLE)
          # AND no -g (so cached .o objects don't embed -dev header paths, which
          # would pin the whole component closure to the recc cache).
          mesonBuildType = "plain";
          separateDebugInfo = false;
        };
    in
      # nix-cli is itself a component, so it already carries the reccOverlay
      # wiring (recursive-nix, hook, RECC env, verbose, per-component timing);
      # here we only rename it and sharpen the description.
      (final.nix.overrideAllMesonComponents reccOverlay).nix-cli.overrideAttrs (old: {
        pname = "nix-cli-recc-test";
        meta = (old.meta or {}) // {description = "Integration-test: build the whole Nix package through recc + nixception";};
      });

    # A C++ hello-world built via Bazel 8 + rules_nixpkgs + nixception (gRPC
    # remote exec).  rules_nixpkgs calls nix-build to configure the CC toolchain
    # (needs recursive-nix); compile actions are dispatched to nixception.  The
    # repo-cache FOD only needs network access, so the nixception wiring is added
    # via overrideAttrs (which only touches the outer build, not the FOD).
    bazel-rules-nixpkgs-hello =
      (bazelPackage {
        name = "bazel-nixpkgs-cc-hello-nixception-test";
        src = ./bazel/rules-nixpkgs-hello;
        registry = final.fetchFromGitHub {
          owner = "bazelbuild";
          repo = "bazel-central-registry";
          rev = "722299976c97e5191045c8016b7c8532189fc3f6";
          hash = "sha256-hi5BKI94am2LCXD93GBeT0gsODxGeSsd0OrhTwpNAgM=";
        };
        targets = ["//src:hello-world"];
        bazel = final.bazel_8;
        commandArgs = bazelRemoteExecFlags ++ ["--jobs=4" "--subcommands"];
        installPhase = ''
          runHook preInstall
          mkdir -p $out/bin
          cp bazel-bin/src/hello-world $out/bin/
          chmod +x $out/bin/hello-world
          echo "Running hello-world binary to verify correctness..."
          output=$($out/bin/hello-world)
          echo "Output: $output"
          if echo "$output" | grep -q "Hello world"; then
            echo "SUCCESS: hello-world produced expected output"
          else
            echo "FAIL: unexpected output from hello-world: $output"
            exit 1
          fi
          echo "Test passed" > $out/result.txt
          runHook postInstall
        '';
        bazelRepoCacheFOD = {
          outputHash = "sha256-50gtbhmbIw8TyDYsVmwVGNJ7qek5GYf7k0SjMjU3tT4=";
          outputHashAlgo = "sha256";
        };
      }).overrideAttrs (old:
        (bazelNixception {} old)
        // {
          meta = {
            description = "Integration-test: build a C++ hello-world through Bazel 8 + rules_nixpkgs + nixception";
            license = lib.licenses.asl20;
            maintainers = with lib.maintainers; [layus];
            platforms = lib.platforms.linux;
          };
        });

    # abseil-cpp //... via Bazel 8 + rules_nixpkgs + nixception.  Uses a fork
    # that adds bzlmod + rules_nixpkgs CC toolchain config.  The nixpkgs tarball
    # is pre-downloaded and exposed via --distdir for the air-gapped final build
    # (the FOD fetches over plain HTTP; fixed-output derivations inside
    # recursive-nix have no network).
    bazel-abseil-cpp = let
      nixpkgsTarball = final.fetchurl {
        url = "https://github.com/NixOS/nixpkgs/archive/refs/tags/25.11.tar.gz";
        sha256 = "bcc12f1c35344a6b5c1f3319923e6d7317cd5f52ad7126e0a5f32e08cbb0a213";
      };
      nixpkgsDistdir = final.runCommand "nixpkgs-distdir" {} ''
        mkdir -p $out
        ln -s ${nixpkgsTarball} $out/25.11.tar.gz
      '';
    in
      (bazelPackage {
        name = "abseil-cpp-nixception-test";
        src = final.fetchFromGitHub {
          owner = "layus";
          repo = "abseil-cpp";
          rev = "8d39d9632bcd8261b66093cfb9cc6071f1e3985e";
          hash = "sha256-OSSrh1Ljxa/zlcPtdx5t9+hCsHwwRLYMn1ovQMQWg8A=";
        };
        registry = final.fetchFromGitHub {
          owner = "bazelbuild";
          repo = "bazel-central-registry";
          rev = "566fb61e2d81fc2ec33fc625566a44d4eb618c68";
          hash = "sha256-vuUK3nP35G1Xndb390PBTu/du0J3PFB5IWEitQ9brnc=";
        };
        targets = ["//..."];
        bazel = final.bazel_8;
        commandArgs = bazelRemoteExecFlags ++ ["--lockfile_mode=off"];
        installPhase = ''
          runHook preInstall

          # --- Headers (mirrors nixpkgs: include/absl/…) ---
          mkdir -p $out/include
          cp -r absl $out/include/absl
          find $out/include -type f \
            ! -name '*.h' ! -name '*.inc' \
            -delete
          find $out/include -type d -empty -delete

          # --- Static libraries (mirrors nixpkgs: lib/libabsl_*.{a,so}) ---
          mkdir -p $out/lib
          find bazel-out/k8-fastbuild/bin/absl -name 'lib*.a' \
              -not -path '*test*' -not -path '*benchmark*' \
            | while IFS= read -r src; do
              rel="''${src#bazel-out/k8-fastbuild/bin/absl/}"
              dir="$(dirname "$rel")"
              base="$(basename "$rel")"
              name="''${base#lib}"
              name="''${name%.a}"
              pkg="$(echo "$dir" | tr '/' '_')"
              last="''${dir##*/}"
              if [ "$last" = "$name" ]; then
                dst="libabsl_''${pkg}.a"
              else
                dst="libabsl_''${pkg}_''${name}.a"
              fi
              cp "$src" "$out/lib/$dst"
            done

          runHook postInstall
        '';
        bazelRepoCacheFOD = {
          outputHash = "sha256-7mPbhXW1BLFTNtk03ZDz1JpSaEK/tB+xO2Z4aaZc0Ms=";
          outputHashAlgo = "sha256";
        };
      }).overrideAttrs (old:
        (bazelNixception {} old)
        // {
          # These flags live in .bazelrc.user so they only affect the final
          # build, not the FOD (which has no nix daemon to resolve the CC
          # toolchain).
          postPatch =
            (old.postPatch or "")
            + ''
              rm -f MODULE.bazel.lock
              cat >> .bazelrc.user <<EOF
              build --host_platform=@rules_nixpkgs_core//platforms:host
              build --extra_execution_platforms=@rules_nixpkgs_core//platforms:host
              build --distdir=${nixpkgsDistdir}
              EOF
            '';
          meta = {
            description = "Integration test: build abseil-cpp //... via Bazel 8 + rules_nixpkgs + nixception";
            license = lib.licenses.asl20;
            maintainers = with lib.maintainers; [layus];
            platforms = lib.platforms.linux;
          };
        });

    # The upstream nixpkgs protoc-gen-js, with remote-exec flags injected into
    # the *final* build only.  buildBazelPackage splits into a deps FOD (network,
    # no nixception) and an air-gapped build; overrideAttrs touches only the
    # latter, and the flags are appended to .bazelrc in postPatch (buildBazelPackage
    # bakes bazelBuildFlags at eval time, so we cannot override those directly).
    protoc-gen-js-with-nixception = let
      protoc = final.callPackage "${final.path}/pkgs/by-name/pr/protoc-gen-js/package.nix" {
        inherit (final) bazel_7;
      };
      remoteExecFlags = bazelRemoteExecFlags ++ ["--jobs=4" "--subcommands"];
      bazelrcLines = lib.concatMapStringsSep "\n" (f: "build ${f}") remoteExecFlags;
    in
      protoc.overrideAttrs (old:
        (bazelNixception {} old)
        // {
          postPatch =
            (old.postPatch or "")
            + ''
              cat >> .bazelrc <<'EOF'
              ${bazelrcLines}
              EOF
            '';
          meta =
            (old.meta or {})
            // {
              description = "Integration-test: build protoc-gen-js via Bazel + nixception (remote exec)";
              platforms = lib.platforms.linux;
            };
        });
  };
in {
  nixceptionChecks = checks;
}
