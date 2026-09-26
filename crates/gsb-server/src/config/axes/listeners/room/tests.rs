//! `max_detach_hold_secs` parses in its three spellings, refuses the
//! rest, and reaches the room configuration every hosted room is built
//! from; `[rooms.<id>]` reaches its room and no other, and the startup
//! check refuses a room the registry would (BACKLOG B18).

use std::time::Duration;

use gsb_core::error::CoreError;
use gsb_core::id::RoomId;
use gsb_core::room::{DEFAULT_MAX_DETACH_HOLD, RoomConfig};

use crate::config::{Config, ServerError};

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

/// The server's room, spelled field by field from the flat keys (the F8
/// mapping) — what every room without an override must stay.
fn server_room(cfg: &Config, id: u64) -> RoomConfig {
    RoomConfig {
        id: RoomId(id),
        tick_hz: cfg.tick_hz,
        control_capacity: cfg.room_control,
        action_capacity: cfg.conn_action,
        max_snapshot_bytes: cfg.max_snapshot_bytes,
        keepalive_hz: cfg.keepalive_hz,
        max_players: cfg.max_players.map(|n| n as usize),
        max_idle_input_secs: cfg.max_idle_input_secs,
        max_detach_hold: cfg.max_detach_hold,
        ..RoomConfig::default()
    }
}

/// Every flat room-level key off the core's default, and two rooms'
/// overrides (`[rooms.1]` a boot room, `[rooms.7]` past `room_count`).
const TUNED: &str = "tick_hz = 60\n\
    room_count = 2\n\
    room_control = 64\n\
    conn_action = 128\n\
    max_snapshot_bytes = 1200\n\
    keepalive_hz = 2.0\n\
    max_players = 12\n\
    max_idle_input_secs = 40\n\
    max_detach_hold_secs = 3\n\
    [rooms.1]\n\
    tick_hz = 15\n\
    room_control = 32\n\
    conn_action = 16\n\
    max_snapshot_bytes = 900\n\
    keepalive_hz = 0.5\n\
    max_players = 2\n\
    max_idle_input_secs = 9\n\
    max_detach_hold_secs = \"off\"\n\
    [rooms.7]\n\
    max_players = 0\n";

/// No override: every room is the server's room, field by field — with
/// and without other rooms' overrides present (the default is today's).
#[test]
fn a_room_without_an_override_is_the_server_room() {
    for text in [
        "",
        "max_players = 12\ntick_hz = 60\nkeepalive_hz = 2.0",
        TUNED,
    ] {
        let cfg = parse(text).expect("parses");
        for id in [2, 3, 1000] {
            assert_eq!(cfg.room_config(id), server_room(&cfg, id), "{text:?} r{id}");
        }
    }
    let cfg = Config::default();
    assert_eq!(cfg.room_config(1), server_room(&cfg, 1));
}

/// `[rooms.1]` reaches room 1 in every key, and no other room; an id past
/// `room_count` gets its own; the flat conventions hold (0 players = no
/// cap, "off" = no ceiling).
#[test]
fn an_override_reaches_its_room_only() {
    let cfg = parse(TUNED).expect("parses");
    let want = RoomConfig {
        tick_hz: 15.0,
        control_capacity: 32,
        action_capacity: 16,
        max_snapshot_bytes: 900,
        keepalive_hz: 0.5,
        max_players: Some(2),
        max_idle_input_secs: Some(9),
        max_detach_hold: None,
        ..server_room(&cfg, 1)
    };
    assert_eq!(cfg.room_config(1), want);
    assert_eq!(cfg.room_config(2), server_room(&cfg, 2));
    let seven = RoomConfig {
        max_players: None,
        ..server_room(&cfg, 7)
    };
    assert_eq!(cfg.room_config(7), seven);
    assert_eq!(cfg.room_template().room(7), seven, "one layering point");
}

fn checked(text: &str) -> Result<(), ServerError> {
    parse(text).expect("parses").check_room_overrides()
}

/// The startup check refuses a room the registry would refuse, by the
/// registry's rule, and nothing else.
#[test]
fn the_check_refuses_a_room_the_registry_would() {
    for (text, id) in [
        ("[rooms.2]\ntick_hz = 7", 2),
        ("[rooms.2]\ntick_hz = 45", 2),
        ("[rooms.9]\ntick_hz = 0", 9),
        ("[rooms.9]\ntick_hz = -15", 9),
        ("[rooms.9]\ntick_hz = nan", 9),
    ] {
        match checked(text) {
            Err(ServerError::RoomOverride {
                id: got,
                source: CoreError::TickRate { .. },
            }) => assert_eq!(got, id, "{text}"),
            other => panic!("{text}: {other:?}"),
        }
    }
    for text in [
        "[rooms.1]\ntick_hz = 10\nkeepalive_hz = 12",
        "[rooms.1]\nkeepalive_hz = 31",
        "keepalive_hz = 10\n[rooms.1]\ntick_hz = 5",
    ] {
        let e = checked(text).expect_err(text);
        assert!(
            matches!(
                e,
                ServerError::RoomOverride {
                    id: 1,
                    source: CoreError::KeepaliveRate { .. }
                }
            ),
            "{text}: {e:?}"
        );
        assert!(e.to_string().contains("`[rooms.1]`"), "{e}");
    }
    for global in ["0", "-30", "nan"] {
        let e = checked(&format!("tick_hz = {global}\n[rooms.1]\nmax_players = 2"));
        assert!(
            matches!(e, Err(ServerError::BadTickRate(_))),
            "{global}: {e:?}"
        );
    }
    for text in [
        "",
        TUNED,
        "[rooms.1]\ntick_hz = 15",
        "[rooms.500]\nmax_players = 3",
        "tick_hz = 60\n[rooms.1]\ntick_hz = 20\nkeepalive_hz = 20",
        // The server's own room is not this check's business (unchanged).
        "keepalive_hz = 40",
    ] {
        assert!(checked(text).is_ok(), "{text}");
    }
}
