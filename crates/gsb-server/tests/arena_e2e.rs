//! The 3D arena hosted by the REAL server (GAME-MODULE G2): `game =
//! "arena"` through the catalog, real sockets (plain TCP and TLS), three
//! teams. Every snapshot a client receives is checked against the team
//! fog rule across the network — an enemy is in a team's package only
//! within 15 m (3D) of one of its units — and the height case (a unit
//! straight above another, beyond the radius) is hidden although the
//! two share a spot on the floor.

#![cfg(feature = "game-arena")]

mod common;
mod hosted;

use std::sync::Arc;
use std::time::Duration;

use gsb_demo_arena::VISION_RADIUS;
use gsb_demo_arena::codec::{Cm3, to_cm};
use hosted::arena::{ArenaView, dist_cm};
use hosted::{Client, Door, eventually, hold};

type Arena = Client<ArenaView>;

/// The team fog rule, from the network side: every unit in a client's
/// view is its own (each team here has one unit — its client's) or an
/// enemy within the vision radius of it. Quantization: centimetres.
fn fog_holds(clients: &[&mut Arena]) {
    let radius = f64::from(to_cm(VISION_RADIUS)) + 2.0;
    for c in clients {
        let Some(me) = c.me() else {
            continue; // before the first snapshot
        };
        for (&id, &at) in &c.view.units {
            if id != c.entity {
                let d = dist_cm(me, at);
                assert!(
                    d <= radius,
                    "unit {id} in {}'s team view at {d} cm (> {radius})",
                    c.entity
                );
            }
        }
    }
}

fn set(ids: &[u64]) -> Vec<u64> {
    let mut v = ids.to_vec();
    v.sort_unstable();
    v
}

async fn three_teams_through(door: Door) {
    let handle = gsb_server::start_server(door.config("arena"))
        .await
        .expect("the arena starts");
    let addr = handle.addr;
    // Joins 1, 2, 3 → teams 0, 1, 2 (the arena's round-robin).
    let mut a: Arena = Client::join(&door, addr, "arena-a", 1).await;
    let mut b: Arena = Client::join(&door, addr, "arena-b", 1).await;
    let mut c: Arena = Client::join(&door, addr, "arena-c", 1).await;
    let (ia, ib, ic) = (a.entity, b.entity, c.entity);

    // Fresh spawns sit at their bases, 43 m apart: each team sees itself.
    eventually(
        &mut [&mut a, &mut b, &mut c],
        Duration::from_secs(5),
        "every team sees its base",
        |cs| cs.iter().all(|c| c.view.sees() == vec![c.entity]),
    )
    .await;

    // A on the floor at the centre, B 20 m straight above it (beyond the
    // radius although planar distance 0), C 10 m up (within reach of
    // both). Numbered inputs: each is acked.
    a.move_to(0.0, 0.0, 0.0, 1).await;
    b.move_to(0.0, 20.0, 0.0, 1).await;
    c.move_to(0.0, 10.0, 0.0, 1).await;
    let at = |y: f32| Cm3 {
        x: 0,
        y: to_cm(y),
        z: 0,
    };
    let mut all = [&mut a, &mut b, &mut c];
    let settled = |cs: &[&mut Arena]| {
        fog_holds(cs);
        cs[0].me() == Some(at(0.0)) && cs[1].me() == Some(at(20.0)) && cs[2].me() == Some(at(10.0))
    };
    eventually(&mut all, Duration::from_secs(10), "units settle", settled).await;
    // Settled: the fog, height included, over a second of snapshots.
    hold(&mut all, Duration::from_secs(1), |cs| {
        fog_holds(cs);
        assert_eq!(
            cs[0].view.sees(),
            set(&[ia, ic]),
            "team 0: B is 20 m above A"
        );
        assert_eq!(
            cs[1].view.sees(),
            set(&[ib, ic]),
            "team 1: A is 20 m below B"
        );
        assert_eq!(
            cs[2].view.sees(),
            set(&[ia, ib, ic]),
            "team 2: both in 10 m"
        );
    })
    .await;
    for cl in all.iter() {
        assert_eq!(cl.view.acks.last(), Some(&1), "the move was acked");
    }

    // B drops to 5 m above A: it enters A's view — and C climbs to 28 m,
    // leaving everyone's. A's two more numbered moves are acked up to 3.
    b.move_to(0.0, 5.0, 0.0, 2).await;
    c.move_to(0.0, 28.0, 0.0, 2).await;
    a.move_to(1.0, 0.0, 0.0, 2).await;
    a.move_to(0.0, 0.0, 0.0, 3).await;
    eventually(
        &mut [&mut a, &mut b, &mut c],
        Duration::from_secs(10),
        "B comes into A's view, C climbs out of it",
        |cs| {
            fog_holds(cs);
            cs[0].view.sees() == set(&[ia, ib])
                && cs[1].view.sees() == set(&[ia, ib])
                && cs[2].view.sees() == vec![ic]
                && cs[0].view.acks.last() == Some(&3)
        },
    )
    .await;
    assert!(a.view.snapshots > 30, "a steady snapshot stream");
    handle.stop().await;
}

#[tokio::test]
async fn three_teams_see_their_fog_over_tcp() {
    three_teams_through(Door::Tcp).await;
}

#[tokio::test]
async fn three_teams_see_their_fog_over_tls() {
    three_teams_through(Door::Tls(Arc::new(common::mint_tls_pki("arena")))).await;
}
