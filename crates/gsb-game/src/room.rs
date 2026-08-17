//! [`DemoRoom`]: the demo game's [`RoomLogic`] implementation.
//!
//! A room owns one bevy [`World`] (exclusively — the room actor is the only
//! borrower) plus a small amount of bookkeeping:
//!
//! - `conn_entity`: which entity belongs to which connection;
//! - `last`: the wire content (entity bits → truncated `(x, y)`) of the
//!   **last emitted** snapshot of the room's single group
//!   (`GroupKey = ()`).
//!
//! Broadcasts are **per-group full, self-contained snapshots**: each tick
//! the room asks the logic for one snapshot per group; the logic encodes
//! the group's *entire* world once and reports whether anything changed
//! (including membership — a join/leave changes the set of entities). The
//! room then freezes the payload and shares it by reference with the
//! group's members. No delta, no history: a lost packet is healed by the
//! next snapshot; "nothing changed" stops the emission entirely, and the
//! room's low-rate keep-alive re-sends the cached snapshot so a client
//! that lost its last packet cannot stay stale forever.
//!
//! "No change" compares **exactly what the snapshot carries** — the set
//! of entities and their truncated positions — so the emission decision
//! is independent of [`bump`] discipline: a `Position` write without a
//! `bump()` is still broadcast, and a `bump()` with no content change
//! emits nothing (no band waste).
//!
//! `last` is a **single-group** ledger, correct because this room has
//! exactly one group. If you change `GroupKey` to a multi-group key
//! (e.g. `ConnectionId`), you MUST key the ledger by group: the room
//! calls `snapshot()` once per group per tick in unspecified order, and a
//! ledger shared across groups makes the groups visited after the first
//! see "no change" and their members starve (see `RoomLogic::snapshot`).
//!
//! Delta compression and area-of-interest grouping (a non-`()` `GroupKey`)
//! are the documented next steps (see `docs/DESIGN.md`).

use std::collections::HashMap;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::{ConnectionId, EntityId};
use gsb_core::room::{Action, RoomLogic, TickCtx};
use gsb_ecs::dirty::{EntityVersion, bump};
use gsb_ecs::{SystemCtx, SystemRunner};
use prost::Message;

use crate::components::{DEFAULT_SPEED, MoveTarget, Owner, Position, Speed};
use crate::op;
use crate::systems::MovementSystem;

/// The demo room: one moving entity per player, free 2D movement.
pub struct DemoRoom {
    runner: SystemRunner,
    conn_entity: HashMap<ConnectionId, Entity>,
    /// Wire content of the last emitted snapshot of the room's single
    /// group, as `(entity bits → (x, y))` (truncated to the wire's
    /// integer positions). The snapshot is re-emitted when this content
    /// changes — i.e. on any position change **or** membership change
    /// (join/leave), which is the room contract for "no change".
    ///
    /// Single-group by construction (`GroupKey = ()`); a multi-group key
    /// requires the ledger to be keyed by group (see module docs and
    /// `RoomLogic::snapshot`).
    last: HashMap<u64, (i32, i32)>,
}

impl Default for DemoRoom {
    fn default() -> Self {
        Self::new()
    }
}

impl DemoRoom {
    pub fn new() -> Self {
        let mut runner = SystemRunner::new();
        runner.add(MovementSystem);
        Self {
            runner,
            conn_entity: HashMap::new(),
            last: HashMap::new(),
        }
    }
}

/// Deterministic pseudo-random spawn point in a 100×100 arena, derived from
/// the connection id (stable across room re-joins in the same session).
fn spawn_pos(conn: ConnectionId) -> (f32, f32) {
    let h = conn.0.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let x = ((h % 1000) as f32) / 10.0 - 50.0;
    let y = (((h >> 32) % 1000) as f32) / 10.0 - 50.0;
    (x, y)
}

impl RoomLogic<World> for DemoRoom {
    // One group per room: everyone sees the whole world. (The interface
    // supports finer groupings, e.g. `GroupKey = ConnectionId` — but then
    // the `last` ledger above must be keyed by group; see the module
    // docs and `RoomLogic::snapshot`.)
    type GroupKey = ();

    fn snapshot_op(&self) -> u16 {
        op::WORLD_SNAPSHOT
    }
    fn private_op(&self) -> u16 {
        op::PRIVATE
    }

    fn group_of(&self, _world: &World, _conn: ConnectionId) -> Self::GroupKey {
        Default::default()
    }

    fn snapshot(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        _group: &Self::GroupKey,
        out: &mut bytes::BytesMut,
    ) -> bool {
        // Collect the broadcastable state (entity, truncated wire
        // position) while the query holds the world borrow.
        let mut current: Vec<(u64, i32, i32)> = Vec::new();
        {
            let mut query = world.query::<(Entity, &Position)>();
            for (entity, pos) in query.iter(world) {
                current.push((entity.to_bits(), pos.x as i32, pos.y as i32));
            }
        }

        // "No change" = identical wire content: the same set of entities
        // at the same (truncated) positions. A membership change
        // (join/leave) or any position change flips it. The comparison is
        // on exactly what the snapshot carries, so the decision is
        // independent of `bump()` discipline (see module docs).
        let changed = self.last.len() != current.len()
            || current.iter().any(|(entity, x, y)| {
                self.last
                    .get(entity)
                    .map(|(lx, ly)| *x != *lx || *y != *ly)
                    .unwrap_or(true)
            });
        if !changed {
            return false;
        }

        let mut snap = crate::game::WorldSnapshot {
            sequence: ctx.tick,
            entities: Vec::with_capacity(current.len()),
        };
        for (entity, x, y) in &current {
            snap.entities.push(crate::game::EntityRecord {
                entity: *entity,
                x: *x,
                y: *y,
            });
        }
        // Encoding into an in-memory buffer cannot fail (no I/O, unbounded
        // capacity); treat a failure as a bug rather than dropping the
        // snapshot.
        snap
            .encode(out)
            .expect("protobuf encode into an in-memory buffer failed");

        self.last.clear();
        for (entity, x, y) in &current {
            self.last.insert(*entity, (*x, *y));
        }
        true
    }

