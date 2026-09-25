//! The MMO's settings and its join router.

use super::*;
use crate::GameError;
use crate::games::settings::SettingsError;

fn configure(toml_text: &str) -> Result<Settings, ServerError> {
    let raw: toml::Table = toml::from_str(toml_text).expect("test toml parses");
    let mut m = MmoModule::new();
    m.configure(&raw, &Config::default())?;
    Ok(m.settings.expect("configured"))
}

fn refusal(toml_text: &str) -> SettingsError {
    match configure(toml_text) {
        Err(ServerError::Game(GameError::Module { game, source })) => {
            assert_eq!(game, "mmo");
            *source
                .downcast::<SettingsError>()
                .expect("a settings error")
        }
        Err(e) => panic!("wrong error kind: {e}"),
        Ok(s) => panic!("`{toml_text}` must be refused, got {s:?}"),
    }
}

#[test]
fn no_table_means_the_mmos_own_logout() {
    let s = configure("game = \"mmo\"").expect("defaults");
    assert_eq!(s.grace, LOGOUT_GRACE);
    assert_eq!(s.to, ExpireTo::Despawn);
    assert!(s.crystallize, "the MMO crystallizes its cross-seam fights");
}

#[test]
fn crystallization_can_be_turned_off() {
    let s = configure("[mmo]\ncrystallize = false").expect("valid");
    assert!(!s.crystallize);
    assert!(
        configure("[mmo]\ncrystallize = true")
            .expect("valid")
            .crystallize
    );
    assert!(matches!(
        refusal("[mmo]\ncrystallize = \"no\""),
        SettingsError::BadValue { key, .. } if key == "mmo.crystallize"
    ));
}

#[test]
fn the_mmo_table_is_read() {
    let s = configure("[mmo]\nlogout_grace_secs = 2\nlogout = \"bot\"").expect("valid");
    assert_eq!(s.grace, Duration::from_secs(2));
    assert_eq!(s.to, ExpireTo::AiHandover);
    let s = configure("[mmo]\nlogout = \"release\"").expect("valid");
    assert_eq!(s.to, ExpireTo::Despawn);
}

#[test]
fn every_fixed_key_is_refused_when_written() {
    for (key, value) in [
        ("visibility", "\"spatial\""),
        ("topology", "\"sharded\""),
        ("communication", "\"delta\""),
        ("shard_count", "4"),
        ("aoi_cell_size", "64.0"),
        ("team_vision_radius", "25.0"),
        ("spawn_half_size", "50.0"),
        ("disconnect_grace_secs", "20.0"),
    ] {
        match refusal(&format!("{key} = {value}")) {
            SettingsError::Fixed { key: k, why } => {
                assert_eq!(k, key);
                if key == "disconnect_grace_secs" {
                    assert!(why.contains("logout_grace_secs"), "{why}");
                }
            }
            other => panic!("{key}: {other}"),
        }
        match refusal(&format!("[mmo]\n{key} = {value}")) {
            SettingsError::Fixed { key: k, .. } => assert_eq!(k, format!("mmo.{key}")),
            other => panic!("{key} inside [mmo]: {other}"),
        }
    }
}

#[test]
fn unknown_keys_and_bad_values_are_refused() {
    assert!(matches!(
        refusal("[mmo]\nrealm = \"standard\""),
        SettingsError::Unknown { key, .. } if key == "realm"
    ));
    assert!(matches!(
        refusal("[mmo]\nlogout = \"despawn\""),
        SettingsError::BadValue { key, .. } if key == "mmo.logout"
    ));
    assert!(matches!(
        refusal("[mmo]\nlogout_grace_secs = \"20\""),
        SettingsError::BadValue { key, .. } if key == "mmo.logout_grace_secs"
    ));
}

/// The router and `spawn_player` agree for a session with no save: the
/// default shard is the one owning the default waystone, and every
/// waystone lies in the region of the shard whose fallback it is.
#[test]
fn unsaved_sessions_go_to_the_default_waystones_shard() {
    assert_eq!(default_shard(), DEFAULT_WAYSTONE);
    for (i, [x, z]) in WAYSTONES.into_iter().enumerate() {
        assert_eq!(home_shard(&Pos3::new(x, 0.0, z)), i, "waystone {i}");
    }
    let realm = Realm::empty().with_login("ann", Pos3::new(300.0, 0.0, 300.0));
    assert_eq!(route(&realm, "ann"), 3, "a save routes to its shard");
    assert_eq!(route(&realm, "bob"), default_shard(), "an unknown player");
    assert_eq!(route(&realm, ""), default_shard(), "an anonymous session");
}
