fn main() {
    gsb_lint::check(env!("CARGO_MANIFEST_DIR").as_ref());

    let proto_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/proto");
    // `war.proto` imports `base.proto` (`gsb.base.RpcResponse`, in the
    // typed `Private` mirror) and `kit.proto` (`gsb.kit.InputAck`, used
    // as is). Both crates publish their proto directory through their
    // `links` key; cargo delivers it here because both are DIRECT
    // dependencies (the same arrangement as gsb-demo's build script).
    let base_dir = std::env::var("DEP_GSB_BASE_PROTO_DIR").expect(
        "DEP_GSB_BASE_PROTO_DIR is not set: gsb-protocol must be a direct \
         dependency and must declare `links = \"gsb-base-proto\"`",
    );
    let kit_dir = std::env::var("DEP_GSB_KIT_PROTO_DIR").expect(
        "DEP_GSB_KIT_PROTO_DIR is not set: gsb-kit must be a direct \
         dependency and must declare `links = \"gsb-kit-proto\"`",
    );

    let mut cfg = prost_build::Config::new();
    if let Some(protoc) = vendored_protoc() {
        cfg.protoc_executable(protoc);
    }
    // Reuse the types gsb-protocol and gsb-kit already generated (a
    // second copy would encode identically but be distinct Rust types).
    cfg.extern_path(".gsb.base", "::gsb_protocol::base");
    cfg.extern_path(".gsb.kit", "::gsb_kit::proto");
    cfg.compile_protos(
        &[format!("{proto_dir}/war.proto")],
        &[proto_dir, &kit_dir, &base_dir],
    )
    .expect("prost codegen failed (using vendored protoc)");
    println!("cargo:rerun-if-changed=proto/war.proto");
    println!("cargo:rerun-if-changed={kit_dir}/kit.proto");
    println!("cargo:rerun-if-changed={base_dir}/base.proto");
}

/// The vendored `protoc` — same contract as `gsb-protocol`'s build script:
/// an explicit path beats `PROTOC`/`PATH`; `None` falls back to
/// prost-build's own lookup on a target the crate ships no binary for.
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
