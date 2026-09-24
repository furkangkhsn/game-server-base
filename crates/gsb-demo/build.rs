fn main() {
    gsb_lint::check(env!("CARGO_MANIFEST_DIR").as_ref());

    let proto_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/proto");
    // `game.proto` imports `base.proto` for `gsb.base.RpcResponse` (the
    // RPC response envelope is a CORE contract — see base.proto). The
    // base crate's build script publishes its proto directory through
    // its `links` key; cargo delivers it here. A relative
    // `../gsb-protocol/proto` would break the moment gsb-protocol comes
    // from the registry rather than this workspace — the exact case a
    // second game crate is.
    let base_dir = std::env::var("DEP_GSB_BASE_PROTO_DIR").expect(
        "DEP_GSB_BASE_PROTO_DIR is not set: gsb-protocol must be a direct \
         dependency and must declare `links = \"gsb-base-proto\"`",
    );
    // `game.proto` also imports `kit.proto` (its typed `Private` mirror
    // reuses `gsb.kit.InputAck`); gsb-kit publishes its proto directory
    // the same way.
    let kit_dir = std::env::var("DEP_GSB_KIT_PROTO_DIR").expect(
        "DEP_GSB_KIT_PROTO_DIR is not set: gsb-kit must be a direct \
         dependency and must declare `links = \"gsb-kit-proto\"`",
    );

    let mut cfg = prost_build::Config::new();
    if let Some(protoc) = vendored_protoc() {
        cfg.protoc_executable(protoc);
    }
    // Do NOT generate a second copy of the `gsb.base` types here: point
    // prost at the ones `gsb-protocol` already generated. Two copies
    // would encode identically but be distinct Rust types, which is the
    // duplication this move exists to remove.
    cfg.extern_path(".gsb.base", "::gsb_protocol::base");
    // Likewise the kit's envelope types: generated once, by gsb-kit.
    cfg.extern_path(".gsb.kit", "::gsb_kit::proto");
    cfg.compile_protos(
        &[format!("{proto_dir}/game.proto")],
        &[proto_dir, &kit_dir, &base_dir],
    )
    .expect("prost codegen failed (using vendored protoc)");
    println!("cargo:rerun-if-changed=proto/game.proto");
    println!("cargo:rerun-if-changed={kit_dir}/kit.proto");
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
