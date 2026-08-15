fn main() {
    gsb_lint::check(env!("CARGO_MANIFEST_DIR").as_ref());

    let proto_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/proto");
    prost_build::Config::new()
        .compile_protos(&[format!("{proto_dir}/game.proto")], &[proto_dir])
        .expect("prost codegen failed");
    println!("cargo:rerun-if-changed=proto/game.proto");
}
