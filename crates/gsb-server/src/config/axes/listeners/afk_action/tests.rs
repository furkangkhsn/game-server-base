//! `afk_action` (BACKLOG E6): its two spellings, the refusal of any
//! other, and its layering — the core's default (`leave_room`), the
//! game's, the flat key, `[rooms.<id>]`.

use gsb_core::room::AfkAction;

use crate::config::{Config, GameDefaults};

fn parse(text: &str) -> Result<Config, toml::de::Error> {
    toml::from_str(text)
}

/// Room `id`'s action under `text`, hosting a game whose default is
/// `game`.
fn action(text: &str, id: u64, game: AfkAction) -> AfkAction {
    parse(text)
        .expect("parses")
        .room_template_for(GameDefaults {
            afk_action: game,
            ..GameDefaults::default()
        })
        .room(id)
        .afk_action
}

/// Omitted = not written; the two spellings read as their actions.
#[test]
fn the_flat_key_parses_its_two_spellings() {
    assert_eq!(parse("").expect("parses").afk_action, None);
    assert_eq!(Config::default().afk_action, None);
    for (word, want) in [
        ("leave_room", AfkAction::LeaveRoom),
        ("disconnect", AfkAction::Disconnect),
    ] {
        let cfg = parse(&format!("afk_action = \"{word}\"")).expect(word);
        assert_eq!(cfg.afk_action, Some(want), "{word}");
    }
}

/// Anything else refuses startup, naming the key and both spellings.
#[test]
fn another_word_is_refused() {
    for bad in [
        "\"kick\"",
        "\"\"",
        "\"Disconnect\"",
        "\"leave-room\"",
        "true",
        "1",
    ] {
        let e = parse(&format!("afk_action = {bad}"))
            .expect_err(bad)
            .to_string();
        assert!(e.contains("afk_action"), "{bad}: {e}");
        assert!(e.contains("\"leave_room\" or \"disconnect\""), "{bad}: {e}");
    }
}

/// Low to high: the core's `leave_room`, the game's default, the flat
/// key, the room's own — each written layer wins over the ones below.
#[test]
fn the_layers_override_low_to_high() {
    use AfkAction::{Disconnect as D, LeaveRoom as L};
    // Nothing written: the game's default (the core's without a game).
    assert_eq!(action("", 1, L), L);
    assert_eq!(action("", 1, D), D);
    assert_eq!(action("max_players = 5", 1, D), D, "other keys only");
    // The flat key over the game.
    assert_eq!(action("afk_action = \"leave_room\"", 1, D), L);
    assert_eq!(action("afk_action = \"disconnect\"", 1, L), D);
    // A room's own over both, for that room alone.
    let text = "afk_action = \"disconnect\"\n\
        [rooms.2]\nafk_action = \"leave_room\"\n\
        [rooms.3]\nmax_players = 4\n";
    assert_eq!(action(text, 1, L), D);
    assert_eq!(action(text, 2, D), L);
    assert_eq!(action(text, 3, L), D, "an override without the key");
    let text = "[rooms.2]\nafk_action = \"disconnect\"\n";
    assert_eq!(action(text, 1, L), L);
    assert_eq!(action(text, 2, L), D);
    // `Config::room_config` is the file's view: no game default.
    let cfg = parse("").expect("parses");
    assert_eq!(cfg.room_config(1).afk_action, L);
}
