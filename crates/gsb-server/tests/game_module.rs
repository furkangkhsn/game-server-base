//! The game-module seam from the outside (docs/GAME-MODULE.md §4.1):
//! game selection by the `game` config key, and a module written in
//! another crate — this test crate — hosted through `start_game_server`.

use std::sync::Arc;
use std::time::Duration;

use bevy_ecs::world::World;
use gsb_client::session::{self, Credentials};
use gsb_core::registry::{BuiltRoom, RoomFactory};
use gsb_core::room::RoomLogic;
use gsb_demo::prelude::*;
use gsb_protocol::MessageTable;
use gsb_server::{Config, GameError, GameModule, RegistryParts, RegistryTask, ServerError};

fn local_cfg() -> Config {
    Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        ..Default::default()
    }
}

/// A third-party-shaped module: the demo's open room under its own name,
/// with its own factory handed to `RegistryParts::spawn` (the one generic
/// method), and an optional refusal to exercise module errors.
struct Tiny {
    refuse: bool,
}

impl GameModule for Tiny {
    fn name(&self) -> &'static str {
        "tiny"
    }

    fn configure(&mut self, _raw: &toml::Table, _engine: &Config) -> Result<(), ServerError> {
        if self.refuse {
            return Err(GameError::Module {
                game: "tiny",
                source: "tiny refuses this config".into(),
            }
            .into());
        }
        Ok(())
    }

    fn register(&self, table: &mut MessageTable) {
        gsb_demo::register(table);
    }

    fn spawn_registry(&self, parts: RegistryParts) -> RegistryTask {
        let factory: RoomFactory<World, (), (), ()> = Arc::new(|_id, _config| BuiltRoom::Single {
            world: World::new(),
            logic: Box::new(gsb_demo::room::OpenRoom::new())
                as Box<dyn RoomLogic<World, GroupKey = (), Strip = ()>>,
        });
        parts.spawn(factory)
    }

    fn describe(&self) -> String {
        "tiny: one open room".into()
    }
}

/// An unknown `game` refuses startup, naming the compiled-in games.
#[tokio::test]
async fn unknown_game_refuses_startup_listing_the_compiled_in_games() {
    let cfg = Config {
        game: "chess".into(),
        ..local_cfg()
    };
    match gsb_server::start_server(cfg).await {
        Err(e @ ServerError::Game(GameError::Unknown { .. })) => {
            let ServerError::Game(GameError::Unknown { name, compiled_in }) = &e else {
                unreachable!()
            };
            assert_eq!(name, "chess");
            assert_eq!(compiled_in, &gsb_server::games::compiled_in());
            assert!(compiled_in.contains(&"demo"));
            let msg = e.to_string();
            assert!(msg.contains("`chess`") && msg.contains("`demo`"), "{msg}");
        }
        Ok(_) => panic!("an unknown game must not start"),
        Err(e) => panic!("wrong error kind: {e}"),
    }
}

/// The key defaults to the demo; a config file can name it explicitly,
/// and the parsed file is kept whole in `Config::raw`.
#[test]
fn game_key_defaults_to_the_demo_and_parses_from_a_file() {
    assert_eq!(Config::default().game, "demo");
    assert!(Config::default().raw.is_empty());
    let path = std::env::temp_dir().join(format!("gsb-game-key-{}.toml", std::process::id()));
    std::fs::write(&path, "game = \"demo\"\nshard_count = 9\n").expect("write");
    let cfg = Config::from_file(&path).expect("parse");
    let _ = std::fs::remove_file(&path);
    assert_eq!(cfg.game, "demo");
    assert_eq!(cfg.raw.get("game").and_then(|v| v.as_str()), Some("demo"));
    assert_eq!(
        cfg.raw.get("shard_count").and_then(|v| v.as_integer()),
        Some(9)
    );
    assert!(
        cfg.raw.get("visibility").is_none(),
        "defaults are not written"
    );
}

/// A module from another crate hosts its rooms: a client joins them.
#[tokio::test]
async fn a_module_from_another_crate_hosts_its_rooms() {
    let handle = gsb_server::start_game_server(Box::new(Tiny { refuse: false }), local_cfg())
        .await
        .expect("the tiny module starts");
    let mut s = gsb_client::connect::tcp(handle.addr)
        .await
        .expect("connect");
    let entity = session::auth_and_join(
        &mut s,
        &Credentials::named("tiny-1"),
        1,
        Duration::from_secs(10),
        |_| {},
    )
    .await
    .unwrap_or_else(|e| panic!("join failed: {e}"))
    .entity;
    assert_ne!(entity, 0);
    handle.stop().await;
}

/// A module's own error reaches the caller as `ServerError::Game`.
#[tokio::test]
async fn a_module_error_refuses_startup() {
    match gsb_server::start_game_server(Box::new(Tiny { refuse: true }), local_cfg()).await {
        Err(ServerError::Game(GameError::Module { game, source })) => {
            assert_eq!(game, "tiny");
            assert_eq!(source.to_string(), "tiny refuses this config");
        }
        Ok(_) => panic!("a refusing module must not start"),
        Err(e) => panic!("wrong error kind: {e}"),
    }
}

/// An explicit module never silently hosts something other than what a
/// config file's `game` key asked for.
#[tokio::test]
async fn an_explicit_module_refuses_a_file_naming_another_game() {
    let mut cfg = local_cfg();
    cfg.raw
        .insert("game".into(), toml::Value::String("demo".into()));
    match gsb_server::start_game_server(Box::new(Tiny { refuse: false }), cfg).await {
        Err(ServerError::Game(GameError::NameMismatch { configured, module })) => {
            assert_eq!(configured, "demo");
            assert_eq!(module, "tiny");
        }
        Ok(_) => panic!("a mismatched game key must not start"),
        Err(e) => panic!("wrong error kind: {e}"),
    }
}
