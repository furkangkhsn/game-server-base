fn main() {
    gsb_lint::check(env!("CARGO_MANIFEST_DIR").as_ref());

    let proto_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/proto");
    // `kit.proto` imports `base.proto` for `gsb.base.RpcResponse`
    // (`Private.responses`); gsb-protocol publishes its proto directory
    // through its `links` key (see its build script).
    let base_dir = std::env::var("DEP_GSB_BASE_PROTO_DIR").expect(
        "DEP_GSB_BASE_PROTO_DIR is not set: gsb-protocol must be a direct \
         dependency and must declare `links = \"gsb-base-proto\"`",
    );

    let mut cfg = prost_build::Config::new();
    if let Some(protoc) = vendored_protoc() {
        cfg.protoc_executable(protoc);
    }
    // Point prost at the `gsb.base` types gsb-protocol already generated
    // (a second copy would encode identically but be distinct Rust types).
    cfg.extern_path(".gsb.base", "::gsb_protocol::base");
    cfg.compile_protos(&[format!("{proto_dir}/kit.proto")], &[proto_dir, &base_dir])
        .expect("prost codegen failed (using vendored protoc)");
    // Publish the proto directory to DIRECT dependents (via the `links`
    // key, see Cargo.toml) as `DEP_GSB_KIT_PROTO_DIR`: a game's proto
    // imports `kit.proto`, and its build script needs this path as a
    // protoc include.
    println!("cargo:dir={proto_dir}");
    println!("cargo:rerun-if-changed=proto/kit.proto");
    println!("cargo:rerun-if-changed={base_dir}/base.proto");
}

/// The vendored `protoc` — same contract as `gsb-protocol`'s build script
/// (see its `vendored_protoc` docs): explicit path beats `PROTOC`/`PATH`,
/// `None` falls back to prost-build's own lookup on a target the crate
/// ships no binary for.
fn vendored_protoc() -> Option<std::path::PathBuf> {
    match protoc_bin_vendored::protoc_bin_path() {
        Ok(path) => Some(path),
        Err(e) => {
            println!(
                "cargo:warning=no vendored protoc for this target ({e}); \
                 falling back to PROTOC/PATH"
            );
            None
        }
    }
}
