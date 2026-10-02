//! The full-snapshot rooms' seat of the block: a room that opted in
//! (`with_snapshot_budget`) answers the core's gate by the block's
//! credit and reports its counter; a room that did not ships every frame
//! and reports nothing of its own — open, PVS and plain sharded alike.

use std::time::Duration;

use bevy_ecs::prelude::World;
use gsb_core::id::{PlayerId, RoomId};
use gsb_core::metrics::LogicCounters;
use gsb_core::room::{GameLogic, TickCtx};

use super::SnapshotBudget;
use crate::pvs::SectorRoom;
use crate::room::OpenRoom;
use crate::sharded::ShardedRoom;
use crate::space::{ConvexSectors2, GridPartition2, Sector};
use crate::testing::{Fixture, Position};

fn ctx(tick: u64) -> TickCtx<'static> {
    TickCtx {
        room: RoomId(1),
        tick,
        dt: Duration::from_secs(1) / 30,
        idle: Default::default(),
        kicks: Default::default(),
        paths: Default::default(),
    }
}

/// Ticks 1..=n of a 3 000 B frame at `budget`: what the room shipped.
fn pattern<R: GameLogic<World>>(
    room: &mut R,
    group: &R::GroupKey,
    budget: usize,
    n: u64,
) -> Vec<bool> {
    let mut world = World::new();
    (1..=n)
        .map(|t| room.ship_snapshot(&mut world, &ctx(t), PlayerId(1), group, 3_000, budget))
        .collect()
}

fn own_counters<R: GameLogic<World>>(room: &R) -> Option<u64> {
    let mut out = LogicCounters::new();
    room.logic_counters(&World::new(), &mut out);
    out.get("snapshot_budget_forced")
}

/// The opted-in room thins (every third frame at a third of the frame,
/// one forced frame in sixteen at zero) and counts; the default room
/// ships everything and counts nothing.
fn check<R: GameLogic<World>>(name: &str, mut plain: R, mut opted: R, group: R::GroupKey) {
    assert!(
        pattern(&mut plain, &group, 0, 20).iter().all(|s| *s),
        "{name}: a room that did not opt in ships every frame"
    );
    assert_eq!(own_counters(&plain), None, "{name}: and reports nothing");
    let want: Vec<bool> = (1..=6).map(|t| t % 3 == 0).collect();
    assert_eq!(pattern(&mut opted, &group, 1_000, 6), want, "{name}");
    assert_eq!(
        own_counters(&opted),
        Some(0),
        "{name}: its counter, from zero"
    );
    let mut world = World::new();
    let shipped: Vec<u64> = (100..132)
        .filter(|t| opted.ship_snapshot(&mut world, &ctx(*t), PlayerId(2), &group, 3_000, 0))
        .collect();
    assert_eq!(shipped, vec![115, 131], "{name}: one in sixteen at zero");
    assert_eq!(own_counters(&opted), Some(2), "{name}: each one counted");
}

#[test]
fn the_full_snapshot_rooms_thin_only_when_opted_in() {
    check(
        "open",
        OpenRoom::with_game(Fixture::default()),
        OpenRoom::with_game(Fixture::default()).with_snapshot_budget(SnapshotBudget::new()),
        (),
    );
    let map = || {
        ConvexSectors2::<Position>::new(
            vec![vec![
                (-50.0, -50.0),
                (50.0, -50.0),
                (50.0, 50.0),
                (-50.0, 50.0),
            ]],
            vec![vec![Sector(0)]],
        )
    };
    check(
        "pvs",
        SectorRoom::with_game(Fixture::default(), map()),
        SectorRoom::with_game(Fixture::default(), map())
            .with_snapshot_budget(SnapshotBudget::new()),
        Sector(0),
    );
    let shard = || {
        ShardedRoom::with_game(
            Fixture::default(),
            GridPartition2::<Position>::new(1, 50.0),
            0,
        )
    };
    check(
        "sharded",
        shard(),
        shard().with_snapshot_budget(SnapshotBudget::new()),
        (),
    );
}
