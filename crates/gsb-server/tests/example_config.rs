//! `config.example.toml` against the hosted games (GAME-MODULE G2): the
//! file as shipped hosts the demo exactly as its documented defaults
//! would; a copy switched to another game by its `game` line alone
//! starts that game (the demo's flat keys, which the other games
//! refuse, are written commented out — finding K5); and its commented
//! `[arena]` / `[mmo]` / `[war]` sections, uncommented, are accepted by
//! their games. Configure only: nothing binds (the example's port is a real
//! one).

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

/// Configure `game` under `cfg`, as the server does before binding.
fn configure(game: &str, cfg: &Config) -> Result<(), ServerError> {
    let mut module = gsb_server::games::by_name(game).expect("compiled in");
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
