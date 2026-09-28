//! The chain's two conventions: a cause already in the message is not
//! repeated; a cause the message does not carry gets its own line.

use super::error_chain;

/// A test error: `message`, and optionally a cause.
#[derive(Debug)]
struct Link {
    message: String,
    cause: Option<Box<Link>>,
}

impl std::fmt::Display for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Link {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.cause.as_deref().map(|c| c as _)
    }
}

fn link(message: &str, cause: Option<Link>) -> Link {
    Link {
        message: message.into(),
        cause: cause.map(Box::new),
    }
}

#[test]
fn a_cause_the_message_carries_is_not_repeated() {
    let err = link("cannot parse x: bad key", Some(link("bad key", None)));
    assert_eq!(error_chain(&err), "cannot parse x: bad key");
}

#[test]
fn a_cause_the_message_does_not_carry_gets_its_own_line() {
    let err = link(
        "game `g`: settings refused",
        Some(link("table unreadable", Some(link("disk on fire", None)))),
    );
    assert_eq!(
        error_chain(&err),
        "game `g`: settings refused\ncaused by: table unreadable\ncaused by: disk on fire"
    );
}

#[test]
fn a_config_parse_error_is_its_display_text() {
    let path = std::env::temp_dir().join(format!("gsb-error-chain-{}.toml", std::process::id()));
    std::fs::write(&path, "tick_hz = 30\nlisten_backlog = -1\n").expect("write");
    let err = crate::Config::from_file(&path).expect_err("refused");
    let _ = std::fs::remove_file(&path);
    let text = error_chain(&err);
    assert_eq!(text, err.to_string().trim_end());
    assert!(!text.ends_with('\n'), "{text:?}");
    assert!(text.contains("line 2"), "{text}");
    assert!(!text.contains("tick_hz = 30"), "{text}");
}
