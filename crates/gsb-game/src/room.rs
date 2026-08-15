//! [`DemoRoom`]: the demo game's [`RoomLogic`] implementation.
//!
//! A room owns one bevy [`World`] (exclusively — the room actor is the only
//! borrower) plus a small amount of bookkeeping:
//!
//! - `conn_entity`: which entity belongs to which connection;
//! - `last_sent`: the last broadcast `EntityVersion` per entity (drives the
//!   dirty check in the broadcast phase);
//! - `pending_out`: event frames (spawn/remove) queued between ticks and
//!   flushed at the start of the next broadcast.
//!
//! Broadcasts are **full, self-contained snapshots** of every entity whose
//! version changed since the last broadcast. A dropped batch (slow client)
//! therefore costs at most one tick of staleness — no client ever needs to
//! replay history. Delta compression and area-of-interest filtering are the
//! documented next steps (see `docs/DESIGN.md`).

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
    last_sent: HashMap<Entity, u64>,
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
            self.last_sent.remove(&entity);
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

    fn broadcast(&mut self, world: &mut World, _ctx: &TickCtx, sink: &mut OutSink<'_>) {
        // 1. Event frames queued between ticks (spawn/leave).
        for frame in self.pending_out.drain(..) {
            sink.broadcast(frame);
        }

        // 2. Full snapshots of every entity whose version changed since the
        //    last broadcast (collect while the query holds the borrow).
        let mut query = world.query::<(Entity, &Position, &EntityVersion)>();
        let mut dirty: Vec<(Entity, f32, f32, u64)> = Vec::new();
        let mut alive: Vec<Entity> = Vec::new();
        for (entity, pos, version) in query.iter(world) {
            alive.push(entity);
            if self.last_sent.get(&entity) == Some(&version.0) {
                continue; // unchanged since last snapshot
            }
            dirty.push((entity, pos.x, pos.y, version.0));
        }
        for (entity, x, y, version) in dirty {
            let msg = crate::game::EntityState {
                entity: entity.to_bits(),
                x: x as i32,
                y: y as i32,
                version,
            };
            sink.broadcast(FrameBody::new(op::ENTITY_STATE, msg.encode_to_vec()));
            self.last_sent.insert(entity, version);
        }

        // 3. Entities we have broadcast before but that no longer exist
        //    (defensive: v1 despawns only go through on_leave, which queues
        //    its own ENTITY_REMOVED).
        let alive_set: std::collections::HashSet<Entity> = alive.into_iter().collect();
        self.last_sent.retain(|entity, _| {
            if !alive_set.contains(entity) {
                let msg = crate::game::EntityRemoved {
                    entity: entity.to_bits(),
                };
                sink.broadcast(FrameBody::new(op::ENTITY_REMOVED, msg.encode_to_vec()));
                false
            } else {
                true
            }
        });
    }
}
