//! The war's settings and its join router.

use super::*;
use crate::GameError;
use crate::games::settings::SettingsError;
use gsb_demo_war::Pos3;
use gsb_demo_war::realm::faction_of;
use gsb_demo_war::world::BASES;

fn configure(toml_text: &str) -> Result<Settings, ServerError> {
    let raw: toml::Table = toml::from_str(toml_text).expect("test toml parses");
    let mut m = WarModule::new();
    m.configure(&raw, &Config::default())?;
    Ok(m.settings.expect("configured"))
}

fn refusal(toml_text: &str) -> SettingsError {
    match configure(toml_text) {
        Err(ServerError::Game(GameError::Module { game, source })) => {
            assert_eq!(game, "war");
            *source
                .downcast::<SettingsError>()
                .expect("a settings error")
        }
        Err(e) => panic!("wrong error kind: {e}"),
        Ok(s) => panic!("`{toml_text}` must be refused, got {s:?}"),
    }
}

#[test]
fn no_table_means_the_kits_defaults() {
    let s = configure("game = \"war\"").expect("defaults");
    assert_eq!(s.grace, gsb_kit::DEFAULT_DISCONNECT_GRACE);
    assert_eq!(s.budget, DEFAULT_TEAM_BUDGET);
}

#[test]
fn the_war_table_is_read() {
    let s = configure("[war]\ndisconnect_grace_secs = 2.5\nteam_budget = 64").expect("valid");
    assert_eq!(s.grace, Duration::from_millis(2500));
    assert_eq!(s.budget, 64);
    let s = configure("[war]\nteam_budget = 16384").expect("the core's cap");
    assert_eq!(s.budget, 16_384);
}

#[test]
fn every_fixed_key_is_refused_when_written() {
    for (key, value) in [
        ("visibility", "\"team\""),
        ("topology", "\"sharded\""),
        ("communication", "\"delta\""),
        ("shard_count", "4"),
        ("aoi_cell_size", "64.0"),
        ("team_vision_radius", "60.0"),
        ("spawn_half_size", "50.0"),
        ("disconnect_grace_secs", "20.0"),
    ] {
        match refusal(&format!("{key} = {value}")) {
            SettingsError::Fixed { key: k, why } => {
                assert_eq!(k, key);
                if key == "disconnect_grace_secs" {
                    assert!(why.contains("[war] disconnect_grace_secs"), "{why}");
                }
            }
            other => panic!("{key}: {other}"),
        }
    }
    // Inside the table, only the flat keys the table does not know.
    match refusal("[war]\nshard_count = 4") {
        SettingsError::Fixed { key, .. } => assert_eq!(key, "war.shard_count"),
        other => panic!("{other}"),
    }
}

#[test]
fn unknown_keys_and_bad_values_are_refused() {
    assert!(matches!(
        refusal("[war]\nfactions = 4"),
        SettingsError::Unknown { key, .. } if key == "factions"
    ));
    for bad in ["0", "16385", "\"many\"", "1.5"] {
        assert!(
            matches!(
                refusal(&format!("[war]\nteam_budget = {bad}")),
                SettingsError::BadValue { key, .. } if key == "war.team_budget"
            ),
            "{bad}"
        );
    }
    assert!(matches!(
        refusal("[war]\ndisconnect_grace_secs = -1"),
        SettingsError::BadValue { key, .. } if key == "war.disconnect_grace_secs"
    ));
}

/// The router and the spawn agree: a saved character goes to the shard
/// of its saved spot; an unsaved (or anonymous) player to its hashed
/// faction's base, which lies in that faction's region.
#[test]
fn the_router_follows_the_placement() {
    let realm = Realm::empty().with_login("ann", Team(2), Pos3::ground(300.0, 300.0));
    assert_eq!(route(&realm, "ann"), 3, "a save routes to its shard");
    for who in ["bob", "", "lg-7"] {
        let f = usize::from(faction_of(who).0);
        assert_eq!(route(&realm, who), f, "{who:?}: its faction's base");
        let [x, z] = BASES[f];
        assert_eq!(home_shard(&Pos3::ground(x, z)), f);
    }
}
