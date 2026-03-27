{
  nixception,
  buildbox,
  gcc,
  wait4x,
  coreutils,
  writeShellScriptBin,
}:
writeShellScriptBin "recc-with-nativelink-test" ''
  set -euo pipefail

  cleanup() {
    local pids=$(jobs -pr)
    [ -n "$pids" ] && kill $pids
  }
  trap "cleanup" INT QUIT TERM EXIT

  # Remove any stale output from a previous run.
  ${coreutils}/bin/rm -f integration_tests/recc/test/main.o

  RUST_BACKTRACE=1 ${nixception}/bin/nixception 2>&1 | tee -i integration_tests/recc/nativelink.log &

  ${wait4x}/bin/wait4x tcp 127.0.0.1:50051 --timeout 30s --quiet

  recc_output=$(
    cd integration_tests/recc && \
    env \
      RECC_VERBOSE=1 \
      RECC_LOG_PROGRESS=1 \
      RECC_INSTANCE=main \
      RECC_SERVER=127.0.0.1:50051 \
      ${buildbox}/bin/recc \
        ${gcc}/bin/g++ -DBUILD_CONSTANT=42 -c test/main.cpp -o test/main.o \
      2>&1 | tee -i recc.log
  )

  echo "recc log:"
  echo "---"
  cat integration_tests/recc/recc.log
  echo "---"

  if [ ! -f integration_tests/recc/test/main.o ]; then
    echo "FAIL: test/main.o was not created"
    echo ""
    echo "nativelink log:"
    echo "---"
    cat integration_tests/recc/nativelink.log
    echo "---"
    exit 1
  fi

  echo "SUCCESS: test/main.o was created"

  nativelink_output=$(cat integration_tests/recc/nativelink.log)

  case $nativelink_output in
    *"ERROR"* )
      echo "Error in nativelink log"
      exit 1
    ;;
    *)
      echo "Successful nativelink run"
    ;;
  esac
''
