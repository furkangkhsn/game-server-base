//! Behaviour the MMO showed for the first time under the REAL server and
//! real clients (GAME-MODULE §5 "G2 sonucu", findings K1/K2): the kit's
//! per-session input sequence state (`InputSeq`: the high-water mark and
//! the pending ack) did not travel with a migrating player. It does now
//! — `KitMig` carries it (`ShardInputRecord`) — and these tests, which
//! locked the old behaviour, are turned around to lock the fix end to
//! end (the kit's own tests: `gsb-kit`'s `sharded::tests::input_carry`).

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
    let realm = Realm::empty().with_login("witness", Pos3::new(250.0, 0.0, 250.0));
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

/// K1: the input processed in the tick its player migrates is acked. The
/// source shard hands the session off in MIGRATE (phase 4), before
/// BROADCAST (phase 6) would emit the ack; the session state rides the
/// migration (through the intermediate shard of the diagonal hop) and
/// the destination sends the ack the source owed — once, before any
/// later input's.
#[tokio::test]
async fn k1_the_input_that_moves_a_player_to_another_shard_is_acked() {
    let (handle, mut a, mut d) = traveller_and_witness().await;
    land_at_waystone_3(&mut a, &mut d, 1).await;
    eventually(
        &mut [&mut a, &mut d],
        Duration::from_secs(5),
        "the Travel's ack",
        |cs| cs[0].view.acks == [1],
    )
    .await;
    a.move_to(260.0, 256.0, 2).await;
    eventually(
        &mut [&mut a, &mut d],
        Duration::from_secs(5),
        "ack 2",
        |cs| cs[0].view.acks == [1, 2],
    )
    .await;
    handle.stop().await;
}

/// K2: the kit's sequence rule (a numbered input at or below the
/// session's mark is dropped) holds within a shard AND across a
/// migration: on the destination, an input older than the `Travel` is a
/// late datagram and is dropped (a reordered or duplicated datagram is
/// never replayed); a newer one is processed.
#[tokio::test]
async fn k2_the_input_sequence_rule_holds_across_a_migration() {
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

    // Travel as seq 3; on shard 3 a seq-2 input (older) is dropped too.
    land_at_waystone_3(&mut a, &mut d, 3).await;
    a.move_to(262.0, 256.0, 2).await;
    hold(&mut [&mut a, &mut d], Duration::from_millis(1000), |cs| {
        let at = cs[1].sees(id).map(|r| ground(&r));
        assert_eq!(at, Some(dm(256.0, 256.0)), "the late seq 2 was dropped");
    })
    .await;
    // A newer input is processed there (and acked: the mark is 4).
    a.move_to(262.0, 256.0, 4).await;
    eventually(
        &mut [&mut a, &mut d],
        Duration::from_secs(5),
        "seq 4 walks the traveller on shard 3",
        |cs| {
            cs[1].sees(id).map(|r| ground(&r)) == Some(dm(262.0, 256.0))
                && cs[0].view.acks.last() == Some(&4)
        },
    )
    .await;
    handle.stop().await;
}