    fn on_join(&mut self, world: &mut World, conn: ConnectionId) -> EntityId {
        let (x, y) = spawn_pos(conn);
        let entity = world
            .spawn((
                Position { x, y },
                Owner(conn),
                Speed(DEFAULT_SPEED),
                EntityVersion(0),
            ))
            .id();
        self.conn_entity.insert(conn, entity);
        // No spawn event: membership is expressed by presence in the next
        // snapshot, which now includes the new entity (the join happened in
        // the control phase, before this tick's broadcast).
        entity.to_bits()
    }

    fn on_leave(&mut self, world: &mut World, conn: ConnectionId) {
        // No remove event: the entity simply drops out of the next
        // snapshot. (The room's stale-leave guard ensures a late leave of
        // a re-joined connection cannot despawn the new entity.)
        if let Some(entity) = self.conn_entity.remove(&conn)
            && world.get_entity(entity).is_ok()
        {
            world.despawn(entity);
        }
    }

    fn ingest(&mut self, world: &mut World, _ctx: &TickCtx, actions: &mut Vec<Action>) {
        for action in actions.drain(..) {
            if action.op != op::MOVE_TO {
                continue;
            }
            let Ok(msg) = <crate::game::MoveTo as Message>::decode(&action.payload[..]) else {
                tracing::warn!(?action.op, "undecodable MOVE_TO payload ignored");
                continue;
            };
            let Some(entity) = self.conn_entity.get(&action.conn).copied() else {
                continue; // not in a room (stale action)
            };
            if world.get_entity(entity).is_err() {
                continue; // entity already gone
            }
            world.entity_mut(entity).insert(MoveTarget {
                x: msg.x as f32,
                y: msg.y as f32,
            });
            bump(world, entity);
        }
    }

    fn update(&mut self, world: &mut World, ctx: &TickCtx) {
        let sys_ctx = SystemCtx {
            tick: ctx.tick,
            dt: ctx.dt.as_secs_f32(),
        };
        self.runner.run_all(world, &sys_ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use gsb_core::id::RoomId;

    fn ctx1() -> TickCtx {
        TickCtx {
            room: RoomId(1),
            tick: 1,
            dt: Duration::from_secs_f64(1.0 / 30.0),
        }
    }

    /// The "no change" decision compares the wire content, not the
    /// `EntityVersion` ledger: a `Position` write **without** `bump()`
    /// must still be broadcast (the old `(entity → version)` detector
    /// would have stayed silent here — a comment-only discipline).
    #[test]
    fn snapshot_emits_on_unbumped_position_write() {
        let mut world = World::new();
        let mut room = DemoRoom::new();
        let entity = room.on_join(&mut world, ConnectionId(1));
        let ctx = ctx1();
        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx, &(), &mut out), "join emits");

        let e = Entity::from_bits(entity);
        world.entity_mut(e).insert(Position { x: 42.0, y: -7.0 });

        let mut out2 = bytes::BytesMut::new();
        assert!(
            room.snapshot(&mut world, &ctx, &(), &mut out2),
            "an unbumped position write must still emit"
        );
        let snap = crate::game::WorldSnapshot::decode(out2.as_ref()).expect("decode");
        assert_eq!(snap.entities.len(), 1);
        assert_eq!(snap.entities[0].x, 42);
        assert_eq!(snap.entities[0].y, -7);
    }

    /// Identical wire content stays silent (a `bump()` with no content
    /// change emits nothing); a membership change is a wire-content
    /// change and must emit.
    #[test]
    fn snapshot_silent_when_wire_content_unchanged() {
        let mut world = World::new();
        let mut room = DemoRoom::new();
        room.on_join(&mut world, ConnectionId(1));
        let ctx = ctx1();
        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx, &(), &mut out), "join emits");

        // A version bump with no content change...
        let entity = *room.conn_entity.get(&ConnectionId(1)).unwrap();
        bump(&mut world, entity);
        let mut out2 = bytes::BytesMut::new();
        assert!(
            !room.snapshot(&mut world, &ctx, &(), &mut out2),
            "bump without content change ⇒ no change ⇒ silent"
        );

        // ...and a leave is a wire-content change.
        room.on_leave(&mut world, ConnectionId(1));
        let mut out3 = bytes::BytesMut::new();
        assert!(
            room.snapshot(&mut world, &ctx, &(), &mut out3),
            "leave ⇒ wire content changed ⇒ emit"
        );
        let snap = crate::game::WorldSnapshot::decode(out3.as_ref()).expect("decode");
        assert!(snap.entities.is_empty(), "left: empty world snapshot");
    }
}
