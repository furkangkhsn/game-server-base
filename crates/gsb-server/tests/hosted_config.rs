//! Game selection and the hosted games' settings from real config FILES
//! (GAME-MODULE §4.3, §6 decisions 2 and 3): `game` picks among all four
//! compiled-in games, an explicitly written key a game fixes refuses
//! startup, the game's own table is read, and a demo config — no `game`
//! key — starts the demo exactly as before.

#![cfg(all(
    feature = "game-demo",
    feature = "game-arena",
    feature = "game-mmo",
    feature = "game-war"
))]

mod common;
mod hosted;

use std::time::Duration;

use gsb_server::{GameError, ServerError};
use hosted::arena::ArenaView;
use hosted::{Client, Door, View, config_file, eventually};
use prost::Message;

/// The refusal a config file gets, as `(game, message)`.
async fn refusal(text: &str) -> (&'static str, String) {
    let cfg = config_file("refuse", &format!("bind = \"127.0.0.1:0\"\n{text}"));
    match gsb_server::start_server(cfg).await {
        Err(ServerError::Game(GameError::Module { game, source })) => (game, source.to_string()),
        Err(e) => panic!("`{text}`: wrong error kind: {e}"),
        Ok(_) => panic!("`{text}` must refuse startup"),
    }
}

#[tokio::test]
async fn an_unknown_game_lists_all_four() {
    let cfg = Door::Tcp.config("chess");
    let Err(e) = gsb_server::start_server(cfg).await else {
        panic!("an unknown game must not start");
    };
    let msg = e.to_string();
    for game in ["`demo`", "`arena`", "`mmo`", "`war`"] {
        assert!(msg.contains(game), "{game} missing: {msg}");
    }
    assert_eq!(
        gsb_server::games::compiled_in(),
        ["demo", "arena", "mmo", "war"]
    );
}

/// Every flat key a game fixes, written explicitly, refuses startup with
/// a message naming it (the MMO's grace naming where its timer lives).
#[tokio::test]
async fn explicitly_written_fixed_keys_refuse_startup() {
    for (game, line) in [
        ("arena", "visibility = \"team\""),
        ("arena", "team_vision_radius = 25.0"),
        ("arena", "shard_count = 1"),
        ("arena", "disconnect_grace_secs = 30.0"),
        ("mmo", "topology = \"sharded\""),
        ("mmo", "shard_count = 4"),
        ("mmo", "aoi_cell_size = 64.0"),
        ("mmo", "disconnect_grace_secs = 20.0"),
        ("war", "visibility = \"team\""),
        ("war", "shard_count = 4"),
        ("war", "team_vision_radius = 60.0"),
        ("war", "disconnect_grace_secs = 30.0"),
    ] {
        let (who, msg) = refusal(&format!("game = \"{game}\"\n{line}")).await;
        assert_eq!(who, game);
        let key = line.split(' ').next().unwrap();
        assert!(msg.contains(&format!("`{key}`")), "{game}/{key}: {msg}");
        assert!(msg.contains("fixed by this game"), "{msg}");
    }
    let (_, msg) = refusal("game = \"mmo\"\ndisconnect_grace_secs = 20.0").await;
    assert!(msg.contains("[mmo] logout_grace_secs"), "{msg}");
    let (_, msg) = refusal("game = \"arena\"\n[arena]\nteam = 2").await;
    assert!(msg.contains("unknown key `arena.team`"), "{msg}");
    let (_, msg) = refusal("game = \"war\"\ndisconnect_grace_secs = 5.0").await;
    assert!(msg.contains("[war] disconnect_grace_secs"), "{msg}");
    let (_, msg) = refusal("game = \"war\"\n[war]\nteam_budget = 0").await;
    assert!(msg.contains("war.team_budget"), "{msg}");
}

/// `[arena] teams = 2` from a file: the third joiner is dealt onto team 0
/// and sees its team-mate at once (with the default three teams it would
/// be alone on team 2 — `arena_e2e` asserts that).
#[tokio::test]
async fn the_arena_table_is_read_from_a_file() {
    let cfg = config_file(
        "arena-teams",
        "game = \"arena\"\nbind = \"127.0.0.1:0\"\n[arena]\nteams = 2\n",
    );
    let handle = gsb_server::start_server(cfg)
        .await
        .expect("the arena starts");
    let mut a: Client<ArenaView> = Client::join(&Door::Tcp, handle.addr, "t-a", 1).await;
    let mut b: Client<ArenaView> = Client::join(&Door::Tcp, handle.addr, "t-b", 1).await;
    let mut c: Client<ArenaView> = Client::join(&Door::Tcp, handle.addr, "t-c", 1).await;
    let (ia, ic) = (a.entity, c.entity);
    let mut both = vec![ia, ic];
    both.sort_unstable();
    eventually(
        &mut [&mut a, &mut b, &mut c],
        Duration::from_secs(5),
        "A and C share team 0; B is alone",
        |cs| {
            cs[0].view.sees() == both
                && cs[2].view.sees() == both
                && cs[1].view.sees() == vec![cs[1].entity]
        },
    )
    .await;
    handle.stop().await;
}

/// The demo's frames, decoded just enough to find the joiner.
#[derive(Default)]
struct DemoView {
    seen: Vec<u64>,
}

impl View for DemoView {
    fn apply(&mut self, op: u16, payload: &[u8]) {
        if op == gsb_demo::op::WORLD_SNAPSHOT {
            let s = gsb_demo::game::WorldSnapshot::decode(payload).expect("demo snapshot");
            self.seen = s.entities.iter().map(|e| e.entity).collect();
        }
    }
}

/// A pre-module config — no `game` key, the demo's flat keys — starts
/// the demo exactly as before, and another game's table in the same file
/// is none of its business.
#[tokio::test]
async fn a_demo_config_without_a_game_key_still_hosts_the_demo() {
    let cfg = config_file(
        "demo",
        "bind = \"127.0.0.1:0\"\nvisibility = \"team\"\ndisconnect_grace_secs = 5.0\n\
         [mmo]\nlogout = \"bot\"\n",
    );
    assert_eq!(cfg.game, "demo");
    let handle = gsb_server::start_server(cfg)
        .await
        .expect("the demo starts");
    let mut c: Client<DemoView> = Client::join(&Door::Tcp, handle.addr, "demo-1", 1).await;
    let me = c.entity;
    eventually(
        &mut [&mut c],
        Duration::from_secs(5),
        "a demo snapshot",
        |cs| cs[0].view.seen.contains(&me),
    )
    .await;
    handle.stop().await;
}
