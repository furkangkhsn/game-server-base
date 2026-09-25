//! The record run (`RecordCodec::RUN`, KIT-ARCHITECTURE §10 "A31")
//! against the `entities` framing, room kind by room kind: the same
//! seeded session played twice — once over the fixture's `entities`
//! codec, once over its record-run twin — every frame taken apart and
//! checked to be exactly its framing's layout, and the two clients of
//! every player holding the same frames and the same view after every
//! tick. The single-room kinds run on the REAL core (a registry, two
//! room actors: `rig`); the sharded kinds on four shard logics per room
//! stepped in the core's phase order (`shards` — why not the actors:
//! see there).
//!
//! Every session's content is also PINNED (a digest of every frame the
//! `entities` side received, taken before the per-record send rate
//! existed — A10): a game on the default rate ships exactly what it
//! shipped before, room kind by room kind.

use bevy_ecs::prelude::World;
use gsb_core::registry::RoomFactory;

use crate::sharded::KitMig;
use crate::testing::{FixCodec, FixMig, PackedCodec};

use compare::Stats;
use rig::Rig;
use rooms::*;
use script::Script;
use shards::{Key, Mig, Shards};

mod compare;
mod game;
mod imports;
mod layout;
mod rig;
mod rooms;
mod script;
mod shards;

/// The AOI cell edge (wire units) — the rooms' and the decoders'.
const CELL: f32 = 20.0;
/// Vision radius of the team rooms.
const RADIUS: f32 = 25.0;
/// The sharded map spans `[-HALF, HALF]²` on a 2×2 grid.
const HALF: f32 = 100.0;
const SHARDS: usize = 4;
/// Steps of every run (20 s at 30 Hz).
const TICKS: u64 = 600;

/// Play the seeded session on the actor twin (the single-room kinds).
async fn play<G, St, Sp>(factory: RoomFactory<World, G, St, Sp>, seed: u64, pin: u64) -> Rig
where
    G: Eq + std::hash::Hash + Clone + std::fmt::Debug + Send + 'static,
    St: std::fmt::Debug + Send + 'static,
    Sp: std::fmt::Debug + Clone + PartialEq + Send + 'static,
{
    let mut rig = Rig::new(factory, 1).await;
    let mut script = Script::new(seed, 20);
    for _ in 0..TICKS {
        rig.play(&script.tick()).await;
    }
    assert_eq!(rig.counters().errors, 0);
    assert!(rig.players() >= 5, "a crowd played");
    exercised(&rig.stats, pin);
    rig
}

/// Play the seeded session on the stepped sharded twin.
fn play_shards<G: Key, St: Mig>(
    build: fn(bool, usize) -> Shard<G, St>,
    seed: u64,
    pin: u64,
) -> Shards<G, St> {
    let mut twin = Shards::new(build, SHARDS);
    let mut script = Script::new(seed, 20);
    for _ in 0..TICKS {
        twin.play(&script.tick());
    }
    assert_eq!(twin.counters().errors, 0);
    assert!(twin.spread().len() == SHARDS, "every shard hosted players");
    exercised(&twin.stats, pin);
    twin
}

/// Both framings carried the same content, and the run was exercised
/// (multi-byte run lengths included).
///
/// `pin` is the session's content digest as the kit produced it before
/// the per-record send rate existed (A10, measured at `a4c8d3e`): a
/// game that keeps the default rate — every codec here — ships exactly
/// the same content, frame by frame.
fn exercised((ent, run): &(Stats, Stats), pin: u64) {
    assert_eq!(
        ent.digest, pin,
        "the session's content changed: {:#018x}",
        ent.digest
    );
    assert_eq!(
        (ent.fulls, ent.deltas, ent.removed, ent.exits, ent.records),
        (run.fulls, run.deltas, run.removed, run.exits, run.records),
        "both framings carried the same content"
    );
    assert_eq!(ent.long_runs, 0, "no run in the entities framing");
    assert!(run.records > 2_000 && run.long_runs > 0, "{run:?}");
}

#[tokio::test(start_paused = true)]
async fn the_open_room_runs_alike() {
    let build = |run| {
        if run {
            open::<PackedCodec>()
        } else {
            open::<FixCodec>()
        }
    };
    play(single(build), 0xA31_0001, 0xa3c3_e651_99d6_a8e2).await;
}

#[tokio::test(start_paused = true)]
async fn the_aoi_room_runs_alike() {
    let build = |run| {
        if run {
            aoi::<PackedCodec>()
        } else {
            aoi::<FixCodec>()
        }
    };
    let rig = play(single(build), 0xA31_0002, 0x5986_a521_5957_9f93).await;
    let (_, run) = &rig.stats;
    assert!(
        run.deltas > 100 && run.removed > 0 && run.exits > 0,
        "{run:?}"
    );
    assert!(rig.counters().private_fulls > 0);
}

#[tokio::test(start_paused = true)]
async fn the_team_room_runs_alike_in_both_modes() {
    let full = |run| {
        if run {
            team::<PackedCodec>(false)
        } else {
            team::<FixCodec>(false)
        }
    };
    play(single(full), 0xA31_0003, 0x988b_3bc9_5d44_b2e0).await;
    let delta = |run| {
        if run {
            team::<PackedCodec>(true)
        } else {
            team::<FixCodec>(true)
        }
    };
    let rig = play(single(delta), 0xA31_0004, 0xac73_56d7_8629_d4a5).await;
    let (_, run) = &rig.stats;
    assert!(run.deltas > 100 && run.removed > 0, "{run:?}");
    assert!(rig.counters().private_fulls > 0);
}

#[tokio::test(start_paused = true)]
async fn the_pvs_room_runs_alike() {
    let build = |run| {
        if run {
            pvs::<PackedCodec>()
        } else {
            pvs::<FixCodec>()
        }
    };
    play(single(build), 0xA31_0005, 0xbeb8_bc71_a161_5392).await;
}

#[test]
fn the_sharded_room_runs_alike() {
    let build = |run, i| -> Shard<(), KitMig<FixMig>> {
        if run {
            Box::new(plain::<PackedCodec>(i))
        } else {
            Box::new(plain::<FixCodec>(i))
        }
    };
    play_shards(build, 0xA31_0006, 0xdf1f_13ed_b36a_34e6);
}

#[test]
fn the_sharded_spatial_room_runs_alike() {
    let build = |run, i| {
        if run {
            spatial::<PackedCodec>(i)
        } else {
            spatial::<FixCodec>(i)
        }
    };
    let twin = play_shards(build, 0xA31_0007, 0x0a6d_3d8a_867e_11aa);
    let (_, run) = &twin.stats;
    assert!(
        run.deltas > 100 && run.removed > 0 && run.exits > 0,
        "{run:?}"
    );
    assert!(twin.counters().private_fulls > 0);
}

#[test]
fn the_sharded_team_room_runs_alike_in_both_modes() {
    let full = |run, i| {
        if run {
            team_shard::<PackedCodec>(i, false)
        } else {
            team_shard::<FixCodec>(i, false)
        }
    };
    play_shards(full, 0xA31_0008, 0x7ae7_32bd_b87a_4f32);
    let delta = |run, i| {
        if run {
            team_shard::<PackedCodec>(i, true)
        } else {
            team_shard::<FixCodec>(i, true)
        }
    };
    let twin = play_shards(delta, 0xA31_0009, 0x5f8a_5195_6263_99ab);
    let (_, run) = &twin.stats;
    assert!(run.deltas > 100 && run.removed > 0, "{run:?}");
    assert!(twin.counters().private_fulls > 0);
}
