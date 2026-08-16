//! [`DemoRoom`]: the demo game's [`RoomLogic`] implementation.
//!
//! A room owns one bevy [`World`] (exclusively — the room actor is the only
//! borrower) plus a small amount of bookkeeping:
//!
//! - `conn_entity`: which entity belongs to which connection;
//! - `last_sent`: per connection, the last `EntityVersion` sent for each
//!   entity. A late joiner has an empty map, so the next broadcast sends it
//!   the **entire world** (full snapshot catch-up);
//! - `pending_out`: event frames (spawn/remove) queued between ticks and
//!   flushed at the start of the next broadcast.
//!
//! Broadcasts are **full, self-contained snapshots**: an entity is re-sent to
//! a connection whenever its version differs from what that connection last
//! received. A dropped batch (slow client) therefore costs at most one tick
//! of staleness — no client ever needs to replay history. Encoding is
//! O(dirty entities); delivery is O(dirty × conns) `Bytes` (Arc) clones.
//! Delta compression and area-of-interest filtering are the documented next
//! steps (see `docs/DESIGN.md`).

use std::collections::HashMap;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::{ConnectionId, EntityId};
use gsb_core::room::{Action, OutSink, RoomLogic, TickCtx};
use gsb_ecs::dirty::{EntityVersion, bump};
use gsb_ecs::{SystemCtx, SystemRunner};
use gsb_protocol::FrameBody;
use prost::Message;

use crate::components::{DEFAULT_SPEED, MoveTarget, Owner, Position, Speed};
use crate::op;
use crate::systems::MovementSystem;

/// The demo room: one moving entity per player, free 2D movement.
pub struct DemoRoom {
    runner: SystemRunner,
    conn_entity: HashMap<ConnectionId, Entity>,
    /// Last version sent per entity, per connection. Empty for a fresh
    /// joiner ⇒ full world snapshot on the next broadcast.
    last_sent: HashMap<ConnectionId, HashMap<Entity, u64>>,
    pending_out: Vec<FrameBody>,
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
            last_sent: HashMap::new(),
            pending_out: Vec::new(),
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
        // The joiner starts with an empty view: the next broadcast sends it
        // the whole world (full snapshot catch-up).
        self.last_sent.entry(conn).or_default();

        // Everyone in the room (and the joiner, from the next tick on) needs
        // to know the new entity exists.
        let msg = crate::game::EntitySpawned {
            entity: entity.to_bits(),
            x: x as i32,
            y: y as i32,
        };
        self.pending_out
            .push(FrameBody::new(op::ENTITY_SPAWNED, msg.encode_to_vec()));

        entity.to_bits()
    }

    fn on_leave(&mut self, world: &mut World, conn: ConnectionId) {
        if let Some(entity) = self.conn_entity.remove(&conn) {
            if world.get_entity(entity).is_ok() {
                world.despawn(entity);
            }
            let msg = crate::game::EntityRemoved {
                entity: entity.to_bits(),
            };
            self.pending_out
                .push(FrameBody::new(op::ENTITY_REMOVED, msg.encode_to_vec()));
            // Per-connection records are pruned by the broadcast's alive
            // sweep; dropping the leaver's whole map below is the fast path.
            for map in self.last_sent.values_mut() {
                map.remove(&entity);
            }
        }
        self.last_sent.remove(&conn);
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

    fn broadcast(&mut self, world: &mut World, _ctx: &TickCtx, sink: &mut OutSink<'_>) {
        // 1. Event frames queued between ticks (spawn/leave) → everyone.
        for frame in self.pending_out.drain(..) {
            sink.broadcast(frame);
        }

        // 2. Per-connection snapshots: an entity is re-sent to a connection
        //    whose recorded version differs from the current one. A joiner's
        //    record map is empty, so it receives the entire world on this
        //    very broadcast (collect state while the query holds the borrow).
        let mut query = world.query::<(Entity, &Position, &EntityVersion)>();
        let mut snapshots: Vec<(Entity, f32, f32, u64)> = Vec::new();
        let mut alive: Vec<Entity> = Vec::new();
        for (entity, pos, version) in query.iter(world) {
            alive.push(entity);
            snapshots.push((entity, pos.x, pos.y, version.0));
        }
        let conns: Vec<ConnectionId> = sink.connections().collect();
        for (entity, x, y, version) in &snapshots {
            let mut need: Vec<ConnectionId> = Vec::new();
            for &conn in &conns {
                let map = self.last_sent.entry(conn).or_default();
                if map.get(entity) != Some(version) {
                    need.push(conn);
                }
            }
            if need.is_empty() {
                continue; // every connection is already up to date
            }
            let msg = crate::game::EntityState {
                entity: entity.to_bits(),
                x: *x as i32,
                y: *y as i32,
                version: *version,
            };
            let frame = FrameBody::new(op::ENTITY_STATE, msg.encode_to_vec());
            for conn in need {
                sink.send(conn, frame.clone());
                self.last_sent
                    .get_mut(&conn)
                    .expect("connection was just collected")
                    .insert(*entity, *version);
            }
        }

        // 3. Per-connection cleanup of entities that no longer exist
        //    (defensive: v1 despawns only go through on_leave, which queues
        //    its own ENTITY_REMOVED).
        let alive_set: std::collections::HashSet<Entity> = alive.into_iter().collect();
        for &conn in &conns {
            let Some(map) = self.last_sent.get_mut(&conn) else {
                continue;
            };
            let gone: Vec<Entity> = map
                .keys()
                .filter(|e| !alive_set.contains(*e))
                .copied()
                .collect();
            for entity in gone {
                let msg = crate::game::EntityRemoved {
                    entity: entity.to_bits(),
                };
                sink.send(
                    conn,
                    FrameBody::new(op::ENTITY_REMOVED, msg.encode_to_vec()),
                );
                map.remove(&entity);
            }
        }
    }
}
