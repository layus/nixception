{
  # Integration test: build spdlog via recc + nixception.
  #
  # Modeled after integration_tests/recc/zlib-with-nixception.nix but adapted for
  # a CMake-based project (spdlog). The cmake configure step is delayed until
  # the preBuildPhase (where the nixception hook brings the server up) by
  # setting `doConfigure = false` and running the configure step inside
  # `buildPhase`. This ensures calls to the remote compiler (recc) happen only
  # after the server is listening.
  #
  # Called from the top-level flake, e.g.:
  #
  #   spdlog-nixception-recc-test = pkgs.callPackage
  #     integration_tests/recc/spdlog-with-nixception.nix {
  #       inherit nixceptionHook buildbox;
  #     };
  #
  nixceptionHook,
  buildbox, # provides the `recc` binary
  gcc,
  stdenv,
  fetchFromGitHub,
  writeShellScriptBin,
  cmake,
  fmt,
  catch2_3,
  lib,
  ninja,
}: let
  # Wrap `recc <compiler>` in a single-word executable so CMake doesn't see a
  # multi-word compiler path (which would be split).
  reccGcc = writeShellScriptBin "recc-gcc" ''
    exec ${buildbox}/bin/recc ${gcc}/bin/gcc "$@"
  '';

  reccGxx = writeShellScriptBin "recc-g++" ''
    exec ${buildbox}/bin/recc ${gcc}/bin/g++ "$@"
  '';
in
  stdenv.mkDerivation (finalAttrs: {
      pname = "spdlog";
      version = "1.17.0";

      # Source: matches the spdlog derivation used elsewhere in the tree.
      src = fetchFromGitHub {
        owner = "gabime";
        repo = "spdlog";
        tag = "v${finalAttrs.version}";
        # same hash as in the primary spdlog derivation
        hash = "sha256-bL3hQmERXNwGmDoi7+wLv/TkppGhG6cO47k1iZvJGzY=";
      };

      # Expose the local Nix daemon for recursive-nix remote execution.
      requiredSystemFeatures = ["recursive-nix"];

      # Inject the nixception hook (it registers a preBuildPhase that starts the
      # server and registers the necessary exit/failure hooks to stop it).
      nativeBuildInputs = [
        (nixceptionHook.withPackages [gcc fmt.dev catch2_3]) # inject fmt and catch2 into the runner sandbox for use during build/check
        cmake
        ninja
      ];

      # Dependencies needed to build spdlog and run its tests.
      buildInputs = [
        fmt
        catch2_3
      ];

      outputs = ["out" "dev"];

      # CMake options we want to pass. We'll apply these during the manual cmake
      # configure invocation inside `buildPhase`.
      cmakeFlags = [
        "-DSPDLOG_BUILD_SHARED=ON"
        "-DSPDLOG_BUILD_STATIC=OFF"
        "-DSPDLOG_BUILD_EXAMPLE=OFF"
        "-DSPDLOG_BUILD_BENCH=OFF"
        "-DSPDLOG_BUILD_TESTS=ON"
        "-DSPDLOG_FMT_EXTERNAL=ON"
        "-DCMAKE_C_COMPILER=${reccGcc}/bin/recc-gcc"
        "-DCMAKE_CXX_COMPILER=${reccGxx}/bin/recc-g++"
      ];

      # Avoid accidental references to bootstrap libgcc in outputs.
      env = {
        NIX_CFLAGS_COMPILE = "-static-libgcc";

        # RECC environment: points recc at the nixception server that the hook
        # starts. These are exported into the build environment so recc can find the
        # CAS / action-cache endpoints.
        RECC_VERBOSE = "1";
        RECC_LOG_PROGRESS = "1";
        RECC_INSTANCE = "main";
        RECC_SERVER = "127.0.0.1:50051";
        RECC_CAS_SERVER = "127.0.0.1:50051";
        RECC_ACTION_CACHE_SERVER = "127.0.0.1:50051";
        RECC_FALLBACK_TO_LOCAL = "1";
        RECC_PROJECT_ROOT = "/build";
        RECC_ENV_TO_READ = "PATH,NIX_CFLAGS_COMPILE,NIX_LDFLAGS,NIX_HARDENING_ENABLE,NIX_BINTOOLS_WRAPPER_TARGET_HOST_x86_64_unknown_linux_gnu,SOURCE_DATE_EPOCH";
        RECC_REMOTE_ENV_NIX_DEBUG = "1";
      };

      strictDeps = true;

      # Build timing helpers for CI visibility.
      preBuild = "
    BUILD_START=$SECONDS
    unset RECC_FALLBACK_TO_LOCAL
    set -x
  ";
      postBuild = ''
        BUILD_END=$SECONDS
        echo "buildPhase completed in $((BUILD_END - BUILD_START)) seconds"
      '';

      doCheck = true;

      meta = {
        description = "Integration-test: build spdlog through recc + nixception (CMake)";
        homepage = "https://github.com/gabime/spdlog";
        license = lib.licenses.mit;
        maintainers = with lib.maintainers; [obadz];
        platforms = lib.platforms.linux;
      };
    })
