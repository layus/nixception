{
  # Integration test: build spdlog via recc + nixception.
  #
  # Modeled after integration_tests/recc/zlib-with-nixception.nix but adapted for
  # a CMake-based project (spdlog).  CMake probes the compiler during
  # configurePhase, so the nixception server must already be listening at that
  # point.  The nixception hook registers nixceptionStartPhase as a
  # preConfigurePhase, which guarantees the server is up before cmake runs.
  #
  # CMAKE_C_COMPILER_LAUNCHER / CMAKE_CXX_COMPILER_LAUNCHER tell CMake to
  # prepend recc before every compiler invocation — the native CMake mechanism
  # for compiler-wrapper tools (ccache, distcc, recc, …).  The compiler
  # itself remains the stdenv gcc-wrapper (picked up from CC/CXX by the cmake
  # hook).  The resulting command lines look like:
  #
  #   recc /nix/store/.../gcc-wrapper/bin/g++ … -c foo.cpp -o foo.o
  #
  # The gcc-wrapper reads NIX_CFLAGS_COMPILE at invocation time and adds
  # -isystem flags for Nix-provided dependencies (fmt, catch2, …).  For this
  # to work on the remote side (inside the runner sandbox), two things are
  # needed:
  #
  #   1. RECC_ENV_TO_READ must include NIX_CFLAGS_COMPILE so that recc
  #      forwards the include-path flags to the remote environment.
  #
  #   2. RECC_ENV_TO_READ must also include
  #      NIX_CC_WRAPPER_TARGET_HOST_x86_64_unknown_linux_gnu — the gcc-wrapper's
  #      add-flags.sh calls accumulateRoles() which checks this variable to
  #      determine the active role suffixes.  Without it, role_suffixes is
  #      empty and mangleVarList never copies NIX_CFLAGS_COMPILE into the
  #      salt-suffixed variable the wrapper actually reads.
  #
  # No wrapper scripts are needed.
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
  cmake,
  fmt,
  catch2_3,
  lib,
  ninja,
}:
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

  # Inject the nixception hook (it registers a preConfigurePhase that starts
  # the server and registers the necessary exit/failure hooks to stop it).
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

  # ── compiler launcher ──────────────────────────────────────────────────
  # CMAKE_C_COMPILER_LAUNCHER / CMAKE_CXX_COMPILER_LAUNCHER tell CMake to
  # prepend recc before every compiler invocation.  The compiler itself
  # remains the stdenv gcc-wrapper (picked up from CC/CXX by the cmake
  # hook).  The gcc-wrapper reads NIX_CFLAGS_COMPILE at invocation time
  # and adds -isystem flags for Nix-provided dependencies — both locally
  # (for recc's dependency scanning) and remotely (in the runner sandbox,
  # where recc forwards NIX_CFLAGS_COMPILE via RECC_ENV_TO_READ).
  cmakeFlags = [
    "-DSPDLOG_BUILD_SHARED=ON"
    "-DSPDLOG_BUILD_STATIC=OFF"
    "-DSPDLOG_BUILD_EXAMPLE=OFF"
    "-DSPDLOG_BUILD_BENCH=OFF"
    "-DSPDLOG_BUILD_TESTS=ON"
    "-DSPDLOG_FMT_EXTERNAL=ON"
    "-DCMAKE_C_COMPILER_LAUNCHER=${buildbox}/bin/recc"
    "-DCMAKE_CXX_COMPILER_LAUNCHER=${buildbox}/bin/recc"
  ];

  # ── RECC_ENV_TO_READ: forward all NIX_* env vars ──────────────────────
  # The gcc-wrapper and binutils-wrapper read a large set of NIX_*
  # environment variables at invocation time (NIX_CFLAGS_COMPILE,
  # NIX_LDFLAGS, NIX_CC_WRAPPER_TARGET_HOST_…, etc.).  Rather than
  # maintaining a fragile hardcoded list, we collect every NIX_* variable
  # from the build environment at configure time and append them to
  # RECC_ENV_TO_READ so recc forwards them all to the remote runner.
  preConfigure = ''
    nix_vars=$(env | sed -n 's/^\(NIX_[^=]*\)=.*/\1/p' | sort -u | tr '\n' ',')
    export RECC_ENV_TO_READ="PATH,SOURCE_DATE_EPOCH,''${nix_vars%,}"
    echo "nixception: RECC_ENV_TO_READ=$RECC_ENV_TO_READ" >&2
  '';

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
    RECC_PROJECT_ROOT = "/build";
    RECC_REMOTE_ENV_NIX_DEBUG = "1";
  };

  strictDeps = true;

  # Build timing helpers for CI visibility.
  preBuild = "
    BUILD_START=$SECONDS
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
