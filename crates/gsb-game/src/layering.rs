//! The phase-0 layering rule, checked by scanning the source tree (the
//! same approach `gsb-lint` takes for its banned patterns): kit code
//! reaches demo code ONLY through `crate::kit::seam` (KIT-ARCHITECTURE
//! §10, phase 0).
//!
//! The check is deliberately stricter than "no `crate::demo` outside the
//! seam": under `kit/`, every `crate::` path outside `kit/seam.rs` must
//! continue with `kit::`. The crate root re-exports demo items under the
//! old public paths (`crate::components`, `crate::room::spawn_pos`, …), so
//! a kit file naming one of those would reach the demo past the seam
//! without ever spelling `crate::demo`. Comments are scanned too: a doc
//! link is a path like any other.

use std::path::{Path, PathBuf};

const SEAM: &str = "seam.rs";

fn walk_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).expect("readable source directory");
    for entry in entries {
        let path = entry.expect("readable directory entry").path();
        if path.is_dir() {
            walk_rs(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Every `crate::` path in `text` (at an identifier boundary) that does
/// not continue with `kit::`, as `(line number, line)`.
fn non_kit_paths(text: &str) -> Vec<(usize, String)> {
    let mut hits = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let mut from = 0;
        while let Some(at) = line[from..].find("crate::") {
            let start = from + at;
            let boundary = line[..start]
                .chars()
                .next_back()
                .is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '$'));
            let rest = &line[start + "crate::".len()..];
            if boundary && !rest.starts_with("kit::") {
                hits.push((i + 1, line.trim().to_string()));
            }
            from = start + "crate::".len();
        }
    }
    hits
}

#[test]
fn kit_reaches_the_demo_only_through_the_seam() {
    let kit = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("kit");
    let mut files = Vec::new();
    walk_rs(&kit, &mut files);
    let seam = kit.join(SEAM);
    assert!(seam.is_file(), "the seam module must exist: {seam:?}");
    // A vacuous pass (wrong directory, nothing scanned) must not count.
    assert!(
        files.len() > 20,
        "expected the whole kit tree, scanned {} files",
        files.len()
    );

    let mut offenders = Vec::new();
    for file in files.iter().filter(|f| **f != seam) {
        let text = std::fs::read_to_string(file).expect("readable source file");
        for (line, src) in non_kit_paths(&text) {
            let rel = file.strip_prefix(&kit).unwrap_or(file).display();
            offenders.push(format!("kit/{rel}:{line}: {src}"));
        }
    }
    assert!(
        offenders.is_empty(),
        "kit code must reach the demo through crate::kit::seam only \
         (every other crate path under kit/ continues with kit::):\n{}",
        offenders.join("\n")
    );

    let seam_text = std::fs::read_to_string(&seam).expect("readable seam");
    assert!(
        seam_text.contains("crate::demo::"),
        "the seam is where the kit's demo imports live"
    );
}

/// The scanner itself: what it flags and what it lets through.
#[test]
fn the_scanner_flags_root_and_demo_paths_but_not_kit_paths() {
    let src = "use crate::kit::seam;\n\
               use crate::demo::spawn::spawn_pos;\n\
               let r = crate::room::spawn_pos(c, h);\n\
               // see [`crate::kit::aoi::AoiRoom`]\n\
               let x = gsb_crate::other;\n";
    let hits: Vec<usize> = non_kit_paths(src).into_iter().map(|(l, _)| l).collect();
    assert_eq!(hits, vec![2, 3]);
}
