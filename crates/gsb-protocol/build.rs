fn main() {
    // Policy lint first: fail fast on banned patterns in src/tests/examples.
    gsb_lint::check(std::path::Path::new(env!("CARGO_MANIFEST_DIR")));
    let proto_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/proto");
    let mut cfg = prost_build::Config::new();
    if let Some(protoc) = vendored_protoc() {
        cfg.protoc_executable(protoc);
    }
    cfg.compile_protos(&[format!("{proto_dir}/base.proto")], &[proto_dir])
        .expect("prost codegen failed (using vendored protoc)");
    // Publish the proto directory to DIRECT dependents (via the `links`
    // key, see Cargo.toml) as `DEP_GSB_BASE_PROTO_DIR`: a game protocol
    // imports `base.proto` for `gsb.base.RpcResponse`, and its build
    // script needs this path as a protoc include. A relative
    // `../gsb-protocol/proto` would only work inside this workspace.
    println!("cargo:dir={proto_dir}");
    println!("cargo:rerun-if-changed=proto/base.proto");
}

/// The vendored `protoc`, so the build needs NO system protoc and no
/// `PROTOC` environment variable — the reason `protoc-bin-vendored` is a
/// build-dependency. It is set through `Config::protoc_executable`, which
/// takes precedence over prost-build's `PROTOC`/`PATH` lookup, so a stale
/// or wrong `PROTOC` in the environment cannot break the build.
///
/// `None` on a target the crate ships no binary for; the build then falls
/// back to prost-build's own lookup (`PROTOC`, then `PATH`) — exactly the
/// old behaviour, so an exotic platform stays buildable with a system
/// protoc.
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
