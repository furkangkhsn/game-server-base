//! `config.example.toml` against the hosted games (GAME-MODULE G2): the
//! file as shipped hosts the demo exactly as its documented defaults
//! would; a copy switched to another game by its `game` line alone
//! starts that game (the demo's flat keys, which the other games
//! refuse, are written commented out — finding K5); and its commented
//! `[arena]` / `[mmo]` / `[war]` sections, uncommented, are accepted by
//! their games, and so is its commented `[rooms.2]` override (B18) and
//! its commented two-door `[[listeners]]` example (F61).
//! Check and configure only: nothing binds (the example's port is a
//! real one).

#![cfg(all(
    feature = "game-demo",
    feature = "game-arena",
    feature = "game-mmo",
    feature = "game-war"
))]

use gsb_server::{Config, ServerError};

const EXAMPLE: &str = include_str!("../../../config.example.toml");

/// `text` as a config FILE would load it (`Config::raw` included).
fn load(text: &str) -> Config {
    let mut cfg: Config = toml::from_str(text).expect("the example parses");
    cfg.raw = toml::from_str(text).expect("the example parses as a table");
    cfg
}

/// Check and configure `game` under `cfg`, as the server does before
/// binding: the file's top-level keys (F62), then the game's settings.
fn configure(game: &str, cfg: &Config) -> Result<(), ServerError> {
    let mut module = gsb_server::games::by_name(game).expect("compiled in");
    cfg.check_top_level_keys(&*module)?;
    module.configure(&cfg.raw, cfg)
}

/// The demo's flat keys the example documents (commented out, at their
/// defaults).
const DEMO_KEYS: [&str; 5] = [
    "visibility",
    "aoi_cell_size",
    "team_vision_radius",
    "spawn_half_size",
    "disconnect_grace_secs",
];

/// The line that opens the games' own tables (commented): everything
/// before it is the flat, top-level part of the file.
const TABLES_START: &str = "#[arena]";

/// The example with its commented demo keys uncommented (the flat part
/// only — `[arena]` has its own `disconnect_grace_secs`): the file as it
/// shipped before K5, every demo key written explicitly.
fn with_demo_keys_written() -> String {
    let mut text = String::new();
    let mut flat = true;
    let mut written = Vec::new();
    for line in EXAMPLE.lines() {
        flat &= line != TABLES_START;
        let key = line.strip_prefix('#').and_then(|l| l.split(" = ").next());
        match key {
            Some(k) if flat && DEMO_KEYS.contains(&k) => {
                written.push(k);
                text.push_str(&line[1..]);
            }
            _ => text.push_str(line),
        }
        text.push('\n');
    }
    written.sort_unstable();
    let mut expected = DEMO_KEYS;
    expected.sort_unstable();
    assert_eq!(
        written, expected,
        "each demo key documented once, commented"
    );
    text
}

/// The example as shipped hosts the demo, and exactly as the demo keys it
/// documents would: uncommenting them changes nothing (same resolved
/// selection, same demo settings) — the commented values ARE the
/// defaults, and the file resolves as it did when it wrote them.
#[test]
fn the_example_hosts_the_demo_as_shipped() {
    let cfg = load(EXAMPLE);
    assert_eq!(cfg.game, "demo");
    configure("demo", &cfg).expect("the demo takes the example");
    for key in DEMO_KEYS {
        assert!(cfg.raw.get(key).is_none(), "`{key}` is written commented");
    }

    let written = load(&with_demo_keys_written());
    configure("demo", &written).expect("the demo takes its keys");
    assert_eq!(
        cfg.resolve_selection().expect("resolves"),
        written.resolve_selection().expect("resolves"),
        "the same room selection"
    );
    assert_eq!(cfg.visibility, written.visibility);
    assert_eq!(cfg.aoi_cell_size, written.aoi_cell_size);
    assert_eq!(cfg.team_vision_radius, written.team_vision_radius);
    assert_eq!(cfg.spawn_half_size, written.spawn_half_size);
    assert_eq!(cfg.disconnect_grace_secs, written.disconnect_grace_secs);
}

/// K5 fixed: a copy of the example switched to another game by its
/// `game` line ALONE starts that game — the file writes none of the keys
/// the other games refuse.
#[test]
fn switching_the_example_to_another_game_needs_only_the_game_line() {
    for game in ["demo", "arena", "mmo", "war"] {
        let text = EXAMPLE.replacen("game = \"demo\"", &format!("game = \"{game}\""), 1);
        let cfg = load(&text);
        assert_eq!(cfg.game, game);
        if let Err(e) = configure(game, &cfg) {
            panic!("{game} refused the example: {e}");
        }
    }
}

