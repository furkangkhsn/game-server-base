//! `[rooms.<id>]` parses its ten room-level keys in their flat
//! spellings and refuses everything else: an unknown key, a key that is
//! not room-level, a malformed id, a bad value.

use std::time::Duration;

use crate::config::{Config, RoomOverride};

fn parse(text: &str) -> Result<Config, toml::de::Error> {
    toml::from_str(text)
}

fn refused(text: &str) -> String {
    parse(text).expect_err(text).to_string()
}

/// Every room-level key, in the table and the dotted spelling; omitted
/// keys stay `None` (the server's value).
#[test]
fn a_room_section_parses_its_room_level_keys() {
    let cfg = parse(
        "tick_hz = 60\n\
         [rooms.7]\n\
         tick_hz = 15\n\
         room_control = 32\n\
         conn_action = 64\n\
         max_snapshot_bytes = 900\n\
         keepalive_hz = 0.5\n\
         max_players = 64\n\
         max_idle_input_secs = 120\n\
         max_detach_hold_secs = \"off\"\n\
         input_rate_hz = 30\n\
         input_burst = 12\n\
         [rooms.2]\n\
         max_players = 2\n",
    )
    .expect("parses");
    assert_eq!(cfg.rooms.keys().copied().collect::<Vec<_>>(), [2, 7]);
    assert_eq!(
        cfg.rooms[&7],
        RoomOverride {
            tick_hz: Some(15.0),
            room_control: Some(32),
            conn_action: Some(64),
            max_snapshot_bytes: Some(900),
            keepalive_hz: Some(0.5),
            max_players: Some(64),
            max_idle_input_secs: Some(120),
            max_detach_hold: Some(None),
            input_rate_hz: Some(30),
            input_burst: Some(12),
        }
    );
    assert_eq!(
        cfg.rooms[&2],
        RoomOverride {
            max_players: Some(2),
            ..RoomOverride::default()
        }
    );
    assert_eq!(cfg.tick_hz, 60.0, "the flat keys are untouched");

    let dotted =
        parse("rooms.3.max_detach_hold_secs = 2.5\nrooms.3.max_players = 0").expect("parses");
    assert_eq!(
        dotted.rooms[&3],
        RoomOverride {
            max_players: Some(0),
            max_detach_hold: Some(Some(Duration::from_millis(2_500))),
            ..RoomOverride::default()
        }
    );
    assert!(parse("").expect("parses").rooms.is_empty());
    assert!(Config::default().rooms.is_empty());
}

/// A typo, a server-wide key, a demo key and a game's table key are all
/// refused, naming the key and listing what a room section takes.
#[test]
fn a_key_that_is_not_room_level_is_refused() {
    for key in [
        "max_playerz = 3",
        "bind = \"0.0.0.0:1\"",
        "max_connections = 5",
        "room_count = 2",
        "idle_timeout_secs = 5",
        "spawn_half_size = 10.0",
        "teams = 3",
        "game = \"arena\"",
    ] {
        let e = refused(&format!("[rooms.7]\n{key}\n"));
        let name = key.split(" = ").next().expect("a key");
        assert!(e.contains(&format!("unknown field `{name}`")), "{key}: {e}");
        assert!(e.contains("max_detach_hold_secs"), "{key}: {e}");
    }
}

/// An id is a positive integer written plainly; `rooms` is a table of
/// tables.
#[test]
fn a_malformed_room_id_is_refused() {
    for id in ["0", "lobby", "07", "\"+7\"", "-1", "18446744073709551616"] {
        let e = refused(&format!("[rooms.{id}]\nmax_players = 2\n"));
        assert!(e.contains("keyed by its room id"), "{id}: {e}");
    }
    let e = refused("rooms = 3");
    assert!(e.contains("a table of room ids"), "{e}");
    let e = refused("[rooms]\n7 = 3\n");
    assert!(e.contains("invalid type"), "{e}");
}

/// A room-level key with the wrong type is refused like its flat
/// counterpart.
#[test]
fn a_bad_value_is_refused() {
    for (line, needle) in [
        ("tick_hz = \"fast\"", "tick_hz"),
        ("max_players = -1", "max_players"),
        ("room_control = 1.5", "room_control"),
        ("max_detach_hold_secs = \"never\"", "or \"off\""),
        ("max_detach_hold_secs = -1", "or \"off\""),
    ] {
        let e = refused(&format!("[rooms.4]\n{line}\n"));
        assert!(e.contains(needle), "{line}: {e}");
    }
}
