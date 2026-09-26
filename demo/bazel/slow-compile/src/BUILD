load("@rules_cc//cc:cc_binary.bzl", "cc_binary")

# slow_table.c and main.c are copies of demo/slow-compile/{slow_table.c,main.c}
# (see demo/flake.nix and gen.py) — the same ~550k-element designated
# aggregate initializer that GCC parses super-linearly (~5s at -O2), compiled
# here via Bazel + rules_nixpkgs instead of a recc-wrapped stdenv compiler.
cc_binary(
    name = "slow_demo",
    srcs = [
        "main.c",
        "slow_table.c",
    ],
)
