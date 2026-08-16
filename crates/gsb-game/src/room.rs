//! [`DemoRoom`]: the demo game's [`RoomLogic`] implementation.
//!
//! A room owns one bevy [`World`] (exclusively — the room actor is the only
//! borrower) plus a small amount of bookkeeping:
//!
//! - `conn_entity`: which entity belongs to which connection;
//! - `last`: the `(entity → version)` pairs of the **last emitted**
//!   snapshot of the room's single group (`GroupKey = ()`).
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
//! The snapshot wire content is a deterministic function of the
//! `(entity, version)` pairs: positions are only ever written through
//! [`bump`], so comparing pairs is exactly "do the bytes change".
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
    /// Last emitted snapshot of the room's single group, as
    /// `(entity bits → version)`. The snapshot is re-emitted when this set
    /// changes — i.e. on any version bump **or** membership change
    /// (join/leave), which is the room contract for "no change".
    last: HashMap<u64, u64>,
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
    // supports finer groupings, e.g. `GroupKey = ConnectionId`.)
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
        // Collect the broadcastable state (entity, position, version)
        // while the query holds the world borrow.
        let mut current: Vec<(u64, i32, i32, u64)> = Vec::new();
        {
            let mut query = world.query::<(Entity, &Position, &EntityVersion)>();
            for (entity, pos, version) in query.iter(world) {
                current.push((entity.to_bits(), pos.x as i32, pos.y as i32, version.0));
            }
        }

        // "No change" = the identical (entity, version) set. A membership
        // change (join/leave) or any version bump flips it. (Wire bytes are
        // a deterministic function of these pairs — see module docs.)
        let changed = self.last.len() != current.len()
            || current
                .iter()
                .any(|(entity, _, _, version)| self.last.get(entity) != Some(version));
        if !changed {
            return false;
        }

        let mut snap = crate::game::WorldSnapshot {
            sequence: ctx.tick,
            entities: Vec::with_capacity(current.len()),
        };
        for (entity, x, y, _version) in &current {
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
        for (entity, _x, _y, version) in &current {
            self.last.insert(*entity, *version);
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
