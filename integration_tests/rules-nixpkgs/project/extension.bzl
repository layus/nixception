"""Module extension for configuring the Nixpkgs CC toolchain."""

load("@rules_nixpkgs_cc//:cc.bzl", "nixpkgs_cc_configure")

def _cc_configure_impl(_module_ctx):
    nixpkgs_cc_configure(
        name = "nixpkgs_config_cc",
        repository = "@nixpkgs",
        register = False,
    )

cc_configure = module_extension(
    implementation = _cc_configure_impl,
)
