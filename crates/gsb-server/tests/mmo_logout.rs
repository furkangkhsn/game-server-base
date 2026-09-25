//! The MMO's disconnect policy under the REAL server (GAME-MODULE §6
//! decision 5): its logout timer read from `[mmo]`, the combat veto
//! holding a character that dropped mid-fight, and a resume onto a
//! character parked on another shard than the one it joined.

#![cfg(feature = "game-mmo")]

mod common;
mod hosted;

use std::time::{Duration, Instant};

use gsb_core::id::RoomId;
use gsb_core::registry::RoomStatus;
use gsb_demo_mmo::components::Kind;
use gsb_demo_mmo::mmo;
use gsb_demo_mmo::{MobSpawn, Pos3, Realm};
use gsb_server::games::mmo::MmoModule;
use hosted::mmo::{MmoView, dm, ground};
use hosted::{Client, Door, config_file, eventually, hold};

type Mmo = Client<MmoView>;

async fn join(addr: std::net::SocketAddr, name: &str) -> Mmo {
    Client::join(&Door::Tcp, addr, name, 1).await
}

/// An MMO server over `realm` with the `[mmo]` table `table`.
async fn start(realm: Realm, table: &str) -> gsb_server::ServerHandle {
    start_with(realm, table, |_| {}).await
}

/// [`start`] with the server config adjusted by `tweak`.
async fn start_with(
    realm: Realm,
    table: &str,
    tweak: impl FnOnce(&mut gsb_server::Config),
) -> gsb_server::ServerHandle {
    let mut cfg = Door::Tcp.config("mmo");
    tweak(&mut cfg);
    cfg.raw = toml::from_str(table).expect("table parses");
    gsb_server::start_game_server(Box::new(MmoModule::with_realm(realm)), cfg)
        .await
        .expect("the MMO starts")
}

/// The registry's member count of room 1.
async fn members(handle: &gsb_server::ServerHandle) -> u32 {
    match handle.room_status(RoomId(1)).await.expect("registry") {
        RoomStatus::Running { members } => members,
        other => panic!("room 1 is {other:?}"),
    }
}

