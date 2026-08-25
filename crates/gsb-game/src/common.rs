//! The machinery the demo rooms share — everything that is **not** a
//! visibility-strategy decision.
//!
//! The four rooms ([`crate::room::OpenRoom`], [`crate::aoi::AoiRoom`],
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
//! own `runner` / `player_entity` / `next_wire_id` fields (the group-key
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

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use bevy_ecs::prelude::{Changed, Entity, World, Without};
use bytes::{BufMut, Bytes, BytesMut};
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, Admission, Detach, ExpireTo, ResumeFound, TickCtx};
use gsb_ecs::{SystemCtx, SystemRunner};
use prost::encoding::varint::encode_varint;
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

/// Per-connection input sequence state (shared by all rooms; see
/// [`ingest`] and [`emit_private`]).
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
    let (x, y) = spawn_pos(conn, spawn_half);
    let wire = next_serial(next_wire_id);
    let entity = world
        .spawn((Position { x, y }, Speed(DEFAULT_SPEED), wire))
        .id();
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

/// `MOVE_TO` ingestion, shared by all rooms: decode the game message,
/// guard against stale actions (connection not in the room) and vanished
/// entities, enforce the per-connection input sequence rule (see below),
/// and write the [`MoveTarget`]. Anything else is ignored with a warning
/// (the op code is the router; undecodable payloads are a client bug,
/// not a reason to drop the connection).
///
/// **Input sequence rule** (the client prediction-reconciliation signal):
/// the client numbers its inputs (monotonic from 1 per session, `seq = 0`
/// = unnumbered legacy). The server processes a numbered action only
/// when it is *strictly newer* than this connection's high-water mark:
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
pub(crate) fn ingest(
    player_entity: &HashMap<PlayerId, Entity>,
    world: &mut World,
    actions: &mut Vec<Action>,
    input: &mut HashMap<PlayerId, InputState>,
) {
    for action in actions.drain(..) {
        if action.op != op::MOVE_TO {
            continue;
        }
        let Ok(msg) = <crate::game::MoveTo as Message>::decode(&action.payload[..]) else {
            tracing::warn!(?action.op, "undecodable MOVE_TO payload ignored");
            continue;
        };
        // Faz 2: actions are keyed by the STABLE player id the core's
        // binding stamped at ingest (bot-synthesized frames carry it
        // directly). A stale/unbound action drops right here — the same
        // silent-skip posture this path always had.
        let Some(entity) = player_entity.get(&action.player).copied() else {
            continue; // not in a room (stale action)
        };
        if world.get_entity(entity).is_err() {
            continue; // entity already gone
        }
        // The sequence rule (see the docs above). `or_default` is a
        // defensive fallback only: `on_join` inserts the session state.
        let st = input.entry(action.player).or_default();
        if msg.seq == 0 {
            // Legacy/unnumbered: process, never advance the mark.
        } else if msg.seq > st.hwm {
            st.hwm = msg.seq;
        } else {
            continue; // duplicate / reordered late: dropped (normal race)
        }
        world.entity_mut(entity).insert(MoveTarget {
            x: msg.x as f32,
            y: msg.y as f32,
        });
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
    let frame = crate::game::Private {
        payload: ack_up_to.map(|upto| {
            crate::game::private::Payload::Ack(crate::game::InputAck {
                processed_up_to: upto,
            })
        }),
        responses: responses
            .iter()
            .map(|r| crate::game::RpcResponse {
                id: r.id,
                ok: r.ok,
                op: r.op as u32,
                reason: r.reason.clone(),
                payload: r.payload.to_vec(),
            })
            .collect(),
    };
    frame
        .encode(out)
        .expect("protobuf encode into an in-memory buffer failed");
    true
}

/// Append this connection's queued RPC answers to a HAND-ENCODED
/// `Private` frame body (the AOI one-shot full path, which cannot go
/// through the generated type without re-encoding the snapshot): field
/// 3 (`responses`, tag 0x1A), one length-delimited `RpcResponse` per
/// entry. No allocation beyond the per-message length probe (responses
/// are rare — the steady-state tick has none).
pub(crate) fn append_responses(responses: &[gsb_core::rpc::RpcReply], out: &mut bytes::BytesMut) {
    for r in responses {
        let msg = crate::game::RpcResponse {
            id: r.id,
            ok: r.ok,
            op: r.op as u32,
            reason: r.reason.clone(),
            payload: r.payload.to_vec(),
        };
        out.put_u8(0x1A); // Private field 3 (responses), LEN
        prost::encoding::varint::encode_varint(msg.encoded_len() as u64, out);
        msg
            .encode(out)
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
/// the same pattern as [`MovementSystem`]); the stamp is idempotent and
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

/// The demo rooms' disconnect-park knob. `grace = 0` disables parking
/// entirely ([`Detach::Despawn`] — the byte-for-byte pre-reconnect
/// behavior), so an operator can turn the feature off without losing the
/// code path. The default lives at [`crate::DEFAULT_DISCONNECT_GRACE`]
/// (the one public constant the server config defaults from).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ParkPolicy {
    pub grace: Duration,
}

impl Default for ParkPolicy {
    fn default() -> Self {
        Self {
            grace: crate::DEFAULT_DISCONNECT_GRACE,
        }
    }
}

/// One entry of the demo park ledger (§4: "park defteri logic'te yaşar" —
/// the ledger lives in the game logic; the core only queries it through
/// [`crate::room::RoomLogic::resume_lookup`]).
///
/// Keyed by identity (the resume key: ticket player or local-auth name),
/// because THAT is what survives the transport death. The entry carries
/// the parked session's STABLE [`PlayerId`] (Faz 2): it is what
/// `resume_lookup` answers (the core finds its row by ONE lookup) and
/// what the bot synthesizes input under — stability across resume comes
/// exactly from riding this record (§14.2).
#[derive(Debug)]
pub(crate) struct ParkEntry {
    /// The parked player's stable identity.
    pub player: PlayerId,
    /// The parked bevy entity (kept alive by the hold).
    pub entity: Entity,
    /// The hold expired toward [`ExpireTo::AiHandover`]: the bot owns the
    /// entity now (`bot_fed` on the core row is this flag's core-side
    /// twin). Cleared when a resume consumes the entry.
    pub bot: bool,
}

/// The policy answer to a transport death (the `on_disconnect` hook
/// body shared by every single-world demo room): park the entity for
/// the configured grace toward AI handover, recording the ledger entry.
/// A connection we do not know (stale detach) cannot park anything.
///
/// Visibility note (§3.2): nothing else changes — the entity keeps its
/// components, group membership and slot, and the snapshot pass keeps
/// encoding it. Under the `all` strategy teammates simply keep seeing
/// the parked hero standing where its last command left it; a team-fog
/// strategy WOULD hide or mark that record here (a per-strategy filter
/// in its snapshot encoder), which is game-band content, not core
/// machinery — deliberately not implemented in the base.
pub(crate) fn park_on_disconnect(
    player_entity: &HashMap<PlayerId, Entity>,
    player: PlayerId,
    identity: &str,
    policy: &ParkPolicy,
    ledger: &mut HashMap<String, ParkEntry>,
) -> Detach {
    if policy.grace.is_zero() || identity.is_empty() {
        // Disabled (or nothing to resume with): the old semantics.
        return Detach::Despawn;
    }
    match player_entity.get(&player) {
        Some(&entity) => {
            ledger.insert(
                identity.to_string(),
                ParkEntry {
                    player,
                    entity,
                    bot: false,
                },
            );
            Detach::Hold {
                grace: Some(policy.grace),
                to: ExpireTo::AiHandover,
            }
        }
        // Stale detach (no entity of ours): fall through to despawn,
        // which the core turns into the ordinary no-op funnel.
        None => Detach::Despawn,
    }
}

/// The `on_detach_expired` hook body: an expired hold either releases the
/// identity (despawn arm — the core runs `on_leave`, we just forget the
/// entry so a later join is a transparent fresh join) or latches the bot
/// marker (AI arm — the entity keeps playing, driven by
/// [`synthesize_bot_moves`]).
pub(crate) fn park_on_expire(
    ledger: &mut HashMap<String, ParkEntry>,
    player: PlayerId,
    to: ExpireTo,
) {
    match to {
        ExpireTo::Despawn => ledger.retain(|_, e| e.player != player),
        ExpireTo::AiHandover => {
            for e in ledger.values_mut().filter(|e| e.player == player) {
                e.bot = true;
            }
        }
    }
}

/// The `resume_lookup` hook body: the ledger answers whether the identity
/// is parked. The answer is the parked session's STABLE [`PlayerId`] —
/// the key the core's own tables are keyed by, so the core finds its row
/// with one lookup and a resumed session keeps the same identity (Faz 2;
/// pre-Faz-2 this resolved the entity's wire serial for the core's scan
/// over the detached rows).
pub(crate) fn park_lookup(
    _world: &World,
    ledger: &HashMap<String, ParkEntry>,
    identity: &str,
) -> ResumeFound {
    match ledger.get(identity) {
        Some(entry) => ResumeFound::Held(entry.player),
        None => ResumeFound::Never,
    }
}

/// The `on_resume` hook body (Faz 2 shrink): consume the ledger entry
/// (the bot loses the entity; the human's numbered inputs take over).
/// With every table keyed by the stable [`PlayerId`] there is nothing
/// left to RE-KEY here — the pre-Faz-2 `conn_entity` rename is gone (the
/// player→entity mapping kept its key across the whole disconnect).
/// What remains is exactly what is session-scoped: the seq/ack reset of
/// DESIGN §14.2 (the resumed session numbers from 1; dropping the entry
/// makes `ingest`'s `or_default` mint a fresh one). Strategy-specific
/// per-session tables, where a room keeps any, are dropped by the room
/// itself before calling into here.
pub(crate) fn park_resume(
    ledger: &mut HashMap<String, ParkEntry>,
    input: &mut HashMap<PlayerId, InputState>,
    identity: &str,
    player: PlayerId,
) {
    ledger.remove(identity);
    input.remove(&player);
}

/// How often the demo bot picks a new wander target, in ticks (~1 s at
/// the default 30 Hz).
const BOT_WANDER_EVERY_TICKS: u64 = 30;

/// How far from its current position the bot may wander (world units) —
/// small enough that a reclaimed hero is roughly where its team left it.
const BOT_WANDER_RADIUS: f32 = 12.0;

/// Deterministic wander jitter for one bot round: an integer hash over
/// (position bits, round index) mapped into
/// `[-BOT_WANDER_RADIUS, +BOT_WANDER_RADIUS]²`. No RNG dependency, stable
/// for a given (where it stands, when asked) pair — the spec's "derive
/// from entity position + tick counter".
fn bot_jitter(x_bits: u32, y_bits: u32, round: u64) -> (f32, f32) {
    let mut z = (x_bits as u64)
        ^ (y_bits as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ round.wrapping_mul(0xD1B5_4A32_D192_ED03);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    let half = BOT_WANDER_RADIUS;
    let x = ((z & 0xFFFF) as f32 / 65535.0 - 0.5) * 2.0 * half;
    let y = ((z >> 16 & 0xFFFF) as f32 / 65535.0 - 0.5) * 2.0 * half;
    (x, y)
}

/// The demo bot (RECONNECT §9): synthesize MOVE_TO frames for every
/// bot-fed entity — handed in as `(its dead connection id, the entity)`
/// pairs by each room's `ingest` (the single-world rooms read them off
/// their ledger entries; the sharded room resolves its wire-keyed ledger
/// through its own tables first) — and push them INTO the tick's action
/// list, ahead of any wire actions. They are indistinguishable from
/// client frames: the ordinary [`ingest`] decodes them, applies the
/// sequence rule (seq = 0: unnumbered, never fights a human high-water
/// mark) and writes the real [`MoveTarget`] — which is the whole point:
/// the bot is an input source without a connection, exercising the REAL
/// movement system, not a parallel teleport path.
pub(crate) fn synthesize_bot_moves(
    bots: impl Iterator<Item = (PlayerId, Entity)>,
    world: &World,
    ctx: &TickCtx,
    actions: &mut Vec<Action>,
) {
    // One cadence gate for the whole tick: the bot acts on every Nth tick
    // only (the cheap common case short-circuits here).
    if !ctx.tick.is_multiple_of(BOT_WANDER_EVERY_TICKS) {
        return;
    }
    let round = ctx.tick / BOT_WANDER_EVERY_TICKS;
    for (player, entity) in bots {
        let Ok(he) = world.get_entity(entity) else {
            continue;
        };
        let Some(pos) = he.get::<Position>().copied() else {
            continue;
        };
        let (jx, jy) = bot_jitter(pos.x.to_bits(), pos.y.to_bits(), round);
        let msg = crate::game::MoveTo {
            x: (pos.x + jx) as i32,
            y: (pos.y + jy) as i32,
            seq: 0,
        };
        actions.push(Action {
            // Synthesized input has no transport session behind it; the
            // stable player id is what routes it.
            conn: ConnectionId(0),
            player,
            op: op::MOVE_TO,
            payload: msg.encode_to_vec().into(),
        });
    }
}

// ════════════════════════════════════════════════════════════════════════
// The shared CELL-DELTA machinery: the spatial visibility strategies run
// the same encoding engine, so it lives here once. Two rooms drive it —
// [`crate::aoi::AoiRoom`] (single world) and
// [`crate::sharded::ShardedSpatialRoom`] (the Faz B per-shard composite) —
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

/// A spatial cell of the world grid — the AOI group key. Cell indices
/// are the floor of (wire position / `cell_size`) — see the module docs
/// of either room ("Cells are computed from the WIRE position").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cell(pub i32, pub i32);

/// How many cells the visibility block extends in each direction from
/// the player's cell. `1` ⇒ a 3×3 block (the cell + its 8 ring-1
/// neighbors).
pub(crate) const RADIUS: i32 = 1;

/// The (dx, dy) offsets of the visibility block centered on a cell
/// (deterministic order: the assembly order of the group's packet).
pub(crate) const BLOCK_OFFSETS: [(i32, i32); 9] = [
    (-RADIUS, -RADIUS), (0, -RADIUS), (RADIUS, -RADIUS),
    (-RADIUS, 0), (0, 0), (RADIUS, 0),
    (-RADIUS, RADIUS), (0, RADIUS), (RADIUS, RADIUS),
];

/// The cell containing the WIRE (integer) position: `floor(x / cell_size)`
/// on the integer coordinates, so the client — which holds only wire
/// coordinates — computes the same cell.
#[inline]
pub(crate) fn cell_of(x: i32, y: i32, cell_size: f32) -> Cell {
    Cell(
        (x as f32 / cell_size).floor() as i32,
        (y as f32 / cell_size).floor() as i32,
    )
}

/// The snapshot header: `sequence` (field 1, varint) + the `delta`
/// flag (field 5; written only when true — a `false`/absent flag
/// means FULL, per `game.proto`).
pub(crate) fn write_snapshot_header(buf: &mut BytesMut, tick: u64, delta: bool) {
    buf.put_u8(0x08); // field 1 (sequence), varint
    encode_varint(tick, buf);
    if delta {
        buf.put_u8(0x28); // field 5 (delta), varint
        buf.put_u8(1);
    }
}

/// Encode `records` as `entities` entries (field 2, length-
/// delimited) — one pre-encoded piece, shareable by reference.
/// (Encoding straight into the buffer: no per-record allocation.)
pub(crate) fn encode_entity_records(records: &[(u64, i32, i32)]) -> Bytes {
    let mut out = BytesMut::new();
    for &(wire, x, y) in records {
        let rec = crate::game::EntityRecord {
            entity: wire,
            x,
            y,
        };
        out.put_u8(0x12); // field 2 (entities), length-delimited
        encode_varint(rec.encoded_len() as u64, &mut out);
        rec.encode(&mut out)
            .expect("protobuf encode into an in-memory buffer failed");
    }
    out.freeze()
}

/// Encode `exits` as `removed` entries (field 3, varint) — the
/// entity-exit piece of a delta.
pub(crate) fn encode_entity_exits(exits: &[u64]) -> Bytes {
    let mut out = BytesMut::new();
    for &wire in exits {
        out.put_u8(0x18); // field 3 (removed), varint
        encode_varint(wire, &mut out);
    }
    out.freeze()
}

/// Encode one `cell_exits` entry (field 4, length-delimited) for
/// `cell` — the single record that makes the client forget a whole
/// cell.
pub(crate) fn encode_cell_exit(cell: Cell) -> Bytes {
    let msg = crate::game::CellExit {
        x: cell.0,
        y: cell.1,
    };
    let mut out = BytesMut::new();
    out.put_u8(0x22); // field 4 (cell_exits), length-delimited
    encode_varint(msg.encoded_len() as u64, &mut out);
    msg.encode(&mut out)
        .expect("protobuf encode into an in-memory buffer failed");
    out.freeze()
}

/// The per-tick classification of one cell (a pure function of the
/// cell's change list and its occupancy baseline — identical for every
/// group that sees the cell).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CellFrag {
    /// Empty now, empty before, and no change recorded this tick:
    /// nothing for any group.
    Silent,
    /// Non-empty before, empty now: the group's packet carries one
    /// `CellExit` record for it (the client forgets the whole cell in
    /// one record).
    Exited,
    /// Empty before, non-empty now: no baseline exists for the cell —
    /// its FULL records go into the group's packet (upserts in a delta
    /// packet; full content in a fresh group's packet).
    Appeared,
    /// Non-empty before and now, content changed: the cell's delta
    /// piece (its change list: exits + updates).
    Delta,
}

/// One cell's changes this tick — the delta's source of truth: the
/// change list *is* the diff, so no per-cell content comparison is ever
/// run. Built incrementally from the dirty set / the borrowed diff;
/// persistent map, cleared in place each tick.
#[derive(Default)]
pub(crate) struct CellChanges {
    /// Records whose wire content changed, or that newly occupy the cell
    /// (wire id, wire x, wire y). Encoded as `entities` upserts (field
    /// 2) — except for an appeared cell, whose group gets the cell's
    /// FULL piece instead (the upserts would be redundant: the client
    /// has no baseline for a cell that was empty).
    pub updates: Vec<(u64, i32, i32)>,
    /// Wire ids that left the cell (a cell-to-cell move or a despawn).
    /// Encoded as `removed` (field 3) — except when the whole cell
    /// exited, in which case one `CellExit` record supersedes them.
    pub exits: Vec<u64>,
    /// The cell was empty at the end of the last tick (set at roll
    /// time, order-independently — see [`CellBook::roll`]).
    pub appeared: bool,
    /// The cell is empty now (and was not empty then) — same evaluation.
    pub exited: bool,
}

/// The per-tick member-event counters of one touched cell: the
/// order-independent group-birth arithmetic reconstructs the before-tick
/// member count from `now − in + out`, so a same-tick exit+entry into
/// the same cell cannot fake a birth.
#[derive(Default)]
pub(crate) struct TouchInfo {
    /// Member entities that entered this cell this tick (joins into it,
    /// cell crossings into it).
    pub member_in: u32,
    /// Member entities that left it (crossings out, leavers).
    pub member_out: u32,
}

/// The per-tick CONTENT bookkeeping of a cell-encoded delta broadcaster —
/// the current buckets, the change lists, and the occupancy/member
/// baselines the classification rolls from. Own entities enter through
/// [`Self::dirty_pass`] (bevy's write path is the structural dirty mark);
/// any other content source (the sharded composite's borrowed border
/// strip) enters through the same four record primitives with
/// `member = false`, so both sources share one arithmetic.
#[derive(Default)]
pub(crate) struct CellBook {
    /// The current buckets: `cell → (wire id → (x, y))` — the content of
    /// every cell, maintained incrementally. Invariant: after a full tick
    /// body (dirty pass + every external source + roll), the buckets
    /// equal the visible world's current content.
    pub buckets: HashMap<Cell, HashMap<u64, (i32, i32)>>,
    /// The cells that were occupied at the last roll: the appearance/
    /// exit baseline. Frozen while content mutates, rolled only by
    /// [`Self::roll`] against the final bucket state (order-
    /// independence — a same-tick exit+entry cannot flip either flag).
    pub prev_occupied: HashSet<Cell>,
    /// Each bucketed entity's cell at the end of the last pass (written
    /// by the dirty pass, read by it, by removal parking, and by the
    /// O(1) `group_of` lookups of the broadcast phase).
    pub last_cell: HashMap<Entity, Cell>,
    /// The member count of each cell (empty entries removed): the
    /// birth arithmetic's "now" input. Borrowed records are never
    /// members — they carry no connection on this side.
    pub member_counts: HashMap<Cell, u32>,
    /// The cells touched this tick with their member-event counters
    /// (persistent map, cleared in place each tick): the roll iterates
    /// exactly this map — O(movers), never O(cells).
    pub touched: HashMap<Cell, TouchInfo>,
    /// Each touched cell's change list for this tick (persistent map,
    /// cleared in place each tick): the delta's source of truth.
    pub cell_changes: HashMap<Cell, CellChanges>,
    /// Removals parked by the CONTROL phase (leaves, migrations-out):
    /// a despawn is not a component write, so the dirty query cannot see
    /// it — the entity, its wire id and its last cell are parked here and
    /// applied by [`Self::apply_removals`]. A join+leave within one tick
    /// parks nothing — the entity never made it into `last_cell`, hence
    /// never into the buckets.
    pub pending_removals: Vec<(Entity, u64, Cell)>,
    /// The member entities (maintained by the room on join/leave/migrate):
    /// the dirty loop's O(1) membership test.
    pub members: HashSet<Entity>,
    /// Cells with members now but none at the last roll: their groups
    /// are fresh and must emit a FULL packet on their first tick.
    pub born_groups: HashSet<Cell>,
}

impl CellBook {
    /// Clear the per-tick state (persistent containers, in place).
    pub(crate) fn begin_tick(&mut self) {
        self.cell_changes.clear();
        self.touched.clear();
        self.born_groups.clear();
    }

    #[inline]
    fn touch(&mut self, c: Cell) {
        self.touched.entry(c).or_default();
    }

    #[inline]
    fn member_event(&mut self, c: Cell, in_: bool) {
        let t = self.touched.entry(c).or_default();
        if in_ {
            t.member_in += 1;
        } else {
            t.member_out += 1;
        }
    }

    /// Primitive: a record NEWLY occupies `cell` (a join spawn, a fresh
    /// migration-in, a borrowed record entering the view). An upsert in
    /// the cell's change list.
    pub(crate) fn record_appearance(
        &mut self,
        wire: u64,
        x: i32,
        y: i32,
        cell: Cell,
        member: bool,
    ) {
        self.cell_changes
            .entry(cell)
            .or_default()
            .updates
            .push((wire, x, y));
        self.buckets.entry(cell).or_default().insert(wire, (x, y));
        self.touch(cell);
        if member {
            *self.member_counts.entry(cell).or_default() += 1;
            self.member_event(cell, true);
        }
    }

    /// Primitive: a record's wire content changed WITHIN `cell` (already
    /// known different — the quantization check belongs to the caller).
    pub(crate) fn record_update(&mut self, cell: Cell, wire: u64, x: i32, y: i32) {
        self.cell_changes
            .entry(cell)
            .or_default()
            .updates
            .push((wire, x, y));
        if let Some(b) = self.buckets.get_mut(&cell) {
            b.insert(wire, (x, y));
        }
        self.touch(cell);
    }

    /// Primitive: a record moved from `old` cell to `new` — an exit in
    /// the source, an upsert in the target (the packet passes fix the
    /// wire order: `removed` before `entities`).
    pub(crate) fn record_cross(
        &mut self,
        old: Cell,
        new: Cell,
        wire: u64,
        x: i32,
        y: i32,
        member: bool,
    ) {
        self.cell_changes.entry(old).or_default().exits.push(wire);
        if let Some(b) = self.buckets.get_mut(&old) {
            b.remove(&wire);
            if b.is_empty() {
                self.buckets.remove(&old);
            }
        }
        self.touch(old);
        self.cell_changes
            .entry(new)
            .or_default()
            .updates
            .push((wire, x, y));
        self.buckets.entry(new).or_default().insert(wire, (x, y));
        self.touch(new);
        if member {
            if let Some(n) = self.member_counts.get_mut(&old) {
                *n -= 1;
                if *n == 0 {
                    self.member_counts.remove(&old);
                }
            }
            *self.member_counts.entry(new).or_default() += 1;
            self.member_event(old, false);
            self.member_event(new, true);
        }
    }

    /// Primitive: a record LEFT `cell` without a tracked position write
    /// (a parked despawn removal, a borrowed record exiting the view).
    pub(crate) fn record_exit(&mut self, cell: Cell, wire: u64, member: bool) {
        self.cell_changes.entry(cell).or_default().exits.push(wire);
        if let Some(b) = self.buckets.get_mut(&cell) {
            b.remove(&wire);
            if b.is_empty() {
                self.buckets.remove(&cell);
            }
        }
        self.touch(cell);
        if member {
            if let Some(n) = self.member_counts.get_mut(&cell) {
                *n -= 1;
                if *n == 0 {
                    self.member_counts.remove(&cell);
                }
            }
            self.member_event(cell, false);
        }
    }

    /// The own-entity dirty pass: bevy's change detection flags every
    /// `Position` write — by any writer, through any API — so the dirty
    /// mark lives in bevy's write path itself and no writer can forget
    /// it. Only changed entities are visited: per-tick work is
    /// proportional to movers, not to the entity count.
    ///
    /// Quantization: wire positions are i32 truncations of f32 motion —
    /// a same-cell move whose wire position did not change records
    /// nothing (the cell can still classify `Silent`), so the stream is
    /// content-identical to a diff-based design.
    pub(crate) fn dirty_pass(&mut self, world: &mut World, cell_size: f32) {
        let mut query =
            world.query_filtered::<(Entity, &WireId, &Position), Changed<Position>>();
        for (entity, wire_id, pos) in query.iter(world) {
            let wire = wire_id.get();
            let (x, y) = (pos.x as i32, pos.y as i32);
            let new_cell = cell_of(x, y, cell_size);
            let is_member = self.members.contains(&entity);
            match self.last_cell.get(&entity).copied() {
                None => {
                    // New this tick (a join, a migration-in, or a spawn
                    // between passes): an upsert in its cell.
                    self.record_appearance(wire, x, y, new_cell, is_member);
                    self.last_cell.insert(entity, new_cell);
                }
                Some(old) if old == new_cell => {
                    // Moved inside its cell — a record only when the wire
                    // content actually changed.
                    let changed = self
                        .buckets
                        .get(&old)
                        .and_then(|b| b.get(&wire))
                        .is_none_or(|&(px, py)| px != x || py != y);
                    if changed {
                        self.record_update(new_cell, wire, x, y);
                    }
                }
                Some(old) => {
                    self.record_cross(old, new_cell, wire, x, y, is_member);
                    self.last_cell.insert(entity, new_cell);
                }
            }
        }
    }

    /// Apply the removals parked during the CONTROL phase (despawns are
    /// invisible to the change query — module docs of the rooms).
    pub(crate) fn apply_removals(&mut self) {
        for (entity, wire, cell) in std::mem::take(&mut self.pending_removals) {
            self.last_cell.remove(&entity);
            // A parked removal is always a member's (connections own the
            // despawned entities); the join+leave-within-one-tick case
            // never parked a removal, so there is no count to undo for it.
            self.record_exit(cell, wire, /*member=*/ true);
        }
    }

    /// The per-cell flags, the group births, and the occupancy roll —
    /// all order-independent: `prev_occupied` was frozen while content
    /// mutated and is rolled here against the FINAL bucket state, and
    /// the member arithmetic reconstructs the before-tick count from the
    /// net events (`now − in + out`). Runs after EVERY content source of
    /// the tick has landed — on the sharded composite that includes the
    /// borrowed-strip integration, which is why the roll cannot simply
    /// sit at the end of `update` there.
    pub(crate) fn roll(&mut self) {
        for (c, t) in self.touched.iter() {
            let occupied_now = self.buckets.contains_key(c);
            let occupied_prev = self.prev_occupied.contains(c);
            let ch = self
                .cell_changes
                .get_mut(c)
                .expect("a touched cell has a change entry");
            ch.appeared = occupied_now && !occupied_prev;
            ch.exited = occupied_prev && !occupied_now;
            let now = self.member_counts.get(c).copied().unwrap_or(0);
            let before = now.wrapping_sub(t.member_in).wrapping_add(t.member_out);
            debug_assert!(
                before.saturating_add(t.member_in) >= t.member_out,
                "member count went negative for {c:?}"
            );
            if before == 0 && now > 0 {
                self.born_groups.insert(*c);
            }
            if occupied_now {
                self.prev_occupied.insert(*c);
            } else {
                self.prev_occupied.remove(c);
            }
        }
    }
}

/// The per-tick encoded-piece caches of a cell-delta broadcaster: every
/// piece is computed lazily ONCE per (cell, kind) per tick and shared as
/// frozen `Bytes` by reference with every group that needs it — the
/// "encode once, share the bytes" spine at cell granularity. Cleared in
/// place at each tick start.
#[derive(Default)]
pub(crate) struct CellPieces {
    /// Each cell's encoded FULL records (the `entities` entries, field 2)
    /// of its current content.
    full_pieces: HashMap<Cell, Bytes>,
    /// Each changed cell's encoded delta: `(removed piece, entities piece)`
    /// assembled from the cell's change list.
    delta_pieces: HashMap<Cell, (Option<Bytes>, Bytes)>,
    /// Each exited cell's encoded `cell_exits` entry (field 4).
    exit_markers: HashMap<Cell, Bytes>,
    /// The assembled FULL snapshot of a cell's 3×3 view (header + the
    /// full pieces): shared between the fresh-group packet, the keep-
    /// alive full, and the one-shot private full.
    full_view: HashMap<Cell, Bytes>,
    /// The scratch behind `full_view` (reused across assemblies;
    /// `split_to` hands out zero-copy views — no per-assembly allocation).
    scratch: BytesMut,
    /// The per-tick classification cache: the per-tick change list does
    /// not change during the tick, so the first caller's classification
    /// is valid for every later group and every later pass — including
    /// the negative answer (`Silent`), which is a hash miss on the change
    /// list, not a scan.
    frag_cache: HashMap<Cell, CellFrag>,
    /// Entity records encoded into pieces so far this tick (the overlap
    /// measurement: ~E per tick — one encoding per entity, in its own
    /// cell's piece). Read/reset via [`Self::take_encoded`].
    encoded: u64,
}

impl CellPieces {
    /// Clear the per-tick caches (persistent containers, in place).
    pub(crate) fn begin_tick(&mut self) {
        self.full_pieces.clear();
        self.delta_pieces.clear();
        self.exit_markers.clear();
        self.full_view.clear();
        self.frag_cache.clear();
        self.encoded = 0;
    }

    /// Records encoded so far this tick (polled once per step via
    /// `GameLogic::encoded_records`).
    pub(crate) fn take_encoded(&mut self) -> u64 {
        std::mem::take(&mut self.encoded)
    }

    /// The per-tick classification of `c` (see [`CellFrag`]) — memoized
    /// per cell per tick, negative answer included.
    pub(crate) fn classify(
        &mut self,
        changes: &HashMap<Cell, CellChanges>,
        c: &Cell,
    ) -> CellFrag {
        if let Some(&frag) = self.frag_cache.get(c) {
            return frag;
        }
        let frag = match changes.get(c) {
            None => CellFrag::Silent,
            Some(ch) if ch.appeared => CellFrag::Appeared,
            Some(ch) if ch.exited => CellFrag::Exited,
            Some(_) => CellFrag::Delta,
        };
        self.frag_cache.insert(*c, frag);
        frag
    }

    /// The cell's FULL piece (its complete current content, encoded once
    /// per tick; `None` for an empty cell).
    pub(crate) fn full_piece(
        &mut self,
        buckets: &HashMap<Cell, HashMap<u64, (i32, i32)>>,
        c: &Cell,
    ) -> Option<Bytes> {
        if !self.full_pieces.contains_key(c)
            && let Some(bucket) = buckets.get(c)
        {
            let records: Vec<(u64, i32, i32)> =
                bucket.iter().map(|(&w, &(x, y))| (w, x, y)).collect();
            self.encoded += records.len() as u64;
            self.full_pieces.insert(*c, encode_entity_records(&records));
        }
        self.full_pieces.get(c).cloned()
    }

    /// A changed cell's delta piece: `(exits, updates)` assembled from
    /// the cell's change list — the change list *is* the diff, so no
    /// per-cell content comparison is ever run (encoded once per tick).
    /// `None` when the cell has no change list (the caller classifies
    /// first; such a cell is silent for the tick).
    pub(crate) fn delta_piece(
        &mut self,
        changes: &HashMap<Cell, CellChanges>,
        c: &Cell,
    ) -> Option<&(Option<Bytes>, Bytes)> {
        if !self.delta_pieces.contains_key(c)
            && let Some(ch) = changes.get(c)
            && (!ch.exits.is_empty() || !ch.updates.is_empty())
        {
            self.encoded += ch.updates.len() as u64;
            self.delta_pieces.insert(
                *c,
                (
                    (!ch.exits.is_empty()).then(|| encode_entity_exits(&ch.exits)),
                    encode_entity_records(&ch.updates),
                ),
            );
        }
        self.delta_pieces.get(c)
    }

    /// One `CellExit` marker for an exited cell (encoded once per tick).
    pub(crate) fn exit_marker(&mut self, c: &Cell) -> Bytes {
        if !self.exit_markers.contains_key(c) {
            self.exit_markers.insert(*c, encode_cell_exit(*c));
        }
        self.exit_markers.get(c).expect("inserted above").clone()
    }

    /// The assembled FULL snapshot of `cell`'s 3×3 view (header with
    /// `delta = false` + the full pieces of every non-empty cell) —
    /// computed once per tick and shared.
    pub(crate) fn full_view(
        &mut self,
        buckets: &HashMap<Cell, HashMap<u64, (i32, i32)>>,
        tick: u64,
        cell: &Cell,
    ) -> Bytes {
        if let Some(bytes) = self.full_view.get(cell) {
            return bytes.clone();
        }
        self.scratch.clear();
        write_snapshot_header(&mut self.scratch, tick, false);
        for (dx, dy) in BLOCK_OFFSETS {
            let c = Cell(cell.0 + dx, cell.1 + dy);
            if let Some(piece) = self.full_piece(buckets, &c) {
                self.scratch.extend_from_slice(&piece);
            }
        }
        let bytes = self.scratch.split_to(self.scratch.len()).freeze();
        self.full_view.insert(*cell, bytes.clone());
        bytes
    }
}

/// Assemble one group's packet from this tick's pieces: a FRESH group
/// (it had no members at the last roll) gets a FULL packet of its 3×3
/// view; an ESTABLISHED group gets a DELTA packet — exits (field 3),
/// then cell exits (field 4), then updates/appeared fulls (field 2) —
/// or nothing (`false`) when the whole block is silent. Marks a fresh
/// group's full in `group_full_emitted` (the batch-ordering signal the
/// private frame uses to skip its one-shot).
pub(crate) fn assemble_group_packet(
    pieces: &mut CellPieces,
    book: &CellBook,
    cell: &Cell,
    group_full_emitted: &mut HashSet<Cell>,
    tick: u64,
    out: &mut BytesMut,
) -> bool {
    if book.born_groups.contains(cell) {
        // Fresh group: every member is new to this view — the first
        // packet is a full (delta=false), so the members end the tick
        // baselined (the invariant starts from here).
        group_full_emitted.insert(*cell);
        let full = pieces.full_view(&book.buckets, tick, cell);
        out.extend_from_slice(&full);
        return true;
    }
    // Established group: emit a delta only when at least one cell in the
    // block has something to say (silence writes no bytes).
    let mut any = false;
    for (dx, dy) in BLOCK_OFFSETS {
        let c = Cell(cell.0 + dx, cell.1 + dy);
        if pieces.classify(&book.cell_changes, &c) != CellFrag::Silent {
            any = true;
        }
    }
    if !any {
        return false;
    }
    write_snapshot_header(out, tick, true);
    // Pass 1: entity exits of every cell — exits before updates, so a
    // cell-to-cell move is exited from its source before it is updated
    // in its target.
    for (dx, dy) in BLOCK_OFFSETS {
        let c = Cell(cell.0 + dx, cell.1 + dy);
        if pieces.classify(&book.cell_changes, &c) == CellFrag::Delta
            && let Some((exits, _)) = pieces.delta_piece(&book.cell_changes, &c)
            && let Some(e) = exits
        {
            out.extend_from_slice(e);
        }
    }
    // Pass 2: cell exits — one record per cell that became empty.
    for (dx, dy) in BLOCK_OFFSETS {
        let c = Cell(cell.0 + dx, cell.1 + dy);
        if pieces.classify(&book.cell_changes, &c) == CellFrag::Exited {
            out.extend_from_slice(&pieces.exit_marker(&c));
        }
    }
    // Pass 3: updates — delta pieces' changed records, and appeared
    // cells' full records (upserts — no baseline exists for a cell that
    // was empty).
    for (dx, dy) in BLOCK_OFFSETS {
        let c = Cell(cell.0 + dx, cell.1 + dy);
        match pieces.classify(&book.cell_changes, &c) {
            CellFrag::Delta => {
                if let Some((_, updates)) = pieces.delta_piece(&book.cell_changes, &c) {
                    out.extend_from_slice(updates);
                }
            }
            CellFrag::Appeared => {
                if let Some(piece) = pieces.full_piece(&book.buckets, &c) {
                    out.extend_from_slice(&piece);
                }
            }
            _ => {}
        }
    }
    true
}
