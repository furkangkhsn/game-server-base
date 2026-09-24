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
//! own `runner` / `player_entity` / `next_wire_id` fields (the group-key
//! type is per-room, and the inline tests reach into these fields), and
//! the shared behaviour is the only copy. A *Visibility* trait over the
//! strategies was considered and rejected — see `docs/ROADMAP.md`,
//! visibility turn, Bölüm C.
//!
//! **The minting point.** [`next_serial`] is the *only* caller of
//! [`WireId::new`] in the crate (crate-private constructor, no `Default`
//! on the type — see `kit/identity.rs`): every room's counter is the only
//! source of identities for that room's entities, and the counter's
//! space stays closed to everything else.
//!
//! **Phase 0 (KIT-ARCHITECTURE §10).** The game-side halves of what used
//! to live here — the movement system stack, `MOVE_TO` decoding, the bot
//! wander, the spawn bundle — moved to the demo module; this module reaches
//! them only through [`crate::kit::seam`].

mod cells;
mod park;

pub use cells::Cell;
pub(crate) use cells::*;
pub(crate) use park::*;

use std::collections::HashMap;

use bevy_ecs::prelude::{Entity, Without, World};
use bytes::BufMut;
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Admission, TickCtx};
use gsb_ecs::{SystemCtx, SystemRunner};
use prost::Message;

use crate::kit::identity::WireId;
use crate::kit::seam;
use crate::kit::seam::Position;

/// Mint the next wire identity from the room's single monotonic counter
/// (see module docs, "The minting point").
#[inline]
pub(crate) fn next_serial(next_wire_id: &mut u64) -> WireId {
    *next_wire_id += 1;
    WireId::new(*next_wire_id)
}

/// Per-connection input sequence state (shared by all rooms; see
/// [`Self::admit`] and [`emit_private`]).
///
/// `hwm` is the highest input sequence this connection has *processed*
/// (the high-water mark); `acked` is the highest sequence already
/// *reported* to the connection. Both are reset on every (re)join — a
/// rejoin is a new session, and the client is expected to restart its
/// counter at 1 (the server-side reset makes the first input of the new
/// session processable even if the client forgets).
#[derive(Debug, Default)]
pub(crate) struct InputState {
    /// Highest processed seq (0 = nothing numbered processed yet).
    pub hwm: u64,
    /// Highest seq already acked (the last `InputAck` sent).
    pub acked: u64,
}

impl InputState {
    /// **Input sequence rule** (the client prediction-reconciliation
    /// signal) — whether an input numbered `seq` is processed; the game's
    /// input decoder (the demo's `ingest`) asks this for every decoded
    /// input. The client numbers its inputs (monotonic from 1 per
    /// session, `seq = 0` = unnumbered legacy). The server processes a
    /// numbered action only when it is *strictly newer* than this
    /// connection's high-water mark:
    ///
    /// - `seq > hwm` — process, and advance `hwm = seq`;
    /// - `seq <= hwm` — a duplicate or a reordered/late action: **dropped,
    ///   silently**. This is a *normal race* of the lossy game band (a
    ///   retransmission or an out-of-order arrival), not a protocol
    ///   violation: no error is answered and nothing is counted against the
    ///   connection's violation budget (which is spent on structural
    ///   protocol errors, and a client re-sending its own input is always
    ///   legitimate). Applying a stale target would regress the entity to an
    ///   old command, so dropping is the only correct behaviour;
    /// - `seq = 0` (legacy/unnumbered) — process, never advance `hwm`. This
    ///   keeps unnumbered clients (and all pre-seq tests) working unchanged.
    ///
    /// Gaps (a lost input) do not block the mark: the ack is a
    /// high-water mark, not a contiguity claim (see `InputAck` in
    /// `game.proto`).
    #[inline]
    pub(crate) fn admit(&mut self, seq: u64) -> bool {
        if seq == 0 {
            // Legacy/unnumbered: process, never advance the mark.
            true
        } else if seq > self.hwm {
            self.hwm = seq;
            true
        } else {
            false // duplicate / reordered late: dropped (normal race)
        }
    }
}

