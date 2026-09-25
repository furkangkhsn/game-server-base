//! The 3D MMO hosted by the REAL server (GAME-MODULE G2): the real
//! registry wires the four shard actors, routes joins through the MMO
//! module's router, and real TCP clients apply what they receive under
//! the kit's client rules. Realms are scripted: saved characters keyed
//! by the player's authenticated identity — here the local-auth path's
//! `Auth.name` (`mmo_home.rs` covers the ticket path and resume).

#![cfg(feature = "game-mmo")]

mod common;
mod hosted;

use std::time::Duration;

use gsb_core::shard::SHARD_SERIAL_RANGE;
use gsb_demo_mmo::{Pos3, Realm};
use gsb_server::games::mmo::MmoModule;
use hosted::mmo::{MmoView, dm, ground};
use hosted::{Client, Door, eventually, hold};

type Mmo = Client<MmoView>;

/// An MMO server over `realm` on an ephemeral TCP port.
async fn start(realm: Realm) -> gsb_server::ServerHandle {
    gsb_server::start_game_server(
        Box::new(MmoModule::with_realm(realm)),
        Door::Tcp.config("mmo"),
    )
    .await
    .expect("the MMO starts")
}

/// The shard that minted a wire id (range partitioning: shard `i` mints
/// from `[i * SHARD_SERIAL_RANGE, (i + 1) * SHARD_SERIAL_RANGE)`).
fn minted_by(id: u64) -> u64 {
    id / SHARD_SERIAL_RANGE
}

async fn join(addr: std::net::SocketAddr, name: &str) -> Mmo {
    Client::join(&Door::Tcp, addr, name, 1).await
}

/// A session with no saved character lands on the default waystone's
/// shard (0) at that waystone; a saved one on the shard owning its save,
/// at its save — the join was ROUTED there (the minting shard is in the
/// wire id), not spawned elsewhere and migrated.
#[tokio::test]
async fn joins_land_on_the_shard_of_their_character() {
    let realm = Realm::empty()
        .with_login("east", Pos3::new(200.0, 0.0, -200.0)) // shard 1
        .with_login("north", Pos3::new(-200.0, 0.0, 200.0)) // shard 2
        .with_login("far", Pos3::new(200.0, 0.0, 200.0)); // shard 3
    let handle = start(realm).await;
    let mut cs = Vec::new();
    for name in ["unsaved", "east", "north", "far"] {
        cs.push(join(handle.addr, name).await);
    }
    let expected = [
        (0, dm(-256.0, -256.0)),
        (1, dm(200.0, -200.0)),
        (2, dm(-200.0, 200.0)),
        (3, dm(200.0, 200.0)),
    ];
    let mut refs: Vec<&mut Mmo> = cs.iter_mut().collect();
    eventually(
        &mut refs,
        Duration::from_secs(5),
        "everyone sees itself",
        |cs| cs.iter().all(|c| c.me().is_some()),
    )
    .await;
    for (c, (shard, at)) in cs.iter().zip(expected) {
        assert_eq!(
            minted_by(c.entity),
            shard,
            "{} minted by shard {shard}",
            c.entity
        );
        assert_eq!(
            ground(&c.me().unwrap()),
            at,
            "spawned at its save / waystone"
        );
    }
    handle.stop().await;
}

/// A player walks across the x = 0 seam (shard 0 → shard 1). It never
/// loses itself (checked after every applied frame), keeps its wire id,
/// and is seen every frame by a neighbour on each side of the seam —
/// after the crossing the shard-0 neighbour sees it across the border.
#[tokio::test]
async fn a_walker_across_a_seam_keeps_itself_and_is_seen_across_it() {
    let realm = Realm::empty()
        .with_login("walker", Pos3::new(-20.0, 0.0, -100.0)) // A, shard 0
        .with_login("east", Pos3::new(40.0, 0.0, -100.0)) // B, shard 1
        .with_login("west", Pos3::new(-60.0, 0.0, -100.0)); // C, shard 0
    let handle = start(realm).await;
    let mut a = join(handle.addr, "walker").await;
    let mut b = join(handle.addr, "east").await;
    let mut c = join(handle.addr, "west").await;
    let id = a.entity;
    assert_eq!(minted_by(id), 0);
    for cl in [&mut a, &mut b, &mut c] {
        cl.view.watch = Some(id);
    }
    eventually(
        &mut [&mut a, &mut b, &mut c],
        Duration::from_secs(5),
        "all three see each other across the seam",
        |cs| cs.iter().all(|c| c.view.players().len() == 3),
    )
    .await;

    a.move_to(30.0, -100.0, 1).await;
    eventually(
        &mut [&mut a, &mut b, &mut c],
        Duration::from_secs(15),
        "the walker arrives on the far side, seen by both neighbours",
        |cs| {
            let there = |c: &Mmo| c.sees(id).map(|r| ground(&r)) == Some(dm(30.0, -100.0));
            cs.iter().all(|c| there(c))
        },
    )
    .await;
    hold(
        &mut [&mut a, &mut b, &mut c],
        Duration::from_millis(1500),
        |_| {},
    )
    .await;
    for (who, cl) in [("walker", &a), ("east", &b), ("west", &c)] {
        assert_eq!(cl.view.lost, 0, "{who} lost the walker on some frame");
        assert!(cl.view.applied > 60, "{who}: a live stream");
    }
    assert_eq!(a.view.acks.last(), Some(&1));
    handle.stop().await;
}

/// `Travel` teleports a player into the diagonal shard's region: it lands
/// at the waystone with the same wire id, a player waiting there sees it
/// arrive, and its next input is served by the destination shard.
#[tokio::test]
async fn a_travel_lands_on_the_destination_shard() {
    let realm = Realm::empty().with_login("waiting", Pos3::new(250.0, 0.0, 250.0)); // D, shard 3
    let handle = start(realm).await;
    let mut a = join(handle.addr, "traveller").await; // waystone 0, shard 0
    let mut d = join(handle.addr, "waiting").await;
    let id = a.entity;
    eventually(
        &mut [&mut a, &mut d],
        Duration::from_secs(5),
        "spawned",
        |cs| cs[0].me().is_some() && cs[1].me().is_some(),
    )
    .await;
    assert!(d.sees(id).is_none(), "shard 0's waystone is far from D");

    a.travel(3, 1).await;
    eventually(
        &mut [&mut a, &mut d],
        Duration::from_secs(5),
        "the traveller lands at waystone 3, seen by D",
        |cs| {
            let at = dm(256.0, 256.0);
            cs[0].me().map(|r| ground(&r)) == Some(at)
                && cs[1].sees(id).map(|r| ground(&r)) == Some(at)
        },
    )
    .await;

    // Its input now reaches the shard that holds it, which acks it (the
    // `Travel`'s own ack, carried across the hand-off, is locked in
    // `mmo_findings`).
    a.move_to(270.0, 256.0, 2).await;
    eventually(
        &mut [&mut a, &mut d],
        Duration::from_secs(5),
        "D sees the traveller walk on shard 3",
        |cs| {
            cs[1].sees(id).map(|r| ground(&r)) == Some(dm(270.0, 256.0))
                && cs[0].view.acks.last() == Some(&2)
        },
    )
    .await;
    assert_eq!(minted_by(id), 0, "the wire id travels with the character");
    handle.stop().await;
}
