//! The arena's settings: what `[arena]` reads, and every refusal.

use std::time::Duration;

use super::*;
use crate::GameError;
use crate::games::settings::SettingsError;

fn configure(toml_text: &str) -> Result<Settings, ServerError> {
    let raw: toml::Table = toml::from_str(toml_text).expect("test toml parses");
    let mut m = ArenaModule::new();
    m.configure(&raw, &Config::default())?;
    Ok(m.settings.expect("configured"))
}

/// The module's own error, unwrapped from the server error.
fn refusal(toml_text: &str) -> SettingsError {
    match configure(toml_text) {
        Err(ServerError::Game(GameError::Module { game, source })) => {
            assert_eq!(game, "arena");
            *source
                .downcast::<SettingsError>()
                .expect("a settings error")
        }
        Err(e) => panic!("wrong error kind: {e}"),
        Ok(s) => panic!("`{toml_text}` must be refused, got {s:?}"),
    }
}

#[test]
fn no_table_means_the_arenas_own_defaults() {
    let s = configure("game = \"arena\"").expect("defaults");
    assert_eq!(s.teams, DEFAULT_TEAMS);
    assert_eq!(s.grace, gsb_kit::DEFAULT_DISCONNECT_GRACE);
}

#[test]
fn the_arena_table_is_read() {
    let s = configure("[arena]\nteams = 2\ndisconnect_grace_secs = 1.5").expect("valid");
    assert_eq!(s.teams, 2);
    assert_eq!(s.grace, Duration::from_millis(1500));
    let s = configure("[arena]\ndisconnect_grace_secs = 0").expect("integer seconds");
    assert_eq!(s.grace, Duration::ZERO);
}

/// Every flat key the arena fixes is refused when written, naming the
/// key — at the top level and inside `[arena]` alike.
#[test]
fn every_fixed_key_is_refused_when_written() {
    for (key, value) in [
        ("visibility", "\"team\""),
        ("topology", "\"single\""),
        ("communication", "\"always-full\""),
        ("shard_count", "4"),
        ("aoi_cell_size", "20.0"),
        ("team_vision_radius", "15.0"),
        ("spawn_half_size", "50.0"),
        ("disconnect_grace_secs", "30.0"),
    ] {
        match refusal(&format!("{key} = {value}")) {
            SettingsError::Fixed { key: k, .. } => assert_eq!(k, key),
            other => panic!("{key}: {other}"),
        }
        if key == "disconnect_grace_secs" {
            continue; // inside `[arena]` it is the arena's own key
        }
        match refusal(&format!("[arena]\n{key} = {value}")) {
            SettingsError::Fixed { key: k, .. } => assert_eq!(k, format!("arena.{key}")),
            other => panic!("{key} inside [arena]: {other}"),
        }
    }
}

#[test]
fn unknown_keys_and_bad_values_are_refused() {
    assert!(matches!(
        refusal("[arena]\nteam = 3"),
        SettingsError::Unknown { key, .. } if key == "team"
    ));
    for bad in ["teams = 0", "teams = 256", "teams = \"3\"", "teams = 2.5"] {
        assert!(
            matches!(refusal(&format!("[arena]\n{bad}")), SettingsError::BadValue { key, .. } if key == "arena.teams"),
            "{bad}"
        );
    }
    assert!(matches!(
        refusal("[arena]\ndisconnect_grace_secs = -1"),
        SettingsError::BadValue { .. }
    ));
    assert!(matches!(
        refusal("arena = 3"),
        SettingsError::BadValue { key, .. } if key == "arena"
    ));
}

/// Another game's table is none of the arena's business.
#[test]
fn other_games_tables_are_ignored() {
    let s = configure("[mmo]\nlogout = \"bot\"\n[arena]\nteams = 4").expect("valid");
    assert_eq!(s.teams, 4);
}
