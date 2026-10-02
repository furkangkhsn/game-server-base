use super::*;

/// The origin of `text`, read from `f.toml`.
fn origin(text: &str) -> ConfigOrigin {
    ConfigOrigin::of_file("f.toml".into(), text)
}

/// Every way to write a top-level key, each at the line that first
/// writes it (1-based); a later header of the same table keeps the
/// first line.
#[test]
fn each_top_level_key_is_at_the_line_that_first_writes_it() {
    let o = origin(
        "flat = 1\n# note\n\n\"quoted\" = 2\ndotted.sub = 3\n[table]\nx = 1\n[deep.sub]\ny = 2\n[[arr]]\n[[arr]]\n[table.more]\n",
    );
    for (key, line) in [
        ("flat", 1),
        ("quoted", 4),
        ("dotted", 5),
        ("table", 6),
        ("deep", 8),
        ("arr", 10),
    ] {
        assert_eq!(o.locate(key), Some(format!("f.toml:{line}")), "{key}");
    }
    assert_eq!(o.locate("x"), None, "a sub-key is no top-level key");
}

/// Windows line ends count lines the same way.
#[test]
fn crlf_line_ends_count_the_same() {
    let o = origin("a = 1\r\n\r\nb = 2\r\n");
    assert_eq!(o.locate("b"), Some("f.toml:3".into()));
}

/// No place is known for a key the file did not write, for text that
/// does not parse, or for a config built in code.
#[test]
fn no_place_without_the_file_writing_the_key() {
    assert_eq!(origin("a = 1\n").locate("b"), None);
    assert_eq!(origin("a = = 1\n").locate("a"), None);
    assert_eq!(ConfigOrigin::default().locate("a"), None);
}
