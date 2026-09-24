//! The game-module seam from the outside (docs/GAME-MODULE.md §4.1):
//! game selection by the `game` config key, and a module written in
//! another crate — this test crate — hosted through `start_game_server`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bevy_ecs::world::World;
use gsb_core::registry::{BuiltRoom, RoomFactory};
use gsb_core::room::RoomLogic;
use gsb_demo::prelude::*;
use gsb_protocol::MessageTable;
use gsb_protocol::base::{Auth, JoinRoom, JoinRoomResult};
use gsb_server::{Config, GameError, GameModule, RegistryParts, RegistryTask, ServerError};
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

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

async fn write_frame(s: &mut TcpStream, op: u16, payload: &[u8]) {
    let mut out = Vec::with_capacity(6 + payload.len());
    out.extend_from_slice(&((2 + payload.len()) as u32).to_le_bytes());
    out.extend_from_slice(&op.to_le_bytes());
    out.extend_from_slice(payload);
    s.write_all(&out).await.expect("write frame");
}

async fn read_frame(s: &mut TcpStream) -> (u16, Vec<u8>) {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).await.expect("frame length");
    let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
    s.read_exact(&mut body).await.expect("frame body");
    (u16::from_le_bytes([body[0], body[1]]), body[2..].to_vec())
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
    let mut s = TcpStream::connect(handle.addr).await.expect("connect");
    let auth = Auth {
        name: "tiny-1".into(),
        ticket: Vec::new(),
        protocol_version: gsb_protocol::PROTOCOL_VERSION,
    };
    write_frame(
        &mut s,
        gsb_protocol::op::base::AUTH_REQ,
        &auth.encode_to_vec(),
    )
    .await;
    let join = JoinRoom { room_id: 1 }.encode_to_vec();
    write_frame(&mut s, gsb_protocol::op::base::JOIN_ROOM_REQ, &join).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    let entity = loop {
        assert!(Instant::now() < deadline, "timed out waiting for the join");
        let (op, payload) = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut s))
            .await
            .expect("a frame");
        if op == gsb_protocol::op::base::JOIN_ROOM_RESULT {
            break JoinRoomResult::decode(&payload[..]).unwrap().entity;
        }
        assert_ne!(op, gsb_protocol::op::base::ERROR, "join failed");
    };
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
