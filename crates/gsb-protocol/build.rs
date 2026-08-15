fn main() {
    let proto_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/proto");
    prost_build::Config::new()
        .compile_protos(&[format!("{proto_dir}/base.proto")], &[proto_dir])
        .expect("prost codegen failed (using vendored protoc)");
    println!("cargo:rerun-if-changed=proto/base.proto");
}
