//! The kit never depends on a game (KIT-ARCHITECTURE §3) — in ANY
//! profile. A normal or build dependency on a game crate is a cargo
//! dependency cycle (every game depends on the kit), which cargo refuses
//! to resolve; a dev-dependency is not (cargo accepts dev-dependency
//! cycles and builds a second copy of the kit, whose types are not the
//! game's). This test closes that gap by reading the kit's manifest: the
//! only `gsb-*` crates it may name are the engine's.

/// The engine crates the kit may depend on (`gsb-lint` is the build
/// scripts' policy scan).
const ENGINE: [&str; 5] = ["gsb-core", "gsb-ecs", "gsb-net", "gsb-protocol", "gsb-lint"];

/// Every dependency named in a `*dependencies` table of `manifest`
/// (`[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`,
/// `[target.….dependencies]`, and the `[dependencies.name]` form).
fn dependencies(manifest: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_table = false;
    for line in manifest.lines().map(str::trim) {
        if let Some(header) = line.strip_prefix('[') {
            let header = header.trim_end_matches(']');
            in_table = header.ends_with("dependencies");
            if let Some((table, name)) = header.rsplit_once('.')
                && table.ends_with("dependencies")
            {
                names.push(name.to_string());
            }
        } else if in_table
            && !line.starts_with('#')
            && let Some((name, _)) = line.split_once('=')
        {
            names.push(name.trim().trim_matches('"').to_string());
        }
    }
    names
}

/// The `gsb-*` dependencies of `manifest` that are not engine crates.
fn games(manifest: &str) -> Vec<String> {
    dependencies(manifest)
        .into_iter()
        .filter(|n| n.starts_with("gsb-") && !ENGINE.contains(&n.as_str()))
        .collect()
}

#[test]
fn the_kit_manifest_names_no_game_crate() {
    let manifest = include_str!("../Cargo.toml");
    let deps = dependencies(manifest);
    // A vacuous pass (nothing parsed) must not count.
    assert!(
        deps.iter().any(|d| d == "gsb-protocol") && deps.iter().any(|d| d == "prost-build"),
        "the kit's dependency tables were not parsed: {deps:?}"
    );
    assert_eq!(games(manifest), Vec::<String>::new(), "a game in {deps:?}");

    // The scanner itself: every table form is read.
    let sample = "[package]\nname = \"gsb-kit\"\n\
                  [dependencies]\ngsb-core = { workspace = true }\n\
                  [dev-dependencies]\ngsb-demo = { workspace = true }\n\
                  [build-dependencies]\n\"gsb-server\" = \"0.1\"\n\
                  [target.'cfg(unix)'.dependencies]\ngsb-demo-arena = \"0.1\"\n\
                  [dependencies.gsb-demo-mmo]\nworkspace = true\n";
    assert_eq!(
        games(sample),
        ["gsb-demo", "gsb-server", "gsb-demo-arena", "gsb-demo-mmo"]
    );
}
