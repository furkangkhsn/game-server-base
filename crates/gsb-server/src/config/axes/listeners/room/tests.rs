//! `max_detach_hold_secs` parses in its three spellings, refuses the
//! rest, and reaches the room configuration every hosted room is built
//! from.

use std::time::Duration;

use gsb_core::room::DEFAULT_MAX_DETACH_HOLD;

use crate::config::Config;

fn parse(text: &str) -> Result<Config, toml::de::Error> {
    toml::from_str(text)
}

fn ceiling(text: &str) -> Option<Duration> {
    parse(text).expect("parses").max_detach_hold
}

/// Omitted = the core's default; a number = that many seconds (0 is
/// literal, fractions allowed); "off" = no ceiling.
#[test]
fn the_ceiling_parses_in_its_three_spellings() {
    assert_eq!(ceiling(""), Some(DEFAULT_MAX_DETACH_HOLD));
    assert_eq!(
        Config::default().max_detach_hold,
        Some(DEFAULT_MAX_DETACH_HOLD)
    );
    assert_eq!(
        ceiling("max_detach_hold_secs = 90"),
        Some(Duration::from_secs(90))
    );
    assert_eq!(ceiling("max_detach_hold_secs = 0"), Some(Duration::ZERO));
    assert_eq!(
        ceiling("max_detach_hold_secs = 2.5"),
        Some(Duration::from_millis(2_500))
    );
    assert_eq!(ceiling("max_detach_hold_secs = \"off\""), None);
}

/// A negative number, another word, or a wrong type is a startup error
/// naming the key and what it takes.
#[test]
fn a_bad_ceiling_is_refused() {
    for bad in ["-1", "-0.5", "\"none\"", "\"\"", "true", "nan", "inf"] {
        let e = parse(&format!("max_detach_hold_secs = {bad}"))
            .expect_err(bad)
            .to_string();
        assert!(e.contains("max_detach_hold_secs"), "{bad}: {e}");
        assert!(e.contains("or \"off\""), "{bad}: {e}");
    }
}

/// The parsed ceiling is what the room gets — and the other room-level
/// keys still reach it too.
#[test]
fn the_ceiling_reaches_the_room() {
    for (text, want) in [
        ("", Some(DEFAULT_MAX_DETACH_HOLD)),
        ("max_detach_hold_secs = 3", Some(Duration::from_secs(3))),
        ("max_detach_hold_secs = \"off\"", None),
    ] {
        let room = parse(text).expect("parses").room_config(7);
        assert_eq!(room.max_detach_hold, want, "{text:?}");
        assert_eq!(room.id.0, 7);
    }
    let cfg = parse("max_players = 12\nmax_idle_input_secs = 40\ntick_hz = 20.0").expect("parses");
    let room = cfg.room_config(1);
    assert_eq!(room.max_players, Some(12));
    assert_eq!(room.max_idle_input_secs, Some(40));
    assert_eq!(room.tick_hz, 20.0);
}
