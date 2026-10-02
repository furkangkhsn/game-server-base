//! Several MMO worlds on one server (GAME-MODULE G2): every room id is a
//! whole sharded group of its own — `room_count > 1` at startup and a
//! room opened at runtime through the ops surface (`POST /rooms/open`),
//! which gets the server's room-level keys like a pre-created one.

#![cfg(feature = "game-mmo")]

mod common;
mod hosted;

use std::time::{Duration, Instant};

use gsb_demo_mmo::components::Kind;
use gsb_demo_mmo::mmo;
use gsb_demo_mmo::{MobSpawn, Pos3, Realm};
use gsb_server::games::mmo::MmoModule;
use hosted::mmo::{MmoView, dm, ground};
use hosted::{Client, Door, eventually, hold, http, until_metric};

type Mmo = Client<MmoView>;

/// Two pre-created MMO rooms and a third opened at runtime are three
/// separate worlds: each has its own four shards (a player in one never
/// appears in another, though all of them stand on the same waystone),
/// its own wire-id space, and input moves only its own world.
#[tokio::test]
async fn every_mmo_room_is_a_whole_sharded_world() {
    let mut cfg = Door::Tcp.config("mmo");
    cfg.room_count = 2;
    cfg.http_listen = "127.0.0.1:0".into();
    let handle =
        gsb_server::start_game_server(Box::new(MmoModule::with_realm(Realm::empty())), cfg)
            .await
            .expect("the MMO starts");
    let ops = handle.http_addr.expect("ops surface");

    let mut x: Mmo = Client::join(&Door::Tcp, handle.addr, "x", 1).await;
    let mut y: Mmo = Client::join(&Door::Tcp, handle.addr, "y", 2).await;
    let reply = http(ops, "POST /rooms/open?id=3&tick_hz=30 HTTP/1.1\r\n\r\n").await;
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    assert!(reply.contains("r3 running"), "{reply}");
    let mut z: Mmo = Client::join(&Door::Tcp, handle.addr, "z", 3).await;

    // Each world's shard 0 minted its first player: the same wire id in
    // three id spaces.
    assert_eq!(x.entity, y.entity);
    assert_eq!(y.entity, z.entity);
    let waystone = dm(-256.0, -256.0);
    eventually(
        &mut [&mut x, &mut y, &mut z],
        Duration::from_secs(5),
        "each player sees itself at waystone 0",
        |cs| {
            cs.iter()
                .all(|c| c.me().is_some_and(|r| ground(&r) == waystone))
        },
    )
    .await;
    x.move_to(-240.0, -256.0, 1).await;
    eventually(
        &mut [&mut x, &mut y, &mut z],
        Duration::from_secs(5),
        "X walks in its own world",
        |cs| cs[0].me().is_some_and(|r| ground(&r) == dm(-240.0, -256.0)),
    )
    .await;
    hold(
        &mut [&mut x, &mut y, &mut z],
        Duration::from_millis(500),
        |cs| {
            for c in cs {
                assert_eq!(c.view.players(), vec![c.entity], "one player per world");
            }
            assert_eq!(ground(&cs[1].me().unwrap()), waystone, "Y did not move");
            assert_eq!(ground(&cs[2].me().unwrap()), waystone, "Z did not move");
        },
    )
    .await;
    handle.stop().await;
}

/// A room opened at runtime gets the server's room-level keys exactly as
/// a pre-created one does (BACKLOG F8): with a 1 s `max_detach_hold_secs`
/// a character that dropped mid-fight in the runtime room is NOT held for
/// its 6 s combat window — the ceiling overrides the veto right at the 1 s
/// logout timer, as `mmo_logout::the_server_ceiling_bounds_the_combat_hold`
/// shows for a pre-created room.
#[tokio::test]
async fn a_runtime_room_gets_the_server_ceiling() {
    let camp = Pos3::new(-250.0, 0.0, -250.0);
    let realm = Realm::empty().with_spawn(MobSpawn::once(Kind::Mob, camp, 1, 1_000_000, 60_000));
    let mut cfg = Door::Tcp.config("mmo");
    cfg.http_listen = "127.0.0.1:0".into();
    cfg.max_detach_hold = Some(Duration::from_secs(1));
    cfg.raw = toml::from_str("[mmo]\nlogout_grace_secs = 1").expect("table parses");
    let handle = gsb_server::start_game_server(Box::new(MmoModule::with_realm(realm)), cfg)
        .await
        .expect("the MMO starts");
    let ops = handle.http_addr.expect("ops surface");
    let reply = http(ops, "POST /rooms/open?id=2 HTTP/1.1\r\n\r\n").await;
    assert!(reply.contains("r2 running"), "{reply}");

    let mut p: Mmo = Client::join(&Door::Tcp, handle.addr, "fighter", 2).await;
    let mut o: Mmo = Client::join(&Door::Tcp, handle.addr, "observer", 2).await;
    let ip = p.entity;
    eventually(
        &mut [&mut p, &mut o],
        Duration::from_secs(5),
        "the mob is up and the two see each other",
        |cs| {
            cs.iter()
                .all(|c| c.view.players().len() == 2 && !c.view.of_kind(mmo::Kind::Mob).is_empty())
        },
    )
    .await;
    let mob = p.view.of_kind(mmo::Kind::Mob)[0];
    p.attack(mob, 1).await;
    eventually(
        &mut [&mut p],
        Duration::from_secs(5),
        "the hit lands",
        |cs| cs[0].view.acks.last() == Some(&1),
    )
    .await;

    drop(p);
    let t0 = Instant::now();
    eventually(
        &mut [&mut o],
        Duration::from_secs(10),
        "the fighter logs out",
        |cs| cs[0].sees(ip).is_none(),
    )
    .await;
    let gone = t0.elapsed();
    assert!(
        gone >= Duration::from_millis(900),
        "the grace is not shortened: {gone:?}"
    );
    // The ceiling, not the end of the combat window (6 s after the hit),
    // ended the hold: the room counts a hold it forced over a standing
    // veto. Read from the room's own counter, not from "gone within
    // 3.5 s" — a starved server is slower to notice the drop and to
    // show the logout, without the ceiling being any later (BACKLOG F52).
    let forced = until_metric(
        ops,
        "gsb_room_detach_forced_total",
        2,
        Duration::from_secs(10),
        "a hold the ceiling forced, counted (none: the hold ended without \
         overriding the combat veto)",
        |n| n > 0.0,
    )
    .await;
    assert_eq!(
        forced, 1.0,
        "held past the 1 s ceiling (the combat window is 6 s): the ceiling \
         forced exactly this one hold"
    );
    handle.stop().await;
}
