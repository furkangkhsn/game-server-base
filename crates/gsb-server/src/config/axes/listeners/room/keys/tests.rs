//! The room-level keys' own values: refused at startup, flat or in a
//! `[rooms.<id>]`, before anything binds.

use crate::config::{Config, ServerError};

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
