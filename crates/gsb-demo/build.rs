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

    // gsb-build: prost-build on the vendored protoc, plus the request
    // codegen of `gsb/options.proto` (`DemoRequests` → `impl Call` +
    // `impl Request for AbilityUse`, op checks at build time). It adds
    // gsb-protocol's proto directory (`gsb/options.proto`) itself.
    let mut b = gsb_build::Builder::new();
    // Do NOT generate a second copy of the `gsb.base` types here: point
    // prost at the ones `gsb-protocol` already generated. Two copies
    // would encode identically but be distinct Rust types, which is the
    // duplication this move exists to remove.
    b.extern_path(".gsb.base", "::gsb_protocol::base");
    // Likewise the kit's envelope types: generated once, by gsb-kit.
    b.extern_path(".gsb.kit", "::gsb_kit::proto");
    if let Err(e) = b.compile(
        &[format!("{proto_dir}/game.proto")],
        &[proto_dir, &kit_dir, &base_dir],
    ) {
        panic!("{e}");
    }
    println!("cargo:rerun-if-changed=proto/game.proto");
    println!("cargo:rerun-if-changed={kit_dir}/kit.proto");
    println!("cargo:rerun-if-changed={base_dir}/base.proto");
    println!("cargo:rerun-if-changed={base_dir}/gsb/options.proto");
}
