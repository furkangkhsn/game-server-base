//! The machinery the demo rooms share — everything that is **not** a
//! visibility-strategy decision.
//!
//! The four rooms (`OpenRoom`, `AoiRoom`, `TeamRoom`, `SectorRoom`) run
//! the *same*
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
//! own `runner` / `player_entity` / `minter` fields (the group-key
//! type is per-room, and the inline tests reach into these fields), and
//! the shared behaviour is the only copy. A *Visibility* trait over the
//! strategies was considered and rejected — see `docs/ROADMAP.md`,
//! visibility turn, Bölüm C.
//!
//! **The minting point.** Every identity a room stamps is drawn from the
//! room's one [`Minter`] (`kit/identity.rs`) — the only construction
//! path of [`WireId`] (private field, no constructor, no `Default`): the
//! counter's space stays closed to everything else.
//!
//! **Phase 0 (KIT-ARCHITECTURE §10).** The game-side halves of what used
//! to live here — the movement system stack, `MOVE_TO` decoding, the bot
//! wander, the spawn bundle — moved to the demo module; this module reaches
//! them only through [`crate::kit::seam`].

mod cells;
mod frame;
mod hooks;
mod input;
mod park;

pub(crate) use cells::*;
pub(crate) use frame::*;
pub(super) use hooks::*;
pub use input::InputSeq;
pub(crate) use input::{append_responses, emit_private};
pub(crate) use park::*;

use std::collections::HashMap;

use bevy_ecs::prelude::{Component, Entity, With, Without, World};
use gsb_core::id::PlayerId;
use gsb_core::room::TickCtx;
use gsb_ecs::{SystemCtx, SystemRunner};

use crate::kit::identity::{Minter, WireId};

/// The leave path, shared by all rooms: no remove event — the entity
/// simply drops out of the next snapshot (membership is expressed by
/// presence). The room's stale-leave guard ensures a late leave of a
/// re-joined connection cannot despawn the new entity (nor drop the
/// re-joined session's input state: the removal is guarded by the same
/// condition).
pub(crate) fn on_leave(
    player_entity: &mut HashMap<PlayerId, Entity>,
    world: &mut World,
    player: PlayerId,
    input: &mut InputSeq,
) {
    if let Some(entity) = player_entity.remove(&player)
        && world.get_entity(entity).is_ok()
    {
        world.despawn(entity);
        input.end(player);
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
/// discipline): entities with the broadcast marker `M` (the codec's
/// `Marker`; the demo: `Position`) but no [`WireId`] yet — anything
/// spawned outside a join (bullets, NPCs, traps, …) — are stamped with
/// the next serial, so the broadcast set is exactly "has the marker" and
/// nothing can be silently invisible. Two passes (the
/// orphan query holds the world borrow, so collect first, then write —
/// the same pattern as the demo's movement system); the stamp is idempotent and
/// costs nothing in steady state (the orphan query matches nothing once
/// every entity is stamped).
///
/// Call site: `OpenRoom` stamps in the broadcast pass (its `snapshot`
/// collects the content it encodes), the other rooms stamp in `update`
/// (their per-tick caches are built right after). Either call site keeps
/// the guarantee: an orphan appears in the very snapshot that notices
/// it.
pub(super) fn stamp_orphans<M: Component>(minter: &mut Minter, world: &mut World) {
    let orphans: Vec<Entity> = world
        .query_filtered::<Entity, (With<M>, Without<WireId>)>()
        .iter(world)
        .collect();
    for entity in orphans {
        world.entity_mut(entity).insert(minter.mint());
    }
}

// ════════════════════════════════════════════════════════════════════════
// The demo disconnect policy (docs/RECONNECT.md §3/§9 — Tur B): a MOBA-
// style park. A dropped transport does NOT despawn the hero; the entity
// stays in the world (visible in every snapshot, holding its room-cap
// slot, §4) for a configurable grace. If the human returns first, the
// core's resume swaps the channels back onto the live entity; if the
// grace runs out, the entity is handed to a stub bot that keeps playing
// it through the ordinary input path (`ExpireTo::AiHandover`).
//
// The whole policy lives here as plain functions over the rooms' fields
// (the same shape as `ingest` / `on_join` above): every `RoomLogic` demo
// room calls the same five hooks with its own tables, and the sharded
// variant carries the park record inside the migrating state (§14.2).
// ════════════════════════════════════════════════════════════════════════

// ════════════════════════════════════════════════════════════════════════
// The shared CELL-DELTA machinery: the spatial visibility strategies run
// the same encoding engine, so it lives here once. Two rooms drive it —
// [`crate::kit::aoi::AoiRoom`] (single world) and
// [`crate::kit::sharded::ShardedSpatialRoom`] (the Faz B per-shard composite) —
// and they differ only in WHAT feeds the bookkeeping (bevy's dirty query
// alone vs the dirty query PLUS a diff of the borrowed border strip) and
// in who counts as a member. The wire format (header/pieces/oneof framing)
// and the delta arithmetic
// (change list = the diff, order-independent flags/birth roll) are
// byte-for-byte common.
//
// What deliberately stayed per-room: the session surface (`conn_view`,
// `group_full_emitted` consumers, `private`'s one-shot shape) and — on the
// sharded side — the borrowed-strip ledger, which is that room's
// load-bearing subtlety (see its module docs).
// ════════════════════════════════════════════════════════════════════════
