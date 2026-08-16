fn main() {
    // Policy lint first: fail fast on banned patterns in src/tests/examples.
    gsb_lint::check(std::path::Path::new(env!("CARGO_MANIFEST_DIR")));
    let proto_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/proto");
    prost_build::Config::new()
        .compile_protos(&[format!("{proto_dir}/base.proto")], &[proto_dir])
        .expect("prost codegen failed (using vendored protoc)");
    println!("cargo:rerun-if-changed=proto/base.proto");
}
