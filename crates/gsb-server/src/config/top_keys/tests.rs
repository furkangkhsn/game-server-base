use super::*;

/// `text` as a parsed config file's table.
fn raw(text: &str) -> toml::Table {
    toml::from_str(text).expect("test toml parses")
}

/// A build whose catalog holds `games`, each owning its own table.
fn only(games: &[&'static str]) -> Vec<(&'static str, Vec<&'static str>)> {
    games.iter().map(|g| (*g, vec![*g])).collect()
}

/// The refusal of `text` under `owners`, as its message.
fn refused(text: &str, owners: &Owners) -> String {
    match check(&raw(text), owners) {
        Err(e @ ServerError::UnknownKey { .. }) => e.to_string(),
        Err(e) => panic!("{text}: not an unknown-key refusal: {e}"),
        Ok(()) => panic!("{text}: accepted"),
    }
}

/// The engine's keys are the struct's fields as the file spells them:
/// a renamed field under its file name, a skipped field absent.
#[test]
fn the_engine_keys_are_the_config_fields_as_the_file_spells_them() {
    let fields = config_fields();
    for key in ["tick_hz", "max_detach_hold_secs", "rooms", "listeners"] {
        assert!(fields.contains(&key), "`{key}` missing");
    }
    for key in ["metrics", "game", "listen_backlog", "http_listen"] {
        assert!(fields.contains(&key), "`{key}` missing");
    }
    for key in ["raw", "max_detach_hold"] {
        assert!(!fields.contains(&key), "`{key}` is not a file key");
    }
    let engine: Vec<&str> = engine_keys().collect();
    assert_eq!(engine.len() + DEMO_KEYS.len(), fields.len());
}

/// Every demo key is a `Config` field (a renamed field fails here), and
/// none of them is the engine's.
#[test]
fn the_demo_keys_are_config_fields_the_engine_does_not_own() {
    for key in DEMO_KEYS {
        assert!(config_fields().contains(key), "`{key}` is no field");
        assert!(!is_engine_key(key), "`{key}` counted as the engine's");
    }
}

/// Every engine key is accepted with no game owning anything.
#[test]
fn every_engine_key_is_accepted() {
    let mut table = toml::Table::new();
    for key in engine_keys() {
        table.insert(key.into(), toml::Value::Integer(1));
    }
    check(&table, &[]).expect("the engine's keys");
}

/// A typo'd engine key and misspelled engine tables: refused, named as
/// the file writes them, with the key they resemble.
#[test]
fn a_key_nobody_owns_is_refused_named_as_written() {
    let owners = only(&["arena"]);
    for (text, parts) in [
        ("tik_hz = 60", ["`tik_hz`", "did you mean `tick_hz`?"]),
        (
            "[metric.otlp]\nendpoint = \"x\"",
            ["`[metric.otlp]`", "did you mean `metrics`?"],
        ),
        (
            "[[listener]]\nbind = \"x\"",
            ["`[[listener]]`", "did you mean `listeners`?"],
        ),
        (
            "[room.2]\ntick_hz = 15",
            ["`[room.2]`", "did you mean `rooms`?"],
        ),
        ("[arnea]\nteams = 2", ["`[arnea]`", "did you mean `arena`?"]),
    ] {
        let msg = refused(text, &owners);
        for part in parts {
            assert!(msg.contains(part), "{text}: {part} missing: {msg}");
        }
        assert!(msg.contains("arena: `arena`"), "the games' keys: {msg}");
    }
    let msg = refused("zzzzzzzz = 1", &owners);
    assert!(!msg.contains("did you mean"), "nothing is close: {msg}");
}

/// A flat value written under a game's name, and a table with direct
/// keys: written as the file writes them.
#[test]
fn the_written_form_follows_the_value() {
    let owners = only(&[]);
    assert!(refused("[x]\na = 1\n[x.b]\nc = 2", &owners).contains("`[x]`"));
    assert!(refused("x = [1, 2]", &owners).contains("`x`"));
    assert!(refused("x = {}", &owners).contains("`[x]`"));
}

/// A name owns the key of that name, flat or table (and its sub-tables).
#[test]
fn an_owned_name_owns_a_flat_key_and_a_table() {
    let owners = vec![("tiny", vec!["tiny", "tiny_speed"])];
    for text in [
        "tiny_speed = 3",
        "[tiny]\na = 1",
        "[tiny.deep]\na = 1",
        "tiny = 2",
    ] {
        check(&raw(text), &owners).unwrap_or_else(|e| panic!("{text}: {e}"));
    }
}

/// The sibling rule, with the catalog spelled out: a table of a game
/// compiled into the build is accepted whichever game is hosted; the same
/// table in a build without that game is refused.
#[test]
fn a_sibling_table_is_accepted_only_where_its_game_is_compiled_in() {
    let text = "game = \"arena\"\n[arena]\nteams = 2\n[mmo]\nlogout = \"instant\"";
    check(&raw(text), &only(&["arena", "mmo"])).expect("mmo compiled in");
    let msg = refused(text, &only(&["arena"]));
    assert!(msg.contains("`[mmo]`"), "{msg}");
}

/// The demo's flat keys are the demo's: accepted where it is compiled in,
/// refused in a build without it; the demo owns no `[demo]` table.
#[test]
fn the_demo_keys_need_the_demo() {
    let demo = vec![("demo", DEMO_KEYS.to_vec())];
    let text = "visibility = \"team\"\nshard_count = 4";
    check(&raw(text), &demo).expect("the demo's keys");
    assert!(refused(text, &only(&["arena"])).contains("`shard_count`"));
    assert!(refused("[demo]\nvisibility = \"team\"", &demo).contains("`[demo]`"));
}

/// The distance behind the suggestion.
#[test]
fn distance_counts_edits() {
    assert_eq!(distance("tik_hz", "tick_hz"), 1);
    assert_eq!(distance("room", "rooms"), 1);
    assert_eq!(distance("abc", "abc"), 0);
    assert_eq!(distance("", "abc"), 3);
    assert_eq!(distance("kitten", "sitting"), 3);
}
