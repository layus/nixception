#!/usr/bin/env bash
# Regenerate the checked-in Rust bindings in nativelink-proto/genproto/.
#
# Usage (from anywhere, needs `protoc` on PATH or $PROTOC):
#   nativelink-proto/update_protos.sh          # rewrite genproto/
#   nativelink-proto/update_protos.sh --check  # fail if genproto/ is stale
set -euo pipefail

proto_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_dir=$(dirname "$proto_dir")
cd "$repo_dir"

protos=(
  build/bazel/remote/execution/v2/remote_execution.proto
  build/bazel/semver/semver.proto
  com/github/trace_machina/nativelink/remote_execution/events.proto
  com/github/trace_machina/nativelink/remote_execution/worker_api.proto
  google/api/annotations.proto
  google/api/client.proto
  google/api/field_behavior.proto
  google/api/http.proto
  google/bytestream/bytestream.proto
  google/longrunning/operations.proto
  google/protobuf/any.proto
  google/protobuf/descriptor.proto
  google/protobuf/duration.proto
  google/protobuf/empty.proto
  google/protobuf/timestamp.proto
  google/protobuf/wrappers.proto
  google/rpc/status.proto
)

# Generated packages, in the order lib.rs lists them.
packages=(
  build.bazel.remote.execution.v2
  build.bazel.semver
  com.github.trace_machina.nativelink.remote_execution
  com.github.trace_machina.nativelink.events
  google.api
  google.bytestream
  google.longrunning
  google.rpc
)

license='// Copyright 2022 The NativeLink Authors. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.'

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

mkdir "$tmp/raw"
cargo run --quiet -p nativelink-proto --example gen_protos_tool -- \
  -o "$tmp/raw" "${protos[@]/#/nativelink-proto/}"

mkdir "$tmp/genproto"
for file in "$tmp"/raw/*.rs; do
  name=$(basename "$file" .rs)
  {
    echo "$license"
    echo
    cat "$file"
  } > "$tmp/genproto/$name.pb.rs"
done
pkg_files=("${packages[@]/#/$tmp/genproto/}")
python3 "$proto_dir/gen_lib_rs_tool.py" --rootdir "$tmp/genproto" "${pkg_files[@]/%/.pb.rs}" \
  > "$tmp/genproto/lib.rs"

if [[ "${1:-}" == "--check" ]]; then
  if diff -r "$proto_dir/genproto" "$tmp/genproto"; then
    echo "genproto/ is up to date"
  else
    echo "genproto/ is out of date; run nativelink-proto/update_protos.sh" >&2
    exit 1
  fi
else
  rm -rf "$proto_dir/genproto"
  cp -r "$tmp/genproto" "$proto_dir/genproto"
fi
