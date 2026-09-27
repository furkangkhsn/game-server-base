//! The room-level keys' own values: refused at startup, flat or in a
//! `[rooms.<id>]`, before anything binds.

use gsb_core::room::InputRate;

use crate::config::{Config, GameDefaults, ServerError};

fn checked(text: &str) -> Result<(), ServerError> {
    toml::from_str::<Config>(text)
        .expect("parses")
        .check_room_keys()
}

/// F21: `conn_action = 0` reached `mpsc::channel(0)` and panicked the
/// room at its first join. Refused, naming the key and where it was
/// written; any other capacity passes.
#[test]
fn a_zero_action_capacity_is_refused_where_it_is_written() {
    for (text, at) in [
        ("conn_action = 0", "`conn_action`"),
        ("[rooms.7]\nconn_action = 0", "`[rooms.7]` `conn_action`"),
        (
            "conn_action = 8\n[rooms.2]\nconn_action = 0",
            "`[rooms.2]` `conn_action`",
        ),
    ] {
        let e = checked(text).expect_err(text);
        assert!(matches!(e, ServerError::RoomKey { .. }), "{text}: {e:?}");
        let msg = e.to_string();
        assert!(msg.starts_with(at), "{text}: {msg}");
        assert!(msg.contains("at least one"), "{text}: {msg}");
    }
    for text in ["", "conn_action = 1", "[rooms.7]\nconn_action = 1"] {
        assert!(checked(text).is_ok(), "{text}");
    }
}

fn limit(per_sec: u32, burst: u32) -> Option<InputRate> {
    InputRate::new(per_sec, burst)
}

fn room_rate(text: &str, id: u64, game: Option<InputRate>) -> Option<InputRate> {
    toml::from_str::<Config>(text)
        .expect("parses")
        .room_template_for(GameDefaults {
            input_rate: game,
            ..GameDefaults::default()
        })
        .room(id)
        .input_rate
}

/// E1: `input_rate_hz` turns the limit on (burst = one second's worth
/// unless `input_burst` says otherwise); omitted or `0` = off.
#[test]
fn the_flat_input_keys_reach_every_room() {
    for (text, want) in [
        ("", None),
        ("input_rate_hz = 0", None),
        ("input_rate_hz = 20", limit(20, 20)),
        ("input_rate_hz = 20\ninput_burst = 5", limit(20, 5)),
    ] {
        for id in [1, 9] {
            assert_eq!(room_rate(text, id, None), want, "{text:?} r{id}");
        }
    }
    let d = Config::default();
    assert_eq!((d.input_rate_hz, d.input_burst), (None, None));
}

/// `[rooms.<id>]` sets its own limit, or turns the server's off, for
/// that room alone.
#[test]
fn a_room_override_sets_or_clears_its_own_limit() {
    let text = "input_rate_hz = 20\n\
        [rooms.2]\ninput_rate_hz = 60\ninput_burst = 10\n\
        [rooms.3]\ninput_rate_hz = 0\n\
        [rooms.4]\nmax_players = 5\n";
    assert_eq!(room_rate(text, 1, None), limit(20, 20));
    assert_eq!(room_rate(text, 2, None), limit(60, 10));
    assert_eq!(room_rate(text, 3, None), None);
    assert_eq!(room_rate(text, 4, None), limit(20, 20), "other keys only");
}

/// The game's number is the default the operator overrides: omitted
/// keys keep it, `input_rate_hz = 0` turns it off, a written rate
/// replaces it — and a room override beats both.
#[test]
fn the_game_default_is_what_the_keys_override() {
    let game = limit(7, 3);
    assert_eq!(room_rate("", 1, game), game);
    assert_eq!(room_rate("max_players = 5", 1, game), game);
    assert_eq!(room_rate("input_rate_hz = 0", 1, game), None);
    assert_eq!(room_rate("input_rate_hz = 30", 1, game), limit(30, 30));
    let text = "[rooms.2]\ninput_rate_hz = 0\n[rooms.3]\ninput_rate_hz = 9\n";
    assert_eq!(room_rate(text, 1, game), game);
    assert_eq!(room_rate(text, 2, game), None);
    assert_eq!(room_rate(text, 3, game), limit(9, 9));
    // `Config::room_config` is the file's view (no game default).
    let cfg = toml::from_str::<Config>("").expect("parses");
    assert_eq!(cfg.room_config(1).input_rate, None);
}

/// A burst without a rate in the same table, a burst for a limit that
/// is off, and a zero burst are refused where they were written — the
/// operator meant something the keys cannot say.
#[test]
fn an_input_burst_without_a_rate_is_refused() {
    for (text, at, why) in [
        ("input_burst = 5", "`input_burst`", "needs `input_rate_hz`"),
        ("input_rate_hz = 0\ninput_burst = 5", "`input_burst`", "off"),
        (
            "input_rate_hz = 20\ninput_burst = 0",
            "`input_burst`",
            "admits nothing",
        ),
        (
            "input_rate_hz = 20\n[rooms.7]\ninput_burst = 5",
            "`[rooms.7]` `input_burst`",
            "needs `input_rate_hz`",
        ),
        (
            "[rooms.7]\ninput_rate_hz = 3\ninput_burst = 0",
            "`[rooms.7]` `input_burst`",
            "admits nothing",
        ),
    ] {
        let e = checked(text).expect_err(text);
        assert!(matches!(e, ServerError::RoomKey { .. }), "{text}: {e:?}");
        let msg = e.to_string();
        assert!(msg.starts_with(at), "{text}: {msg}");
        assert!(msg.contains(why), "{text}: {msg}");
    }
    for text in [
        "input_rate_hz = 20",
        "input_rate_hz = 0",
        "input_rate_hz = 20\ninput_burst = 1",
        "[rooms.7]\ninput_rate_hz = 0",
    ] {
        assert!(checked(text).is_ok(), "{text}");
    }
}
