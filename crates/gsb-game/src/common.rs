//! The machinery the demo rooms share — everything that is **not** a
//! visibility-strategy decision.
//!
//! The four rooms ([`crate::room::DemoRoom`], [`crate::aoi::AoiRoom`],
//! [`crate::team::TeamRoom`], [`crate::pvs::SectorRoom`]) run the *same*
//! game: the same components, the same movement system, the same wire
//! format, the same spawn distribution, the same identity rules. They
//! differ in exactly the two things the group-snapshot architecture
//! leaves to the game: the group key (`GroupKey`) and what lands in each
//! group's snapshot. Everything else — wire-identity minting, the
//! connection→entity table, `MOVE_TO` ingestion, the system run, orphan
//! stamping — is byte-for-byte identical across the rooms, so it lives
//! here, once.
//!
//! This is deliberately a set of plain functions over the rooms' fields
//! (not a trait, not a struct that owns the fields): each room keeps its
//! own `runner` / `conn_entity` / `next_wire_id` fields (the group-key
//! type is per-room, and the inline tests reach into these fields), and
//! the shared behaviour is the only copy. A *Visibility* trait over the
//! strategies was considered and rejected — see `docs/ROADMAP.md`,
//! visibility turn, Bölüm C.
//!
//! **The minting point.** [`next_serial`] is the *only* caller of
//! [`WireId::new`] in the crate (crate-private constructor, no `Default`
//! on the type — see `components.rs`): every room's counter is the only
//! source of identities for that room's entities, and the counter's
//! space stays closed to everything else.

use std::collections::HashMap;

use bevy_ecs::prelude::{Entity, World, Without};
use gsb_core::id::{ConnectionId, EntityId};
use gsb_core::room::{Action, TickCtx};
use gsb_ecs::{SystemCtx, SystemRunner};
use prost::Message;

use crate::components::{DEFAULT_SPEED, MoveTarget, Position, Speed, WireId};
use crate::op;
use crate::room::spawn_pos;
use crate::systems::MovementSystem;

/// The system stack every demo room runs (the demo game is the same in
/// all of them — only visibility differs).
pub(crate) fn movement_runner() -> SystemRunner {
    let mut runner = SystemRunner::new();
    runner.add(MovementSystem);
    runner
}

/// Mint the next wire identity from the room's single monotonic counter
/// (see module docs, "The minting point").
#[inline]
pub(crate) fn next_serial(next_wire_id: &mut u64) -> WireId {
    *next_wire_id += 1;
    WireId::new(*next_wire_id)
}

/// The player-spawn path, shared by all rooms: the deterministic spawn
/// point (same distribution in every strategy — a fair comparison in the
/// load generator), a fresh wire identity through the room's single
/// minting point, and the connection→entity table update. Returns the
/// wire id (it also goes to the joiner in `JOIN_ROOM_RESULT`, so both
/// paths share one space).
pub(crate) fn on_join(
    conn_entity: &mut HashMap<ConnectionId, Entity>,
    next_wire_id: &mut u64,
    spawn_half: f32,
    world: &mut World,
    conn: ConnectionId,
) -> EntityId {
    let (x, y) = spawn_pos(conn, spawn_half);
    let wire = next_serial(next_wire_id);
    let entity = world
        .spawn((Position { x, y }, Speed(DEFAULT_SPEED), wire))
        .id();
    conn_entity.insert(conn, entity);
    wire.get()
}

/// The leave path, shared by all rooms: no remove event — the entity
/// simply drops out of the next snapshot (membership is expressed by
/// presence). The room's stale-leave guard ensures a late leave of a
/// re-joined connection cannot despawn the new entity.
pub(crate) fn on_leave(
    conn_entity: &mut HashMap<ConnectionId, Entity>,
    world: &mut World,
    conn: ConnectionId,
) {
    if let Some(entity) = conn_entity.remove(&conn)
        && world.get_entity(entity).is_ok()
    {
        world.despawn(entity);
    }
}

/// `MOVE_TO` ingestion, shared by all rooms: decode the game message,
/// guard against stale actions (connection not in the room) and vanished
/// entities, and write the [`MoveTarget`]. Anything else is ignored with
/// a warning (the op code is the router; undecodable payloads are a
/// client bug, not a reason to drop the connection).
pub(crate) fn ingest(
    conn_entity: &HashMap<ConnectionId, Entity>,
    world: &mut World,
    actions: &mut Vec<Action>,
) {
    for action in actions.drain(..) {
        if action.op != op::MOVE_TO {
            continue;
        }
        let Ok(msg) = <crate::game::MoveTo as Message>::decode(&action.payload[..]) else {
            tracing::warn!(?action.op, "undecodable MOVE_TO payload ignored");
            continue;
        };
        let Some(entity) = conn_entity.get(&action.conn).copied() else {
            continue; // not in a room (stale action)
        };
        if world.get_entity(entity).is_err() {
            continue; // entity already gone
        }
        world.entity_mut(entity).insert(MoveTarget {
            x: msg.x as f32,
            y: msg.y as f32,
        });
    }
}

/// Run the room's systems for this tick (single-threaded, ordered — the
/// room actor is the only owner of the world, so no synchronization is
/// ever needed).
pub(crate) fn run_systems(runner: &mut SystemRunner, world: &mut World, ctx: &TickCtx) {
    let sys_ctx = SystemCtx {
        tick: ctx.tick,
        dt: ctx.dt.as_secs_f32(),
    };
    runner.run_all(world, &sys_ctx);
}

/// The orphan stamp (the broadcast set is *structural*, not a
/// discipline): entities with a [`Position`] but no [`WireId`] yet —
/// anything spawned outside `on_join` (bullets, NPCs, traps, …) — are
/// stamped with the next serial, so the broadcast set is exactly "has a
/// `Position`" and nothing can be silently invisible. Two passes (the
/// orphan query holds the world borrow, so collect first, then write —
/// the same pattern as [`MovementSystem`]); the stamp is idempotent and
/// costs nothing in steady state (the orphan query matches nothing once
/// every entity is stamped).
///
/// Call site: `DemoRoom` stamps in the broadcast pass (its `snapshot`
/// collects the content it encodes), the other rooms stamp in `update`
/// (their per-tick caches are built right after). Either call site keeps
/// the guarantee: an orphan appears in the very snapshot that notices
/// it.
pub(crate) fn stamp_orphans(next_wire_id: &mut u64, world: &mut World) {
    let orphans: Vec<Entity> = world
        .query_filtered::<(Entity, &Position), Without<WireId>>()
        .iter(world)
        .map(|(entity, _)| entity)
        .collect();
    for entity in orphans {
        world.entity_mut(entity).insert(next_serial(next_wire_id));
    }
}