/// The player-spawn path, shared by all rooms: the deterministic spawn
/// point (same distribution in every strategy — a fair comparison in the
/// load generator), a fresh wire identity AND a fresh stable player
/// identity through their minting counters, the player→entity table
/// update, and the input session reset (a (re)join is a new input
/// session — see [`InputState`]). Returns the [`Admission`] (the stable
/// `PlayerId` keys every table from here on; the entity/wire id also
/// goes to the joiner in `JOIN_ROOM_RESULT`, so both paths share one
/// space). Identity policy note: the demo mints a fresh PlayerId per
/// first join; resume stability comes from the park ledger carrying it.
pub(crate) fn on_join(
    player_entity: &mut HashMap<PlayerId, Entity>,
    next_player_id: &mut u64,
    next_wire_id: &mut u64,
    spawn_half: f32,
    world: &mut World,
    conn: ConnectionId,
    input: &mut HashMap<PlayerId, InputState>,
) -> Admission {
    *next_player_id += 1;
    let player = PlayerId(*next_player_id);
    input.insert(player, InputState::default());
    // The spawn point is derived from the TRANSPORT session id (as it
    // always was): the load generator's home distribution pairs with it.
    // (Game side: the spawn point and the player bundle; kit side: the
    // identity stamped on it.)
    let wire = next_serial(next_wire_id);
    let entity = seam::spawn_player(world, conn, spawn_half, wire);
    player_entity.insert(player, entity);
    Admission {
        player,
        entity: wire.get(),
    }
}

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
    input: &mut HashMap<PlayerId, InputState>,
) {
    if let Some(entity) = player_entity.remove(&player)
        && world.get_entity(entity).is_ok()
    {
        world.despawn(entity);
        input.remove(&player);
    }
}

/// Emit this connection's private frame for the tick: the pending input
/// acknowledgment (the `ack` oneof) and/or the connection's queued RPC
/// answers (the `responses` repeated field, see `gsb_core::rpc`), as ONE
/// `Private` frame — the per-tick, per-connection slot of the batch, so
/// everything rides the SAME delivery as that tick's group snapshot (no
/// extra send, no extra await: the tick body stays synchronous).
///
/// Returns `true` when a frame was produced. The ack part advances
/// `acked` only when emitted (the mark is the highest processed seq, so
/// the client's reconciliation stays sound); the responses are delivered
/// exactly once (the actor's queue is drained per tick — see the core's
/// fan-out) and the logic decides their order (arrival order within the
/// tick, per the `gsb_core::rpc` contract). Passing an empty `responses`
/// slice is the ack-only form (the pre-Faz-3 shape every room had before
/// the shard actor gained its RPC machinery).
pub(crate) fn emit_private(
    input: &mut HashMap<PlayerId, InputState>,
    player: PlayerId,
    responses: &[gsb_core::rpc::RpcReply],
    out: &mut bytes::BytesMut,
) -> bool {
    let mut ack_up_to: Option<u64> = None;
    if let Some(st) = input.get_mut(&player)
        && st.hwm > st.acked
    {
        ack_up_to = Some(st.hwm);
        st.acked = st.hwm;
    }
    if ack_up_to.is_none() && responses.is_empty() {
        return false;
    }
    let frame = seam::Private {
        payload: ack_up_to.map(|upto| {
            seam::private::Payload::Ack(seam::InputAck {
                processed_up_to: upto,
            })
        }),
        // The core owns both halves of the RPC envelope (base.proto), so
        // the reply's wire shape comes from the core's conversion — the
        // game crate never re-derives the field mapping.
        responses: responses.iter().map(Into::into).collect(),
    };
    frame
        .encode(out)
        .expect("protobuf encode into an in-memory buffer failed");
    true
}

/// Append this connection's queued RPC answers to a HAND-ENCODED
/// `Private` frame body (the AOI one-shot full path, which cannot go
/// through the generated type without re-encoding the snapshot): field
/// 3 (`responses`, tag 0x1A), one length-delimited
/// `gsb.base.RpcResponse` per entry. No allocation beyond the
/// per-message length probe (responses are rare — the steady-state tick
/// has none).
pub(crate) fn append_responses(responses: &[gsb_core::rpc::RpcReply], out: &mut bytes::BytesMut) {
    for r in responses {
        let msg: gsb_protocol::base::RpcResponse = r.into();
        out.put_u8(0x1A); // Private field 3 (responses), LEN
        prost::encoding::varint::encode_varint(msg.encoded_len() as u64, out);
        msg.encode(out)
            .expect("protobuf encode into an in-memory buffer failed");
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
/// the same pattern as the demo's movement system); the stamp is idempotent and
/// costs nothing in steady state (the orphan query matches nothing once
/// every entity is stamped).
///
/// Call site: `OpenRoom` stamps in the broadcast pass (its `snapshot`
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
