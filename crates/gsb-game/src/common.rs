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

use std::collections::HashMap;
use std::time::Duration;

use bevy_ecs::prelude::{Entity, World, Without};
use bytes::BufMut;
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, Admission, Detach, ExpireTo, ResumeFound, TickCtx};
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

/// Per-connection input sequence state (shared by all rooms; see
/// [`ingest`] and [`emit_ack`]).
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

/// Emit this connection's pending input acknowledgment as the `Private`
/// frame payload into `out` (returning `true` when a frame was
/// produced), advancing `acked`. Called from each room's `private`
/// seam — the per-tick, per-connection slot of the batch — so the ack
/// rides the SAME delivery as that tick's group snapshot (no extra send,
/// no extra await: the tick body stays synchronous).
///
/// The ack is emitted only on the ticks in which `hwm` advanced past the
/// last ack (not every tick): a few bytes per advanced tick, zero
/// otherwise. The mark is monotonic and can never exceed the highest
/// processed seq (it IS that seq), so the client's reconciliation
/// ("everything up to N is applied; re-apply N+1, N+2, …") is sound.
pub(crate) fn emit_ack(
    input: &mut HashMap<PlayerId, InputState>,
    player: PlayerId,
    out: &mut bytes::BytesMut,
) -> bool {
    emit_private(input, player, &[], out)
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
/// exactly once (the room's queue is drained per tick — see the core's
/// fan-out) and the logic decides their order (arrival order within the
/// tick, per the `gsb_core::rpc` contract).
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
