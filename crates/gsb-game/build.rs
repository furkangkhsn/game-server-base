fn main() {
    gsb_lint::check(env!("CARGO_MANIFEST_DIR").as_ref());

    let proto_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/proto");
    let mut cfg = prost_build::Config::new();
    if let Some(protoc) = vendored_protoc() {
        cfg.protoc_executable(protoc);
    }
    cfg.compile_protos(&[format!("{proto_dir}/game.proto")], &[proto_dir])
        .expect("prost codegen failed (using vendored protoc)");
    println!("cargo:rerun-if-changed=proto/game.proto");
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
