//! Codegen grpc/protobuf bindings for rust.
//!
//! Usage: `gen_protos_tool -o <output_dir> <input.proto>...`
//! (driven by `update_protos.sh`).

use std::path::PathBuf;

use prost_build::Config;

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let mut output_dir = None;
    let mut paths = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "-o" || arg == "--output_dir" {
            output_dir = args.next().map(PathBuf::from);
        } else {
            paths.push(arg);
        }
    }
    let output_dir = output_dir.expect("missing -o <output_dir>");
    assert!(!paths.is_empty(), "no input proto files given");

    let mut config = Config::new();
    config.bytes(["."]);

    let structs_with_data_to_ignore = [
        "BatchReadBlobsResponse.Response",
        "BatchUpdateBlobsRequest.Request",
        "ReadResponse",
        "WriteRequest",
    ];

    for struct_name in structs_with_data_to_ignore {
        config.type_attribute(struct_name, "#[derive(::derivative::Derivative)]");
        config.type_attribute(struct_name, "#[derivative(Debug)]");
        config.field_attribute(
            format!("{struct_name}.data"),
            "#[derivative(Debug=\"ignore\")]",
        );
    }

    config.skip_debug(structs_with_data_to_ignore);

    tonic_build::configure()
        .emit_rerun_if_changed(false)
        .out_dir(output_dir)
        .compile_protos_with_config(config, &paths, &["nativelink-proto"])?;
    Ok(())
}
