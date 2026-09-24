//! Behaviour the MMO shows for the first time under the REAL server and
//! real clients (GAME-MODULE §5 "G2 sonucu", findings K1/K2): the kit's
//! per-session input sequence state (`InputSeq`: the high-water mark and
//! the pending ack) does NOT travel with a migrating player (`KitMig`
//! carries the game state and the park record only).
//!
//! These tests lock TODAY's behaviour on purpose: the fix belongs in
//! `gsb-kit` (out of this round's scope), and when it lands they break
//! and are turned around — as the MMO crate's own `findings` tests were.

#![cfg(feature = "game-mmo")]

mod common;
mod hosted;

use std::time::Duration;

use gsb_demo_mmo::{Pos3, Realm};
use gsb_server::games::mmo::MmoModule;
use hosted::mmo::{MmoView, dm, ground};
use hosted::{Client, Door, eventually, hold};

type Mmo = Client<MmoView>;

/// A traveller (waystone 0, shard 0) and a player waiting at waystone 3
/// (shard 3), both spawned.
async fn traveller_and_witness() -> (gsb_server::ServerHandle, Mmo, Mmo) {
    let realm = Realm::empty().with_login(2, Pos3::new(250.0, 0.0, 250.0));
    let handle = gsb_server::start_game_server(
        Box::new(MmoModule::with_realm(realm)),
        Door::Tcp.config("mmo"),
    )
    .await
    .expect("the MMO starts");
    let mut a: Mmo = Client::join(&Door::Tcp, handle.addr, "traveller", 1).await;
    let mut d: Mmo = Client::join(&Door::Tcp, handle.addr, "witness", 1).await;
    eventually(
        &mut [&mut a, &mut d],
        Duration::from_secs(5),
        "spawned",
        |cs| cs[0].me().is_some() && cs[1].me().is_some(),
    )
    .await;
    (handle, a, d)
}

async fn land_at_waystone_3(a: &mut Mmo, d: &mut Mmo, seq: u64) {
    let id = a.entity;
    a.travel(3, seq).await;
    eventually(&mut [a, d], Duration::from_secs(5), "landed", |cs| {
        cs[1].sees(id).map(|r| ground(&r)) == Some(dm(256.0, 256.0))
    })
    .await;
}

/// K1: the input processed in the tick its player migrates is never
/// acknowledged. The source shard hands the session off in MIGRATE
/// (phase 4), before BROADCAST (phase 6) would emit the ack; the
/// destination starts a fresh mark, so the client hears about that input
/// only when a LATER one is acked there (the ack is a high-water mark).
#[tokio::test]
async fn k1_the_input_that_moves_a_player_to_another_shard_is_not_acked() {
    let (handle, mut a, mut d) = traveller_and_witness().await;
    land_at_waystone_3(&mut a, &mut d, 1).await;
    hold(&mut [&mut a, &mut d], Duration::from_millis(1500), |cs| {
        assert!(
            cs[0].view.acks.is_empty(),
            "K1 fixed? the Travel was acked: {:?} — turn this test around",
            cs[0].view.acks
        );
    })
    .await;
    a.move_to(260.0, 256.0, 2).await;
    eventually(
        &mut [&mut a, &mut d],
        Duration::from_secs(5),
        "ack 2",
        |cs| cs[0].view.acks == [2],
    )
    .await;
    handle.stop().await;
}

/// K2: the kit's sequence rule (a numbered input at or below the
/// session's mark is dropped) holds within a shard but restarts at the
/// destination of a migration: an input older than the `Travel` is
/// processed after it — a reordered or duplicated datagram would replay.
#[tokio::test]
async fn k2_the_input_sequence_rule_restarts_after_a_migration() {
    let (handle, mut a, mut d) = traveller_and_witness().await;
    let id = a.entity;
    // Within shard 0 the rule holds: seq 2 walks, a late seq 1 is dropped.
    a.move_to(-250.0, -256.0, 2).await;
    a.move_to(-200.0, -256.0, 1).await;
    eventually(
        &mut [&mut a],
        Duration::from_secs(5),
        "seq 2 arrives",
        |cs| cs[0].me().map(|r| ground(&r)) == Some(dm(-250.0, -256.0)),
    )
    .await;
    hold(&mut [&mut a], Duration::from_millis(700), |cs| {
        let at = cs[0].me().map(|r| ground(&r));
        assert_eq!(at, Some(dm(-250.0, -256.0)), "the late seq 1 was dropped");
    })
    .await;

    // Travel as seq 3; on shard 3 a seq-2 input (older) is admitted.
    land_at_waystone_3(&mut a, &mut d, 3).await;
    a.move_to(262.0, 256.0, 2).await;
    eventually(
        &mut [&mut a, &mut d],
        Duration::from_secs(5),
        "K2: the stale seq 2 moved the player on shard 3 (fixed? turn this test around)",
        |cs| cs[1].sees(id).map(|r| ground(&r)) == Some(dm(262.0, 256.0)),
    )
    .await;
    handle.stop().await;
}