/// The commented `[arena]` / `[mmo]` / `[war]` examples, uncommented, are accepted
/// by their games (and ignored by the demo).
#[test]
fn the_commented_game_tables_are_valid() {
    let mut text = String::new();
    let mut tables = false;
    for line in EXAMPLE.lines() {
        tables |= line == TABLES_START;
        let game_line = tables
            && [
                "#[arena]",
                "#teams",
                "#disconnect_grace_secs",
                "#[mmo]",
                "#logout",
                "#[war]",
                "#team_budget",
            ]
            .iter()
            .any(|p| line.starts_with(p));
        text.push_str(if game_line { &line[1..] } else { line });
        text.push('\n');
    }
    let cfg = load(&text);
    let raw = &cfg.raw;
    assert!(raw.get("arena").is_some_and(|t| t.get("teams").is_some()));
    assert!(
        raw.get("arena")
            .is_some_and(|t| t.get("disconnect_grace_secs").is_some())
    );
    assert!(raw.get("mmo").is_some_and(|t| t.get("logout").is_some()));
    let war = raw.get("war");
    assert!(war.is_some_and(|t| t.get("team_budget").is_some()));
    assert!(war.is_some_and(|t| t.get("disconnect_grace_secs").is_some()));
    for game in ["arena", "mmo", "war"] {
        if let Err(e) = configure(game, &cfg) {
            panic!("{game} refused its own example table: {e}");
        }
    }
    configure("demo", &cfg).expect("the demo ignores the games' tables");
}

/// The commented `[rooms.2]` example (B18), uncommented, gives room 2 its
/// own keys under the rate rule, leaves room 1 the server's room, and
/// every game still takes the file; as shipped, no room is overridden.
#[test]
fn the_commented_room_override_is_valid() {
    let mut text = String::new();
    let mut block = false;
    for line in EXAMPLE.lines() {
        block = line == "#[rooms.2]" || (block && line.starts_with('#') && line.contains(" = "));
        text.push_str(if block { &line[1..] } else { line });
        text.push('\n');
    }
    let shipped = load(EXAMPLE);
    assert!(shipped.rooms.is_empty(), "written commented");
    let cfg = load(&text);
    assert_eq!(cfg.rooms.keys().copied().collect::<Vec<_>>(), [2]);
    let two = cfg.room_config(2);
    assert_eq!((two.max_players, two.tick_hz), (Some(2000), 10.0));
    assert_eq!(two.step_divisor(cfg.tick_hz).ok(), Some(3), "10 divides 30");
    assert_eq!(cfg.room_config(1), shipped.room_config(1));
    for game in ["demo", "arena", "mmo", "war"] {
        let text = text.replacen("game = \"demo\"", &format!("game = \"{game}\""), 1);
        if let Err(e) = configure(game, &load(&text)) {
            panic!("{game} refused the room override: {e}");
        }
    }
}

/// The commented two-door `[[listeners]]` example's lines, uncommented,
/// and the example with every other line as shipped.
fn listener_example() -> (String, String) {
    const START: &str = "# Example: an encrypted public door plus a plaintext LAN door:";
    let (mut doors, mut rest) = (String::new(), String::new());
    let mut block = false;
    for line in EXAMPLE.lines() {
        block = line == START || (block && line != "# Rules:");
        let (out, text) = match line.strip_prefix("#   ").filter(|_| block) {
            Some(entry) => (&mut doors, entry),
            None => (&mut rest, line),
        };
        out.push_str(text);
        out.push('\n');
    }
    (doors, rest)
}

/// The commented two-door `[[listeners]]` example, written where the
/// example says (after every flat key), parses under the strict entry
/// grammar (F61) — every key it writes is one an entry takes. Written in
/// place instead, the flat keys after it would belong to its last entry:
/// that now refuses startup, naming one of them, where it used to drop
/// every one of them silently.
#[test]
fn the_commented_listener_example_is_valid() {
    let (doors, rest) = listener_example();
    let cfg = load(&format!("{rest}\n{doors}"));
    let entries = cfg.listeners.expect("the example's doors");
    let got: Vec<_> = entries
        .iter()
        .map(|e| (e.transport.to_string(), e.bind.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            ("tls".to_owned(), "0.0.0.0:7777"),
            ("tcp".to_owned(), "192.168.1.10:7777")
        ]
    );
    assert!(entries[0].tls_cert.is_some() && entries[0].tls_key.is_some());
    assert_eq!(
        cfg.tick_hz,
        load(EXAMPLE).tick_hz,
        "the flat keys still read"
    );
    assert!(load(EXAMPLE).listeners.is_none(), "written commented");

    let in_place = EXAMPLE.replacen(
        "#   [[listeners]]\n#   transport = \"tcp\"\n#   bind = \"192.168.1.10:7777\"",
        "[[listeners]]\ntransport = \"tcp\"\nbind = \"192.168.1.10:7777\"",
        1,
    );
    assert_ne!(in_place, EXAMPLE, "the example's last door, uncommented");
    let e = toml::from_str::<Config>(&in_place).expect_err("flat keys inside an entry");
    assert!(e.to_string().contains("unknown field `"), "{e}");
}
