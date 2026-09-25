//! The owner paces what it exports (A10): an importing shard cannot
//! compute the class of an encoded body, so in delta mode the shard that
//! owns a record advances its export body only on the record's due
//! steps; the full mode exports every change, as before.

use std::time::Duration;

use bevy_ecs::prelude::World;
use gsb_core::id::{ConnectionId, RoomId};
use gsb_core::room::{GameLogic, TickCtx};
use gsb_core::shard::{ShardLogic, TeamImports};

use super::super::RADIUS;
use super::super::rooms::plain;
use crate::codec::SendEvery;
use crate::identity::WireId;
use crate::sharded::ShardedTeamRoom;
use crate::space::VisionGrid2;
use crate::testing::{PackedCodec, Position, Rated, fix_lent_pos, read_packed};

fn ctx(tick: u64) -> TickCtx<'static> {
    TickCtx {
        room: RoomId(1),
        tick,
        dt: Duration::from_secs_f64(1.0 / 30.0),
        idle: Default::default(),
    }
}

/// Shard 0's export of its own record, tick by tick, while the record
/// walks (x = -10: every 8th step): `(tick, exported y)`.
fn exports(delta: bool) -> (u64, Vec<(u64, i32)>) {
    let mut world = World::new();
    let room = ShardedTeamRoom::with_shard(
        plain::<Rated<PackedCodec>>(0),
        VisionGrid2::new(RADIUS),
        fix_lent_pos,
    );
    let mut room = if delta { room.with_delta() } else { room };
    let joined = room.on_join_as(&mut world, ConnectionId(1), "-10:-10:0");
    let entity = world
        .query::<(bevy_ecs::prelude::Entity, &WireId)>()
        .iter(&world)
        .find(|(_, w)| w.get() == joined.entity)
        .map(|(e, _)| e)
        .expect("the record");
    let mut seen = Vec::new();
    for tick in 1..=24u64 {
        let y = -10.0 - tick as f32;
        world.entity_mut(entity).insert(Position { x: -10.0, y });
        room.update(&mut world, &ctx(tick));
        let export = room
            .team_exchange(&mut world, &ctx(tick), &[], &TeamImports::default())
            .expect("the composite takes part");
        let body = &export
            .records
            .iter()
            .find(|r| r.wire == joined.entity)
            .expect("exported")
            .bytes;
        seen.push((tick, read_packed(&mut &body[..]).expect("a body").y));
    }
    (joined.entity, seen)
}

#[test]
fn the_owner_exports_a_change_on_its_due_steps() {
    let (wire, seen) = exports(true);
    let mut last = None;
    for (tick, y) in seen {
        // The room's step is its tick here (one update per tick from 1).
        let now = -10 - tick as i32;
        if tick == 1 || SendEvery::Ticks8.due(tick, wire) {
            assert_eq!(y, now, "tick {tick}: due (or new) — the current value");
        } else {
            assert_eq!(Some(y), last, "tick {tick}: not due — the body stays");
        }
        last = Some(y);
    }
    let (_, full) = exports(false);
    assert!(
        full.iter().all(|&(tick, y)| y == -10 - tick as i32),
        "the full mode exports every change: {full:?}"
    );
}
