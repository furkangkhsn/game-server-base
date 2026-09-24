//! `config.example.toml` against the hosted games (GAME-MODULE G2): its
//! commented `[arena]` / `[mmo]` sections, uncommented, are accepted by
//! their games; and — a finding — the file as shipped cannot simply be
//! switched to another game, because it writes the demo's flat keys
//! explicitly and the other games refuse them. Configure only: nothing
//! binds (the example's port is a real one).

#![cfg(all(feature = "game-demo", feature = "game-arena", feature = "game-mmo"))]

use gsb_server::{Config, GameError, ServerError};

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

/// The demo's flat keys the example writes explicitly.
const DEMO_KEYS: [&str; 5] = [
    "visibility",
    "aoi_cell_size",
    "team_vision_radius",
    "spawn_half_size",
    "disconnect_grace_secs",
];

#[test]
fn the_example_hosts_the_demo_as_shipped() {
    let cfg = load(EXAMPLE);
    assert_eq!(cfg.game, "demo");
    configure("demo", &cfg).expect("the demo takes the example");
}

/// FINDING: `game = "arena"` / `"mmo"` in a copy of the example refuses
/// startup on the first demo key it writes (`visibility`) — the
/// operator has to delete the demo's keys first.
#[test]
fn switching_the_example_to_another_game_trips_on_the_demo_keys() {
    let cfg = load(EXAMPLE);
    for game in ["arena", "mmo"] {
        match configure(game, &cfg) {
            Err(ServerError::Game(GameError::Module { source, .. })) => {
                let msg = source.to_string();
                assert!(msg.contains("`visibility`"), "{game}: {msg}");
            }
            other => panic!("{game}: expected a refusal, got {other:?}"),
        }
    }
}

/// The commented `[arena]` / `[mmo]` examples, uncommented (and the
/// demo's flat keys dropped), are accepted by their games.
#[test]
fn the_commented_game_tables_are_valid() {
    let mut text = String::new();
    for line in EXAMPLE.lines() {
        let flat = line.split([' ', '=']).next().unwrap_or("");
        if DEMO_KEYS.contains(&flat) {
            continue;
        }
        let game_line = [
            "#[arena]",
            "#teams",
            "#disconnect_grace_secs",
            "#[mmo]",
            "#logout",
        ]
        .iter()
        .any(|p| line.starts_with(p));
        text.push_str(if game_line { &line[1..] } else { line });
        text.push('\n');
    }
    let cfg = load(&text);
    let raw = &cfg.raw;
    assert!(raw.get("arena").is_some_and(|t| t.get("teams").is_some()));
    assert!(raw.get("mmo").is_some_and(|t| t.get("logout").is_some()));
    for game in ["arena", "mmo"] {
        if let Err(e) = configure(game, &cfg) {
            panic!("{game} refused its own example table: {e}");
        }
    }
    configure("demo", &cfg).expect("the demo ignores the games' tables");
}
