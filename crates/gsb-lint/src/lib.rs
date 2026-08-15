//! Build-time policy checks for the `gsb` workspace.
//!
//! This crate is used from other crates' `build.rs` files. It scans the
//! invoking crate's `src/` tree and fails the build if a banned pattern is
//! found **in code**. The architecture of this project is lock-free and
//! free of hot-path multiplexing; this makes those guarantees enforceable
//! at compile time instead of relying on code review.
//!
//! ```rust,ignore
//! // build.rs
//! fn main() {
//!     gsb_lint::check(env!("CARGO_MANIFEST_DIR"));
//! }
//! ```
//!
//! Comments are stripped before matching, so documentation may mention the
//! banned patterns (to explain why they are absent) without tripping the
//! check.

use std::path::{Path, PathBuf};

/// Patterns that are banned in gsb crate *code*.
///
/// - `tokio::select` / `futures::select`: multiplexing in the hot path is
///   the exact thing this architecture avoids; actors are mailbox-driven and
///   I/O is handled by dedicated pump tasks.
/// - `Mutex` / `RwLock` / `parking_lot`: all shared state is owned by actors
///   and exchanged over channels.
pub const BANNED_PATTERNS: &[&str] = &[
    "tokio::select",
    "futures::select",
    "std::sync::Mutex",
    "std::sync::RwLock",
    "parking_lot",
];

/// Scan `crate_dir/src/**/*.rs` and panic (failing the build) if any banned
/// pattern appears in code (comments are stripped first).
pub fn check(crate_dir: &Path) {
    let src = crate_dir.join("src");
    if !src.exists() {
        return;
    }
    let mut offenders = 0usize;
    for file in walk_rs(&src) {
        let Ok(raw) = std::fs::read_to_string(&file) else {
            continue;
        };
        let code = strip_comments(&raw);
        for (lineno, line) in code.lines().enumerate() {
            for pat in BANNED_PATTERNS {
                if line.contains(pat) {
                    let rel = file
                        .strip_prefix(crate_dir)
                        .unwrap_or(&file)
                        .to_string_lossy();
                    eprintln!(
                        "gsb-lint: banned pattern `{pat}` found in {rel}:{}:\n  {}",
                        lineno + 1,
                        line.trim()
                    );
                    offenders += 1;
                }
            }
        }
    }
    if offenders > 0 {
        panic!(
            "gsb-lint: {} banned pattern occurrence(s) found in {}. \
             This architecture is lock-free and multiplex-free by design; \
             move shared state into an actor and communicate over channels.",
            offenders,
            crate_dir.display()
        );
    }
}

/// Replace comments with spaces (keeping newlines, so line numbers survive).
/// Handles `//` line comments, nested `/* */` block comments, and string /
/// char literals (so `//` inside a string is not treated as a comment).
fn strip_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    #[derive(PartialEq)]
    enum State {
        Normal,
        Line,
        Block(usize), // nesting depth
        Str,
        Char,
    }
    let mut state = State::Normal;
    while let Some(c) = chars.next() {
        match state {
            State::Normal => match c {
                '/' => {
                    if matches!(chars.peek(), Some('/')) {
                        chars.next();
                        state = State::Line;
                        out.push(' ');
                        out.push(' ');
                    } else if matches!(chars.peek(), Some('*')) {
                        chars.next();
                        state = State::Block(1);
                        out.push(' ');
                        out.push(' ');
                    } else {
                        out.push(c);
                    }
                }
                '"' => {
                    state = State::Str;
                    out.push(c);
                }
                '\'' => {
                    state = State::Char;
                    out.push(c);
                }
                _ => out.push(c),
            },
            State::Line => {
                if c == '\n' {
                    state = State::Normal;
                    out.push('\n');
                } else {
                    out.push(' ');
                }
            }
            State::Block(depth) => {
                if c == '\n' {
                    out.push('\n');
                }
                match c {
                    '/' if matches!(chars.peek(), Some('*')) => {
                        chars.next();
                        state = State::Block(depth + 1);
                        out.push(' ');
                    }
                    '*' if matches!(chars.peek(), Some('/')) => {
                        chars.next();
                        if depth == 1 {
                            state = State::Normal;
                        } else {
                            state = State::Block(depth - 1);
                        }
                        out.push(' ');
                    }
                    _ => out.push(' '),
                }
            }
            State::Str => {
                out.push(c);
                match c {
                    '\\' => {
                        if let Some(e) = chars.next() {
                            out.push(e);
                        }
                    }
                    '"' => state = State::Normal,
                    '\n' => state = State::Normal, // unterminated; recover
                    _ => {}
                }
            }
            State::Char => {
                out.push(c);
                match c {
                    '\\' => {
                        if let Some(e) = chars.next() {
                            out.push(e);
                        }
                    }
                    '\'' => state = State::Normal,
                    '\n' => state = State::Normal,
                    _ => {}
                }
            }
        }
    }
    out
}

fn walk_rs(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk_rs(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_line_comments() {
        let out = strip_comments("let a = 1; // tokio::select here\nlet b = 2;");
        assert!(!out.contains("tokio::select"));
        assert!(out.contains("let a = 1;"));
        assert!(out.contains("let b = 2;"));
        assert_eq!(out.lines().count(), 2);
    }

    #[test]
    fn strips_block_comments_nested() {
        let src = "/* outer /* inner tokio::select */ still */ let x = 1;";
        let out = strip_comments(src);
        assert!(!out.contains("tokio::select"));
        assert!(out.contains("let x = 1;"));
    }

    #[test]
    fn keeps_strings() {
        let src = "let s = \"// not a comment\"; tokio::select!{}";
        let out = strip_comments(src);
        assert!(out.contains("\"// not a comment\""));
        assert!(out.contains("tokio::select"));
    }

    #[test]
    fn preserves_line_numbers() {
        let src = "let a = 1;\n/* multi\nline */\nlet b = 2;";
        let out = strip_comments(src);
        assert_eq!(out.lines().count(), 4);
        assert!(!out.contains("multi"));
        assert!(out.contains("let b = 2;"));
    }
}
