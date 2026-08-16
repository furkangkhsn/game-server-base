fn main() {
    // Policy lint: fail fast on banned patterns in src/tests/examples.
    gsb_lint::check(std::path::Path::new(env!("CARGO_MANIFEST_DIR")));
}