/// Two characters drop at the same moment next to an observer; one of
/// them had just landed a hit. With a 1 s logout timer the peaceful one
/// logs out after the grace, the fighter is HELD while in combat (the
/// MMO's veto, 6 s after its hit) and logs out once it cools down — and
/// the registry releases both rows.
#[tokio::test]
async fn a_fighter_is_held_past_the_grace_and_a_peaceful_player_logs_out() {
    let camp = Pos3::new(-250.0, 0.0, -250.0); // 8.5 m from waystone 0
    let realm = Realm::empty().with_spawn(MobSpawn::once(Kind::Mob, camp, 1, 1_000_000, 60_000));
    let handle = start(realm, "[mmo]\nlogout_grace_secs = 1").await;
    let mut p = join(handle.addr, "fighter").await;
    let mut q = join(handle.addr, "peaceful").await;
    let mut o = join(handle.addr, "observer").await;
    let (ip, iq) = (p.entity, q.entity);
    eventually(
        &mut [&mut p, &mut q, &mut o],
        Duration::from_secs(5),
        "the mob is up and everyone at waystone 0 sees everyone",
        |cs| {
            cs.iter()
                .all(|c| c.view.players().len() == 3 && !c.view.of_kind(mmo::Kind::Mob).is_empty())
        },
    )
    .await;
    let mob = p.view.of_kind(mmo::Kind::Mob)[0];
    p.attack(mob, 1).await;
    eventually(
        &mut [&mut p, &mut o],
        Duration::from_secs(5),
        "the hit lands",
        |cs| cs[0].view.acks.last() == Some(&1) && cs[1].sees(mob).is_some_and(|m| m.hp < 60_000),
    )
    .await;
    assert_eq!(members(&handle).await, 3);

    drop(p);
    drop(q);
    let t0 = Instant::now();
    eventually(
        &mut [&mut o],
        Duration::from_secs(5),
        "the peaceful one logs out",
        |cs| cs[0].sees(iq).is_none(),
    )
    .await;
    let logged_out = t0.elapsed();
    assert!(
        logged_out >= Duration::from_millis(900),
        "not before the grace: {logged_out:?}"
    );
    // The fighter stays in the world well past the grace (in combat).
    let until = Duration::from_secs(3).saturating_sub(t0.elapsed());
    hold(&mut [&mut o], until, |cs| {
        assert!(cs[0].sees(ip).is_some(), "the fighter logged out in combat");
    })
    .await;
    eventually(
        &mut [&mut o],
        Duration::from_secs(10),
        "the fighter logs out",
        |cs| cs[0].sees(ip).is_none(),
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while members(&handle).await != 1 {
        assert!(
            Instant::now() < deadline,
            "the registry kept a logged-out row"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    handle.stop().await;
}

/// The server's `max_detach_hold_secs` reaches the MMO's shards: with a
/// 1 s ceiling, a character that dropped mid-fight is NOT held for its
/// combat window (6 s after its hit) — the veto is overridden where the
/// ceiling falls, right at the 1 s logout timer (the ceiling never
/// shortens the grace), and the registry releases the row.
#[tokio::test]
async fn the_server_ceiling_bounds_the_combat_hold() {
    let camp = Pos3::new(-250.0, 0.0, -250.0);
    let realm = Realm::empty().with_spawn(MobSpawn::once(Kind::Mob, camp, 1, 1_000_000, 60_000));
    let ceiling: gsb_server::Config =
        toml::from_str("max_detach_hold_secs = 1").expect("the key parses");
    let handle = start_with(realm, "[mmo]\nlogout_grace_secs = 1", |cfg| {
        cfg.max_detach_hold = ceiling.max_detach_hold;
    })
    .await;
    let mut p = join(handle.addr, "fighter").await;
    let mut o = join(handle.addr, "observer").await;
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
    assert!(
        gone < Duration::from_millis(3_500),
        "held past the 1 s ceiling (the combat window is 6 s): {gone:?}"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while members(&handle).await != 1 {
        assert!(Instant::now() < deadline, "the registry kept the row");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    handle.stop().await;
}

/// `game = "mmo"` from a config FILE through the catalog, with the
/// `[mmo]` table's 1 s logout timer: a dropped character logs out within
/// seconds (the MMO's own default is 20 s).
#[tokio::test]
async fn the_mmo_table_sets_the_logout_timer() {
    let cfg = config_file(
        "mmo-logout",
        "game = \"mmo\"\nbind = \"127.0.0.1:0\"\n[mmo]\nlogout_grace_secs = 1\n",
    );
    let handle = gsb_server::start_server(cfg).await.expect("the MMO starts");
    let q = join(handle.addr, "leaver").await;
    let mut o = join(handle.addr, "stayer").await;
    let iq = q.entity;
    eventually(&mut [&mut o], Duration::from_secs(5), "O sees Q", |cs| {
        cs[0].sees(iq).is_some()
    })
    .await;
    drop(q);
    eventually(
        &mut [&mut o],
        Duration::from_secs(5),
        "Q logs out after 1 s",
        |cs| cs[0].sees(iq).is_none(),
    )
    .await;
    handle.stop().await;
}

/// A character joins on shard 0, walks into shard 1, drops there and is
/// parked there; its next session (same name, a new connection the
/// router would send to shard 0) resumes it on shard 1 — the registry's
/// resume broadcast finds it — with the same wire id, and its input works.
#[tokio::test]
async fn a_character_parked_on_another_shard_resumes_there() {
    let realm = Realm::empty()
        .with_login("wanderer", Pos3::new(-20.0, 0.0, -100.0)) // A, shard 0
        .with_login("witness", Pos3::new(40.0, 0.0, -100.0)); // B, shard 1
    let handle = start(realm, "").await;
    let mut a = join(handle.addr, "wanderer").await;
    let mut b = join(handle.addr, "witness").await;
    let id = a.entity;
    a.move_to(30.0, -100.0, 1).await;
    eventually(
        &mut [&mut a, &mut b],
        Duration::from_secs(15),
        "A crossed",
        |cs| cs[1].sees(id).map(|r| ground(&r)) == Some(dm(30.0, -100.0)),
    )
    .await;

    drop(a);
    hold(&mut [&mut b], Duration::from_secs(1), |cs| {
        assert!(cs[0].sees(id).is_some(), "the parked character stays");
    })
    .await;
    let mut a2 = join(handle.addr, "wanderer").await;
    assert_eq!(a2.entity, id, "the resume found the character on shard 1");
    a2.move_to(60.0, -100.0, 1).await;
    eventually(
        &mut [&mut a2, &mut b],
        Duration::from_secs(10),
        "the resumed character walks on",
        |cs| {
            cs[0].me().map(|r| ground(&r)) == Some(dm(60.0, -100.0))
                && cs[1].sees(id).map(|r| ground(&r)) == Some(dm(60.0, -100.0))
        },
    )
    .await;
    handle.stop().await;
}
