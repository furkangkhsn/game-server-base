//! Sharded rooms: one logical room running on N **shard actors**.
//!
//! ## Why this exists
//!
//! A room's tick is a single serial path (CONTROL → READ → CONVERT →
//! SYSTEMS → BROADCAST) owned by one actor. The measured single-room wall
//! (9–10k members, `docs/ROADMAP.md`) is that serial path: the server's
//! CPU pool sits at ~25 % while one room actor cannot finish a step inside
//! the tick budget. Sharding splits one logical room's world into N
//! disjoint spatial regions, each owned by its own shard actor on its own
//! task, all subscribed to the same global ticker. Parallelism comes from
//! the actor model (N independent tasks), not a thread pool: every shard
//! is the exclusive owner of its world, its connection table, and its
//! group table — there is no shared mutable state and no lock.
//!
//! The registry spawns the N shards for one logical `RoomId`, routes each
//! join to the shard that owns the joiner's spawn point (a pure
//! `home_shard` function the factory supplies), and never awaits a shard
//! (its only interaction is channel sends — the same discipline as rooms).
//!
//! ## The tick body (synchronous, with the Faz 3 RPC phases)
//!
//! ```text
//! Phase 0   │  CONTROL:  drain the shard channel: Join / Leave / Migrate /
//!           │            Border / Shutdown (try_recv — non-blocking)
//! Phase 0b  │  RPC:      drain worker completions (try_recv), reconcile
//!           │            against the pending set; sweep expired pending
//!           │            requests (timeout answers) — see `crate::rpc`
//! Phase 0c  │  DETACH:   detach-hold sweep (the room's mirror)
//! Phase 1   │  READ:     pull actions from each connection's channel
//!           │            (same bounded pull as the room)
//! Phase 1.5 │  BINDING:  translate each action's conn → PlayerId
//! Phase 2a  │  SPLIT:    carve the base-band RPC_REQ envelopes out of the
//!           │            pulled actions (in order — all fire-and-forget
//!           │            actions of the tick ingest BEFORE any request is
//!           │            handed to `handle_request`)
//! Phase 2b  │  CONVERT:  actions → component writes (ShardLogic::ingest)
//! Phase 2c  │  REQUESTS: correlated requests → Reply/Reject/External;
//!           │            an External registers pending (caps checked) and
//!           │            spawns a worker task reporting to the completion
//!           │            channel of 0b
//! Phase 3   │  SYSTEMS:  run the ordered game systems (ShardLogic)
//! Phase 4   │  MIGRATE:  despawn the entities marked out last tick; then,
//!           │            for each neighbor, collect the entities that moved
//!           │            into the neighbor's region and send them over
//!           │            (full state + the player's moved channels)
//! Phase 5   │  BORDER:   export this shard's boundary entities (full
//!           │            state, idempotent) to every neighbor
//! Phase 6   │  BROADCAST: one snapshot per group, including the borrowed
//!           │            boundary entities, plus each connection's private
//!           │            frame carrying this tick's queued RPC answers
//! ```
//!
//! ## Migration protocol (exactly-once ownership per tick index)
//!
//! An entity belongs to exactly one shard's world per **tick index**. The
//! protocol:
//!
//! - tick N, shard A (phase 4): A notices entity e's position is inside
//!   neighbor B's region. A sends `Migrate { at_tick: N, wire, state, … }`
//!   to B (full state, including e's wire identity) and marks e as
//!   "out at N+1". e stays in A's world for the rest of tick N.
//! - tick N+1, shard A (phase 4): A despawns e (the mark fired).
//! - tick N+1, shard B (phase 0): B receives the message and spawns e with
//!   its full state **before** B's step N+1.
//!
//! So at tick index N e is in A only; at N+1 in B only. A's tick-N
//! snapshot still carries e (it is A's entity then); B's clients first
//! see e at N+1. **This is the accepted one-tick alignment**: A's tick-N
//! data takes effect on B at B's tick N+1 — a player crossing a boundary
//! appears on the other side one tick later. The send is a synchronous
//! `try_send` into B's bounded channel, so under normal delivery the
//! message is queued well before B's tick N+1 CONTROL drains it. The
//! degradation boundary: if A *lags by a full tick* at send time (it is
//! already behind the global ticker), B may pick the message up one tick
//! late and e is absent from every shard for exactly one tick — a bounded
//! blink, never a duplicate, and the wire identity stays unique.
//!
//! **In-flight input** moves with the connection: the player's outbound
//! channel and action inbox are *moved* inside the `Migrate` message
//! (ownership transfer by channel move — the connection actor is unaware;
//! it keeps writing to the same channel, whose receiver now lives in B).
//! Actions queued before the move stay in the same channel object and are
//! pulled by B on its next tick — they are never lost or mis-routed.
//!
//! **The leave/migration race (join epochs).** A leave and a migration of
//! the same connection race in both directions; without a guard the losing
//! shard can resurrect the connection (a `Migrate` arriving after the
//! leave re-inserts it). The guard is a **join epoch**: the connection's
//! dispatcher task hands out a per-connection monotonic epoch (one per
//! `Join` op), and every `Join`/`Leave`/`Migrate` carries the epoch of
//! the join it belongs to. Each shard keeps `conn_epoch: the highest
//! epoch it has seen per connection`; a `Migrate` is accepted only while
//! its epoch is *newer* than the shard's entry (a processed leave of that
//! join records the same epoch as a tombstone, so a late `Migrate` of the
//! dead join is dropped). A fresh join carries a strictly newer epoch, so
//! it is always accepted. The entity-id guard on `Leave` (stale leaves
//! cannot despawn a re-joined entity) composes with this: the two guards
//! together make the leave/migration race deterministic in both orderings.
//!
//! **Tombstone lifetime.** A leave's tombstone is not kept forever — that
//! would grow with connection churn. It outlives its leave by
//! `TOMBSTONE_TTL_TICKS` ticks and is swept lazily on the
//! `TOMBSTONE_SWEEP_EVERY_TICKS` cadence in CONTROL. Expiry cannot reopen
//! the race: a `Migrate` is generated only while its entity is alive
//! (leave-processing despawns the entity) and travels in a bounded
//! per-shard FIFO inbox that every CONTROL phase drains, so a `Migrate`
//! processed after its tombstone expired must have sat queued for more
//! than TTL ticks after its leave — meaning the shard itself stalled far
//! beyond any healthy operating point, outside the degradation envelope
//! this protocol already assumes (the one-tick alignment, the bounded
//! blink above). Within the envelope, no genuinely racing `Migrate` can
//! still be in flight when its tombstone expires.
//!
//! ## Boundary visibility (borrowed entities)
//!
//! A player on a shard boundary must see entities in the neighboring
//! shard — otherwise enemies vanish at the line. Every tick (phase 5) each
//! shard exports its **boundary entities** (game-defined: the demo exports
//! the entities within one border width of its region edges) to all
//! neighbors as a `Border` exchange; each shard keeps a persistent
//! per-neighbor view of the borrowed records and includes them in
//! **every** group's snapshot (phase 6 passes them to
//! `GameLogic::snapshot`). The exchange is a **seq-stamped delta**
//! (`docs/CROSS-SHARD.md` §6.4, the four-pin contract): the sender diffs
//! the strip against what each neighbor last accepted and ships only
//! upserts + explicit exits; a Full (whole strip) is sent on first
//! contact, on any continuity break, and on a low-frequency periodic
//! cadence.
//!
//! **Why the four pins** (each one exists because its absence has a named
//! failure mode): (1) *sequence stamps* turn a lost exchange into a
//! DETECTED event instead of silent divergence — the receiver demands an
//! exact match with its expected value and otherwise rejects wholesale,
//! never half-applies; (2) *explicit exit records* keep departed entities
//! from ghosting forever — a delta carries no implicit "everything not
//! mentioned still exists" contract the way a wholesale replacement does;
//! (3) *three resync triggers* (receiver-detected seq gap → request;
//! neighbor rebuild → the fresh incarnation leads with a Full because its
//! sender state starts empty; periodic Full every 256 ticks → the sigorta
//! that bounds any residual divergence's lifetime); (4) *migration
//! interaction* stays natural: a crossing shows up as an exit in the old
//! side's delta and an upsert in the new side's, and the own-wins filter
//! (below) swallows the crossing tick's double view unchanged. A send
//! failure (bounded `try_send`) is answered sender-side by flagging that
//! neighbor for a Full next tick — self-healing within ONE tick rather
//! than waiting out the cadence, because a dropped DELTA diverges until
//! healed where a dropped full self-healed by luck of idempotence.
//!
//! **Wire-identity interaction:** borrowed records carry the *neighbor's*
//! wire ids. The ranges are disjoint (below), so a client's view — own
//! shard's entities plus the borrowed boundary set — can never contain
//! the same id for two different entities, and the snapshot's "no
//! change" ledger is a plain union map over wire ids. One subtlety the
//! core enforces: an entity that just crossed INTO this shard appears as
//! its own record AND (for one tick) as the neighbor's stale borrowed
//! copy of itself — the actor filters the borrowed set against
//! [`GameLogic::own_wires`] so the own (fresh) record wins and the
//! snapshot never lists one entity twice.
//!
//! ## The ShardLink seam (`docs/DISTRIBUTED.md` §3)
//!
//! Everything this actor exchanges with a NEIGHBOR crosses the
//! crate-private [`ShardLink`] trait: `send` (best-effort — a full or
//! dead link refuses and hands the message back) and `drain` (FIFO,
//! non-blocking). Today every link is an [`InProcLink`] around exactly
//! the bounded mpsc halves the registry already wired, which is why
//! capacities, ordering and drop timing are bit-for-bit the pre-seam
//! channel behavior. The seam exists so the future UDS/TCP links (§10
//! triggers) become NEW TYPES behind the same two methods — the tick
//! body never learns what a link rides on. Send is best-effort BY
//! DESIGN: delivery classes and healing rules live in the message
//! semantics (§4), not in the link. Registry-originated control shares
//! the shards' inboxes (one bounded FIFO per shard — see
//! `registry.rs`), so the inbound side is modeled as ONE link over that
//! shared inbox rather than per-neighbor receive ends; splitting a
//! dedicated neighbor queue out would be a topology change (a second
//! interleaving for the protocol to reason about), not a refactor.
//!
//! ## Wire identity (range partitioning)
//!
//! Today a room mints wire ids from one monotonic counter; the invariant
//! is "two different entities never share a wire id over the room's
//! lifetime". With N shards a *shared allocator* (a round-trip message per
//! spawn) is incompatible with the architecture: spawns happen inside the
//! tick body, which is synchronous (no await), and an allocator actor
//! would add a shared component to a deliberately shared-state-free design
//! (a pre-fetched batch of ids is range partitioning with extra steps).
//! So each shard owns a **disjoint range**: shard i mints
//! `i * SHARD_SERIAL_RANGE + 1 … (i+1) * SHARD_SERIAL_RANGE`. No message,
//! no shared state, no await — and the invariant holds by construction
//! (the ranges are disjoint over the room's whole lifetime).
//!
//! Consequences, stated plainly:
//!
//! - A migrated entity **keeps its wire id** (the id is part of the
//!   migrated state): to a client it is the same entity that crossed the
//!   boundary, which is exactly what the id's purpose is to express.
//! - The id space is finite per shard (`SHARD_SERIAL_RANGE` = 2^20
//!   identities ≈ 100× the measured 10k wall in re-join churn per shard);
//!   a shard that exhausts its range refuses new joins with `RoomFull`
//!   (logged; structurally unreachable at the default).
//! - The varint cost: shard i's ids start at `i * 2^20`; at N ≤ 8 every
//!   id stays ≤ 2^23 (≤ 4-byte varint — the same as today's churn at
//!   scale), and even at N = 16 the offset stays under 2^25.
//!
//! ## Connection ownership
//!
//! "The shard owning a connection" = the shard whose `conns` table holds
//! it. Ownership transfers inside the `Migrate` message (the `out`
//! channel and the action `Inbox` move — no shared state, no registry
//! involvement, the connection actor never notices). A `Leave` is
//! **broadcast to all shards** of the room: exactly one shard (the owner)
//! matches the entity-id guard and despawns; the others no-op. The
//! alternative (the registry tracking the current owning shard per
//! connection and routing the leave there) needs a new registry message
//! per migration plus a lost-update failure mode (the bounded metrics
//! channel can drop); the broadcast costs N−1 no-op messages on a
//! low-frequency event (leave churn) and is race-free by construction.
//!
//! ## Capacity
//!
//! The room's membership cap (`RoomConfig::max_players`) is enforced
//! **in the registry** for sharded rooms: the registry is the only actor
//! that sees every join (a shard cannot count the room without shared
//! state), so it keeps a per-room member counter and rejects the join
//! *before* dispatch when the cap is reached (`RoomFull`, the same gentle
//! `ERROR 8` path as a single room).
//!
//! ## Metrics identity
//!
//! The collector keys room reports by the sample's room id, so each shard
//! reports under a derived id `room << 16 | index` (the sub-space above
//! real room ids; the load generator aggregates the shards of one logical
//! room).
//!
//! ## Shard-RPC and match-result (the Faz 3 promotion)
//!
//! Each shard runs the full `crate::rpc` machinery with the SAME contract
//! as the room actor — pending set + timeout sweep in CONTROL (0b),
//! completion channel drained non-blockingly, answers queued into the
//! per-connection private frame, caps from the shared `RoomConfig` fields.
//! Three shapes differ from the single-room actor, resolved here:
//!
//! - **Pending state is CONN-keyed** (`pending` / `queued` keyed by the
//!   transport session), exactly like the room's. The alternative —
//!   keying by the stable [`PlayerId`] — was rejected: it would CHANGE
//!   the RECONNECT §11 semantics ("a resumed session never inherits its
//!   dead session's in-flight work"). A session that dies (detach, leave,
//!   rejoin) drops its pending budget with the binding row; a resumed
//!   player starts with a fresh budget on a fresh conn key, which is the
//!   documented room behavior shards must mirror. Correlation ids are
//!   client-assigned PER CONNECTION anyway, so a player-keyed table would
//!   mix two sessions' id spaces under one key.
//! - **Migration drops the migrating session's request state** at
//!   migrate-out (the same §11 posture as detach): the worker future was
//!   spawned by the SENDING shard and reports to ITS completion channel;
//!   carrying pending entries across would need worker-report forwarding
//!   between actors (new machinery for zero client-visible gain — the
//!   request was already answered-with-silence by the move). The stale
//!   report lands late on the sending shard and is counted
//!   `requests_late`; the player re-requests on the receiving shard.
//! - **match_result fires per shard at that shard's teardown**: each
//!   shard reports through the shared sink under the same logical room id,
//!   so ONE logical room yields one payload PER SHARD (the platform
//!   adapter concatenates/filters). Nothing was added to the wire or to
//!   [`crate::registry::MatchResult`]. Rejected alternative: suppressing
//!   match_result for sharded rooms entirely (an explicit NOT-DONE) —
//!   rejected because the seam already exists end to end and per-shard
//!   final state is strictly more information than none; the adapter-side
//!   aggregation is trivial (filter by room, concatenate payloads).

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::fmt::Debug;
use std::hash::Hash;
use std::time::Instant;

use tokio::sync::{broadcast, mpsc, oneshot};
use prost::Message;
use tracing::{debug, info, warn};

use crate::channel::{FrameBatch, Inbox, Mailbox};
use crate::error::CoreError;
use crate::id::{ConnectionId, EntityId, PlayerId, RoomId};
use crate::metrics::{MetricsEvent, RoomSample, hist_index};
use crate::room::{
    Action, Detach, ExpireTo, GameLogic, GroupState, ResumeFound, RoomConn, RoomConfig,
    RoomCounters, TickCtx,
};
use crate::rpc::{Completion, PendingRequest, RpcReply, RpcRequest, RPC_REQ_OP, TIMEOUT_REASON};
use crate::ticker::TickInfo;

/// Identities per shard in the wire-id range partitioning (see module
/// docs, "Wire identity"): 2^20 ≈ 100× the measured 10k single-room wall
/// in per-shard lifetime spawn churn.
pub const SHARD_SERIAL_RANGE: u64 = 1 << 20;

/// A leave tombstone outlives the leave that wrote it by this many
/// ticks, then becomes sweepable (see `conn_tombstone`). Hardcoded on
/// purpose: this is a CORRECTNESS parameter of the leave/migration race
/// gate, not an operator tuning knob — its safe value derives from the
/// protocol's own degradation envelope (bounded FIFO residence), not
/// from a deployment's taste.
const TOMBSTONE_TTL_TICKS: u64 = 256;

/// The CONTROL phase sweeps expired tombstones every this many ticks
/// (lazily: no timer, no extra await — the sweep rides the tick the
/// shard is already running). With the TTL above, the table then holds
/// at most "the leaves of the last TTL window", so the O(n) `retain`
/// runs over a small set and amortizes to noise. Also hardcoded: it is
/// the other half of the same correctness contract.
const TOMBSTONE_SWEEP_EVERY_TICKS: u64 = 512;

/// One neighbor's boundary entity, as included in this shard's snapshots:
/// the CORE-MANAGED identity envelope (`wire` — minted from the room's
/// range-partitioned counters, deduplicated by the own-wins filter,
/// exited by the delta protocol) around a LOGIC-OWNED payload (`state`).
///
/// Why the split lives here: the seam's identity vocabulary (wire ids)
/// is the core's — every protocol mechanism keys off it — but WHAT an
/// entity must carry across the seam is the game's decision
/// (`docs/TRAIT-ARCHITECTURE.md`: state AND encoding belong to the
/// logic). A position-only game keeps the payload minimal; a combat or
/// prediction game extends it (velocity, facing, hp snapshot) without
/// touching this crate. Serialization at process-boundary links is the
/// future work of `docs/DISTRIBUTED.md` §4b — the codec will belong to
/// the logic because the payload type already does.
#[derive(Debug, Clone, PartialEq)]
pub struct BorderRecord<S> {
    /// The entity's wire identity (core vocabulary — see the module docs,
    /// "Wire identity").
    pub wire: u64,
    /// The game-defined strip payload, opaque to the core.
    pub state: S,
}

/// One neighbor's boundary update (CROSS-SHARD §6.4 pin 1–2): either the
/// COMPLETE strip (`Full` — bootstrap, resync, the periodic sigorta) or the
/// difference against what that neighbor last accepted from us (`Delta`:
/// upserts for new/changed records plus EXPLICIT exits for wire ids that
/// left the strip — without exits a ghost would persist forever, because a
/// delta carries no implicit "everything else is unchanged AND STILL THERE"
/// contract the way a full replacement does).
///
/// `seq` is the SENDER-side per-neighbor monotonic sequence (advanced once
/// per exchange actually queued): the receiver tracks the expected value
/// and treats any mismatch as a lost exchange, triggering a resync (pin 3a)
/// instead of silently diverging.
#[derive(Debug)]
pub enum BorderExchange<S> {
    /// The complete boundary strip: the receiver replaces its whole view
    /// for this neighbor and re-baselines its expected sequence. Accepted
    /// at ANY time — this is what makes a rebuilt shard's recovery
    /// automatic (its fresh incarnation restarts the sequence and always
    /// leads with a Full) without extra rebuild-notification wiring
    /// (§6.4 pin 3b).
    Full {
        seq: u64,
        /// The tick index the set was sampled at (diagnostics).
        tick: u64,
        entities: Vec<BorderRecord<S>>,
    },
    /// The difference against the receiver's last-known state. Applied
    /// atomically ONLY when `seq` matches the expected value exactly; any
    /// other value rejects the whole delta (never half-applied state) and
    /// requests a resync.
    Delta {
        seq: u64,
        /// See [`BorderExchange::Full::tick`].
        tick: u64,
        /// New or changed boundary records.
        upserts: Vec<BorderRecord<S>>,
        /// Wire ids that LEFT the strip since the last accepted exchange.
        exits: Vec<u64>,
    },
}

// measurement scaffolding for CROSS-SHARD §7 — remove or promote after
// the delta decision. These counters quantify the CURRENT full-state
// border exchange (bytes/records/CPU per tick, send drops) so a delta
// implementation can be judged against real numbers; the delta branch
// reuses this exact accounting for an apples-to-apples comparison.

/// The accounted payload size of ONE record: a u64 wire id plus the
/// payload's IN-MEMORY size. In process nothing is serialized, so this is
/// the wire-format LOWER bound a process-boundary deployment would pay
/// (the real codec is future work owned by the logic —
/// `docs/DISTRIBUTED.md` §4b); accounting through one helper keeps every
/// sender/receiver number comparable.
fn border_record_len<S>(r: &BorderRecord<S>) -> u64 {
    (std::mem::size_of::<u64>() + std::mem::size_of_val(&r.state)) as u64
}

/// The accounted payload size of one FULL [`BorderExchange`]: a u64 seq
/// header plus one record per entity. Both the baseline and the delta
/// implementation account through this helper (and [`delta_payload_len`]
/// for deltas) so the numbers stay comparable.
fn border_payload_len<'a, S: 'a>(
    records: impl Iterator<Item = &'a BorderRecord<S>>,
) -> u64 {
    std::mem::size_of::<u64>() as u64 + records.map(border_record_len).sum::<u64>()
}

/// The accounted payload size of one DELTA [`BorderExchange`]: the same
/// u64 seq header, one record per upsert and 8 bytes per exit (a bare
/// wire id). Same lower-bound accounting discipline as
/// [`border_payload_len`] — this is what a delta costs on a wire.
fn delta_payload_len<S>(upserts: &[BorderRecord<S>], exits: usize) -> u64 {
    std::mem::size_of::<u64>() as u64
        + upserts.iter().map(border_record_len).sum::<u64>()
        + (exits as u64) * std::mem::size_of::<u64>() as u64
}

/// The OWNED exchange payload computed before the send (phase 5): the
/// ledger commit on send success reads this instead of the queued
/// message, so no clone of the strip is kept alive for the commit.
/// Module-level because it is generic over the strip payload (a nested
/// item cannot see its parent's generics).
enum Commit<S> {
    Full,
    Delta {
        upserts: Vec<BorderRecord<S>>,
        exits: Vec<u64>,
    },
}

/// How often each neighbor is force-served a FULL exchange even when
/// deltas would do (§6.4 pin 3c): the low-frequency sigorta against
/// silent divergence — any bug that loses an untracked update heals at
/// the next cadence tick instead of never. 256 ticks ≈ 8.5 s at 30 Hz:
/// rare enough to be invisible in the byte budget, frequent enough to
/// bound the divergence lifetime far below any operational timescale.
const BORDER_FULL_EVERY_TICKS: u64 = 256;

/// Per-window border-exchange counters of ONE shard actor: send side =
/// phase 5 (collect_border + delta-vs-ledger + try_send to every
/// neighbor), receive side = the CONTROL-phase `ShardMsg::Border` arm
/// (apply upserts/exits or replace wholesale; there is no separate decode
/// step in process). Windowed on purpose: `step` resets the struct every
/// ~1 s of ticks and logs the deltas as one summary line — cumulative
/// counters would only give run-averages, while the decision needs
/// steady-state rates. The baseline fields keep their full-era semantics
/// (`exports`/`export_records`/`export_bytes` now account whatever is
/// ACTUALLY shipped — a delta's accounted size, not the strip's) so the
/// §7 baselines stay directly comparable; the fields below them are the
/// delta-era additions.
#[derive(Debug, Default)]
struct BorderStats {
    /// Exchanges sent (one per neighbor per tick that ships anything),
    /// cumulative-in-window: Fulls AND deltas AND failed attempts —
    /// attempts, because the drop counters need the same denominator as
    /// the full-era baseline.
    exports: u64,
    /// Records shipped, summed over all sends in-window (a Full counts
    /// its whole strip, a Delta its upserts+exits; counted per send,
    /// not per unique record, because that is what the channel carries).
    export_records: u64,
    /// Accounted payload bytes actually shipped ([`border_payload_len`]
    /// for Fulls, [`delta_payload_len`] for deltas) summed over all sends
    /// in-window.
    export_bytes: u64,
    /// Largest single export's record count in-window (context for the
    /// mean: one dense seam vs uniformly thin borders).
    export_records_max: usize,
    /// Wall time spent in phase 5 (collect + per-neighbor delta diff +
    /// clone + try_send), summed over ticks in-window (µs).
    export_us: u64,
    /// `try_send` failures against full/closed neighbor mailboxes,
    /// in-window (each one is a lost exchange; the delta path answers it
    /// with a forced Full on the next tick instead of waiting out the
    /// divergence until the periodic cadence).
    export_drops: u64,
    /// Exchanges received and applied, in-window (rejected deltas are NOT
    /// imports — they show up as `resync_requests_sent`).
    imports: u64,
    /// Records applied, in-window.
    import_records: u64,
    /// Accounted payload bytes applied, in-window.
    import_bytes: u64,
    /// Wall time spent applying received exchanges (map insert/remove /
    /// wholesale replace), summed over messages in-window (µs).
    import_us: u64,

    // -- Delta-era additions (CROSS-SHARD §6.4 / Faz 1). Additive by
    //    design: the baseline comparison needs the fields above intact. --
    /// Deltas queued successfully, in-window.
    delta_exchanges: u64,
    /// Fulls queued successfully, in-window (bootstrap + resync + the
    /// periodic cadence + send-failure recovery).
    full_exchanges: u64,
    /// Fulls served BECAUSE the neighbor asked for a resync, in-window.
    full_resyncs_served: u64,
    /// Resync requests SENT upstream after rejecting a delta (seq gap /
    /// stale view), in-window.
    resync_requests_sent: u64,
    /// Deltas LOST to `try_send` failures, in-window (the subset of
    /// `export_drops` that was a delta — each one is divergence until the
    /// forced Full lands).
    delta_drops: u64,
    /// What the equivalent FULL exchanges would have accounted, in-window
    /// (ledger-sized [`border_payload_len`] per successful send): the
    /// context number that makes "delta vs full" readable from one log
    /// line without re-running the baseline.
    equiv_full_bytes: u64,
}

/// The SENDER-side per-neighbor state of the delta protocol: what this
/// neighbor last accepted from us, under which sequence number, plus the
/// two flags that force the next exchange to be a Full. Keyed by shard
/// index like the mailbox table; created lazily on first export.
#[derive(Debug)]
struct NeighborExport<S> {
    /// The strip as THIS neighbor last accepted it (wire → record): the
    /// baseline every delta is diffed against. Advanced only on a
    /// successfully queued exchange — a Full overwrites it with the whole
    /// current strip, a Delta applies its own upserts/exits.
    ledger: HashMap<u64, BorderRecord<S>>,
    /// The sequence number stamped on the last exchange QUEUED for this
    /// neighbor (monotonic per sender incarnation; a fresh actor restarts
    /// at 0 and leads with a Full, which is exactly why a rebuilt shard
    /// resyncs its receivers with no extra wiring).
    seq: u64,
    /// Force the NEXT export to this neighbor to be a Full. Set by a send
    /// failure (a lost delta is divergence until healed — self-healing
    /// within ONE tick instead of waiting for the 256-tick cadence), by an
    /// explicit resync request, and by FIRST CONTACT; cleared by the Full
    /// that answers it.
    needs_full: bool,
    /// The neighbor explicitly asked for a resync
    /// ([`ShardMsg::ResyncRequest`]): the serving Full is counted as
    /// `full_resyncs_served`. Implies `needs_full`.
    resync_requested: bool,
}

impl<S> Default for NeighborExport<S> {
    /// A brand-new entry means UNKNOWN receiver state — first contact,
    /// or a freshly rebuilt incarnation meeting a receiver that still
    /// holds the dead incarnation's view. The protocol's answer to
    /// unknown is always the same: lead with a Full (`needs_full` starts
    /// TRUE, unlike a derived default).
    fn default() -> Self {
        Self {
            ledger: HashMap::new(),
            seq: 0,
            needs_full: true,
            resync_requested: false,
        }
    }
}

/// The RECEIVER-side per-neighbor state of the delta protocol: the
/// persistent borrowed view built incrementally from the neighbor's
/// exchanges, plus the continuity guard. Replaces the full-era
/// latest-whole-exchange slot.
#[derive(Debug)]
struct NeighborView<S> {
    /// The borrowed boundary records, keyed by wire id (upserts insert,
    /// exits remove — no ghosts survive an exit).
    recs: HashMap<u64, BorderRecord<S>>,
    /// The sequence number the NEXT delta from this neighbor must carry.
    /// A mismatch means an exchange went missing: reject, request a
    /// resync, stop trusting the view until a Full re-baselines it.
    expected_seq: u64,
    /// Set when a gap/stale-seq delta was rejected: the view MAY be wrong
    /// by an unknown amount (we know only THAT we lost something), so it
    /// is excluded from snapshots until the healing Full arrives —
    /// rendering possibly-diverged borrowed entities (ghost positions,
    /// despawned ids) would be worse than their brief absence.
    stale_until_full: bool,
}

impl<S> Default for NeighborView<S> {
    /// A fresh view trusts nothing yet: empty records, expecting sequence
    /// 0 (a Full re-baselines), not quarantined.
    fn default() -> Self {
        Self {
            recs: HashMap::new(),
            expected_seq: 0,
            stale_until_full: false,
        }
    }
}

/// A player's channel halves, moved with a migrating player entity
/// (ownership transfer — see module docs, "Migration protocol").
#[derive(Debug)]
pub struct PlayerMigration {
    /// The stable player identity (Faz 2): the receiving shard keys the
    /// row under it — unchanged by the move, exactly like `entity`.
    pub player: PlayerId,
    /// The transport session bound to this player at send time: the
    /// receiving shard installs ITS binding row (`conn → player`), so
    /// control broadcasts (Leave/Detach) still find the owner after the
    /// move. Session-keyed on purpose — it changes on resume, the
    /// player key does not.
    pub conn: ConnectionId,
    /// The join's epoch (the leave/migration race gate).
    pub epoch: u64,
    /// The entity this connection owns in the sending shard (the
    /// receiving shard re-registers it under the same id).
    pub entity: EntityId,
    /// The connection's outbound channel (fan-out).
    pub out: mpsc::Sender<FrameBatch>,
    /// The connection's action inbox (input).
    pub actions: Inbox<Action>,
    // -- Detach state that travels with the row (a PARKED player's
    //    entity migrates exactly like a live one — passive systems keep
    //    running on it, §3.2 — and the receiving shard must re-attach the
    //    same flags or it would start broadcasting into the dead outbound
    //    half and polluting its drop counter). --------------------------
    /// See [`crate::room::RoomConn::detached`].
    pub detached: bool,
    /// See [`crate::room::RoomConn::detach_deadline`].
    pub detach_deadline: Option<std::time::Instant>,
    /// See [`crate::room::RoomConn::expire_to`].
    pub expire_to: ExpireTo,
    /// See [`crate::room::RoomConn::bot_fed`].
    pub bot_fed: bool,
    /// See [`crate::room::RoomConn::session_epoch`] (the resume guard's
    /// stamp survives migrations).
    pub session_epoch: u64,
}

/// A migrating entity: the full game state plus the owning player, when
/// the entity is a player (NPCs have no player).
pub struct Migrating<S> {
    /// The entity's wire identity (kept across the migration).
    pub wire: u64,
    /// The full component state (game-shaped).
    pub state: S,
    pub player: Option<PlayerId>,
}

/// The per-shard answer to a broadcast resume (`ShardMsg::Resume`):
/// `Ok(Some(..))` = this shard held the identity and rebound it;
/// `Ok(None)` = not here; `Err` = the epoch guard tripped.
pub type ResumeReply = Result<Option<(EntityId, Mailbox<Action>)>, CoreError>;

/// Messages between shards and from the registry to a shard. One bounded
/// channel per shard: control (join/leave/shutdown) and the shard
/// protocol (migrate/border) share it — all are drained with `try_recv`
/// at the tick boundary, and the exchange traffic is small (a few
/// messages per neighbor per tick). `S` is the migration state and `B`
/// the boundary-strip payload (both game-owned; see [`ShardLogic`]).
#[derive(Debug)]
pub enum ShardMsg<S, B> {
    /// A player joins this shard (the registry routed it here through
    /// `home_shard`; `epoch` is the join's epoch — see module docs).
    Join {
        conn: ConnectionId,
        epoch: u64,
        out: mpsc::Sender<FrameBatch>,
        reply: oneshot::Sender<Result<(EntityId, Mailbox<Action>), CoreError>>,
    },
    /// A player left (broadcast to ALL shards of the room — exactly one of
    /// them owns the connection; the entity-id guard makes the others
    /// no-ops). `epoch` is the epoch of the join being left.
    Leave {
        conn: ConnectionId,
        entity: EntityId,
        epoch: u64,
    },
    /// The connection's transport died (the registry's `ConnClosed`
    /// broadcast, mirroring `RoomControl::Detach`): exactly the owning
    /// shard runs the policy ([`GameLogic::on_disconnect`]); the others
    /// no-op on the same entity-id guard a `Leave` uses.
    Detach {
        conn: ConnectionId,
        entity: EntityId,
        identity: String,
    },
    /// An identified join whose park ledger may hold this identity — the
    /// implicit resume attempt of §14.3, BROADCAST to every shard (§6):
    /// only the shard whose ledger holds it accepts (`Ok(Some(..))`);
    /// all others answer `Ok(None)` ("not here"); a tripped epoch guard
    /// answers `Err(CoreError::ResumeStale)`. The single-winner property
    /// is structural (the record exists on exactly one shard — the
    /// migration protocol's exactly-once invariant), locked by test in
    /// the reconnect suite.
    Resume {
        conn: ConnectionId,
        /// The new session's dispatcher-minted join epoch (the §7 guard:
        /// a stale duplicate is rejected by one integer comparison).
        epoch: u64,
        identity: String,
        out: mpsc::Sender<FrameBatch>,
        reply: oneshot::Sender<ResumeReply>,
    },
    /// An entity (with, for player entities, its connection) moves into
    /// this shard's region: installed in phase 0, before this tick's step.
    Migrate {
        from: usize,
        /// The tick index at which the sender sampled the crossing.
        at_tick: u64,
        wire: u64,
        state: S,
        player: Option<PlayerMigration>,
    },
    /// A neighbor's boundary update (§6.4): a Full replaces this shard's
    /// view of that neighbor wholesale; a Delta applies its upserts/exits
    /// when its sequence number matches the expected one exactly.
    Border {
        from: usize,
        exchange: BorderExchange<B>,
    },
    /// A neighbor rejected our delta stream (sequence gap or a view it
    /// had marked stale): serve that neighbor a FULL on the next phase 5.
    /// Deliberately a tiny standalone message on the same bounded mailbox
    /// instead of a shared resync flag: it rides the existing FIFO, so no
    /// new channel, no await, and the ordering against in-flight deltas
    /// is the natural one (the Full is generated after everything already
    /// queued was sent).
    ResyncRequest { from: usize },
    /// Stop the shard (drops the world).
    Shutdown,
}

/// Game-side behaviour of a SHARD actor: everything [`GameLogic`] (this
/// crate's `room` supertrait) already shares — the tick seam, snapshot
/// groups, membership, and the reconnect surface — plus this actor's
/// exclusive sharding seam below. The compile-time separation survives
/// the unification DELIBERATELY (design candidate A of
/// `docs/TRAIT-ARCHITECTURE.md` §3, not the single-trait B): forgetting
/// `neighbors()` or the serial range still does not compile, so a silent
/// migration break stays structurally impossible.
///
/// Implemented by the game crate; the core never inspects the world `W`
/// or the state `S::State`.
pub trait ShardLogic<W>: GameLogic<W> {
    /// The full state of a migrating entity (game-shaped; opaque to the
    /// core). `Debug` so a `ShardMsg` can derive it.
    type State: Debug + Send + 'static;

    /// This shard's index within its room (0-based; the factory supplies
    /// it at construction and the registry relies on it for the sample id
    /// and the neighbor wiring).
    fn index(&self) -> usize;

    /// The total number of shards in this room (the topology; the factory
    /// supplies it at construction).
    fn shard_count(&self) -> usize;

    /// This shard's wire-id range base: ids are minted as
    /// `serial_base + serial_used + 1`. Ranges of all shards of a room
    /// must be disjoint (the identity invariant).
    fn serial_base(&self) -> u64;
    /// This shard's wire-id range size.
    fn serial_range(&self) -> u64;
    /// How many identities this shard has minted so far.
    fn serial_used(&self) -> u64;

    /// The neighbor shard indices this shard exchanges with (the grid
    /// topology is the game's business).
    fn neighbors(&self) -> &[usize];

    /// Entities that moved into neighbor `neighbor`'s region by the end of
    /// this tick (phase 4): the full state plus the owning connection for
    /// player entities. The shard sends the `Migrate` messages itself and
    /// despawns the entities on the next tick (see module docs,
    /// "Migration protocol"); the logic only *reports* the crossings.
    fn collect_migrations(
        &mut self,
        world: &mut W,
        neighbor: usize,
    ) -> Vec<Migrating<Self::State>>;

    /// Install a migrating entity: spawn it with its full state, keeping
    /// its wire id. `player` is present for player entities (the shard
    /// handles the binding table itself); the logic must record the
    /// player→entity bookkeeping so that `on_leave` and the next
    /// `collect_migrations` see it. The player id is UNCHANGED by the
    /// move (stable identity — Faz 2).
    fn on_migrate_in(
        &mut self,
        world: &mut W,
        wire: u64,
        state: Self::State,
        player: Option<PlayerId>,
    );

    /// Remove an entity that migrated out (phase 4, the mark fired):
    /// despawn it and clean its bookkeeping.
    fn on_migrate_out(&mut self, world: &mut W, wire: u64);

    /// This shard's boundary entities for the phase-5 export to the
    /// neighbors. Each record pairs the CORE-managed wire identity with a
    /// LOGIC-owned payload ([`GameLogic::Strip`] — the visibility-strip
    /// content is the game's decision: position-only games keep it
    /// minimal, combat/prediction games extend it). The actor diffs this
    /// set against its per-neighbor ledgers and ships only deltas
    /// (upserts + exits) — except where a Full is due (first contact,
    /// resync, periodic cadence, post-drop healing), when the same set
    /// ships whole.
    ///
    /// Change detection is WHOLESALE [`PartialEq`] on the payload: any
    /// field difference produces an upsert. A game that wants looser
    /// equivalence (ignore-jitter) implements it in ITS type — quantize
    /// or round inside the `PartialEq`, or report an already-quantized
    /// payload from this method — so the core's diff stays one comparison
    /// and the equivalence policy lives where the payload lives.
    fn collect_border(&self, world: &W) -> Vec<BorderRecord<Self::Strip>>;

    /// The wire ids of this shard's OWN entities (the snapshot's own
    /// records). Used to keep a migrating entity from appearing twice in
    /// one snapshot — as its own record AND as the neighbor's one-tick-
    /// stale borrowed copy of itself (module docs, "Boundary
    /// visibility"): when both are present, the own record wins.
    fn own_wires(&self, world: &W) -> Vec<u64>;
}

// ---------------------------------------------------------------------
// The ShardLink seam (`docs/DISTRIBUTED.md` §3): everything shard↔
// neighbor crosses this interface. Today's only implementation is
// in-process; the Ipc/Net links arrive as new types behind the same two
// methods when their §10 triggers fire.
// ---------------------------------------------------------------------

/// The message class that crosses a [`ShardLink`]. The neighbor protocol
/// arms of [`ShardMsg`] (`Migrate`, `Border`,
/// [`ShardMsg::ResyncRequest`]) are all that ever flows across a link —
/// but the type is deliberately the WHOLE enum, not a narrower wire
/// enum: registry-originated arms share the SAME bounded per-shard inbox
/// by design (one FIFO per shard), and carving out a dedicated neighbor
/// channel would be a queue-topology change with a new interleaving for
/// the protocol to reason about, not a refactor. The seam does not
/// demultiplex — the CONTROL drain plus `handle_msg` stay the single
/// inbound authority.
pub(crate) type NeighborMsg<S, B> = ShardMsg<S, B>;

/// Why a best-effort [`ShardLink::send`] refused a message. The refused
/// value travels back INSIDE the error so a failed send loses nothing:
/// the migration path rolls the moved connection halves back out of the
/// exact message it tried to ship (a failed migration orphans nothing),
/// which is precisely what the raw `TrySendError` used to carry.
/// `Full` and `Closed` stay distinct because they answer to different
/// healing rules — full is transient backpressure (the §4 classes heal:
/// forced Full next tick, crossing re-collected next tick), closed means
/// this peer incarnation is gone (the room death watcher owns that
/// story), and some paths log only the transient case.
#[derive(Debug)]
pub(crate) enum LinkFull<M> {
    /// The link's bounded queue was full — the transient drop case.
    Full { msg: M },
    /// Nothing will ever dequeue from this link again (the peer's receive
    /// end is gone).
    Closed { msg: M },
}

impl<M> LinkFull<M> {
    /// Take the refused message back out: the rollback path reads its
    /// payload to restore exactly what the failed send would have
    /// consumed.
    fn into_msg(self) -> M {
        match self {
            LinkFull::Full { msg } | LinkFull::Closed { msg } => msg,
        }
    }
}

/// The shard↔neighbor communication contract (`docs/DISTRIBUTED.md`
/// §3): unifies in-process channels with future UDS/TCP links behind one
/// object-safe seam, so a distributed link can drop exactly where the
/// in-process one drops and every existing recovery path keeps working
/// unchanged. Send is BEST-EFFORT by design — delivery classes and
/// healing rules live in the message semantics (§4), not in the link.
/// Object-safe on purpose: the future Ipc/Net links will be separate
/// types held as trait objects beside today's.
pub(crate) trait ShardLink<S, B>: Send {
    /// En-queue one message for the peer, best-effort: a full or dead
    /// link refuses and hands the message BACK ([`LinkFull`]) — the
    /// caller's healing rules decide what that loss means. Never blocks,
    /// never awaits.
    fn send(&mut self, msg: NeighborMsg<S, B>)
    -> Result<(), LinkFull<NeighborMsg<S, B>>>;

    /// Take every queued inbound message, in send order (FIFO). The
    /// CONTROL phase drains through here; an empty queue yields an empty
    /// vec. Never blocks.
    fn drain(&mut self) -> Vec<NeighborMsg<S, B>>;
}

/// The in-process [`ShardLink`] (`docs/DISTRIBUTED.md` §3, "today"
/// column): wraps the existing bounded mpsc halves with ZERO transport
/// behavior of its own — `send` is `try_send` mapped onto [`LinkFull`],
/// `drain` is the plain `try_recv` loop — so capacity, FIFO order and
/// drop timing are the channel's, unchanged from the pre-seam wiring.
/// Either half may be absent: without a transmit end the link refuses
/// sends (`Closed` — it accepts nothing by construction); without a
/// receive end it delivers nothing. Both are total functions instead of
/// panics so the same type serves every wiring (per-neighbor outbound
/// slots, the actor's own inbound inbox, and the paired form tests use).
pub(crate) struct InProcLink<S, B> {
    /// The peer's mailbox (a clone of its shared per-shard inbox sender).
    tx: Option<Mailbox<ShardMsg<S, B>>>,
    /// This side's receive half. `None` on today's per-neighbor outbound
    /// slots: their inbound traffic lands in the shard's OWN shared
    /// inbox, whose link lives separately on the actor.
    rx: Option<Inbox<ShardMsg<S, B>>>,
}

impl<S, B> InProcLink<S, B> {
    /// Wrap one outbound per-neighbor mailbox: the actor sends into the
    /// neighbor's shared inbox and never receives here.
    fn outbound(tx: Mailbox<ShardMsg<S, B>>) -> Self {
        Self {
            tx: Some(tx),
            rx: None,
        }
    }

    /// Wrap the shard's own inbound inbox — the CONTROL drain's source,
    /// carrying registry control AND neighbor protocol messages on one
    /// bounded FIFO (registry.rs pass 1).
    fn inbound(rx: Inbox<ShardMsg<S, B>>) -> Self {
        Self {
            tx: None,
            rx: Some(rx),
        }
    }
}

impl<S: Send, B: Send> ShardLink<S, B> for InProcLink<S, B> {
    fn send(
        &mut self,
        msg: NeighborMsg<S, B>,
    ) -> Result<(), LinkFull<NeighborMsg<S, B>>> {
        match &self.tx {
            Some(tx) => tx.try_send(msg).map_err(|e| match e {
                mpsc::error::TrySendError::Full(msg) => LinkFull::Full { msg },
                mpsc::error::TrySendError::Closed(msg) => LinkFull::Closed { msg },
            }),
            // No transmit half: nothing was queued and nothing ever will
            // be — the permanent refusal, not backpressure.
            None => Err(LinkFull::Closed { msg }),
        }
    }

    fn drain(&mut self) -> Vec<NeighborMsg<S, B>> {
        let mut out = Vec::new();
        if let Some(rx) = &mut self.rx {
            while let Ok(m) = rx.try_recv() {
                out.push(m);
            }
        }
        out
    }
}

/// The shard actor. Owns one shard's world, its player table (the
/// shard's share of the room's members), and its group table — the
/// same ownership discipline as the room actor (`RoomActor<W, G, Sp>`),
/// plus the shard protocol state (neighbor mailboxes, the delta
/// protocol's per-neighbor sender ledgers and receiver views, the
/// pending migrate-out marks, the deferred migrations, the conn-epoch
/// tables). `G` is the snapshot group key, `St` the migration state and
/// `Sp` the boundary-strip payload (all three game-owned; see
/// [`ShardLogic`] and [`GameLogic::Strip`]).
pub struct ShardActor<W, G, St, Sp> {
    config: RoomConfig,
    index: usize,
    world: W,
    logic: Box<dyn ShardLogic<W, GroupKey = G, State = St, Strip = Sp>>,
    tick_rx: broadcast::Receiver<TickInfo>,
    /// The shard's own inbound link over the shared per-shard inbox
    /// (registry control AND neighbor protocol messages ride ONE bounded
    /// FIFO — registry.rs pass 1). Held as a [`ShardLink`] so the CONTROL
    /// drain crosses the seam: a future process-boundary deployment swaps
    /// the transport without touching the tick body.
    inbox: Box<dyn ShardLink<St, Sp>>,
    /// This shard's members, keyed by STABLE player identity (Faz 2 —
    /// same shape as the room actor; a resume or migration never re-keys
    /// this table).
    conns: HashMap<PlayerId, RoomConn<G>>,
    /// THE session binding table (Faz 2 — the shard-side twin of the
    /// room's): transport session → player. Established at join /
    /// migrate-in / resume; torn down at leave / detach-expire-despawn /
    /// migrate-out. Also the ingest-side authority translating every
    /// pulled action's `conn`.
    binding: HashMap<ConnectionId, PlayerId>,
    /// The epoch of the join this shard currently holds per TRANSPORT
    /// SESSION (set on Join/Migrate-in/resume; carried in outgoing
    /// Migrate messages). Conn-keyed BY DESIGN (Faz 2, documented where
    /// the contract left latitude): epochs are minted per session at the
    /// dispatcher, and the leave/migration race gate pairs a session's
    /// LEAVE against a session's in-flight MIGRATE — re-keying to stable
    /// players would make an old session's tombstone kill a resumed
    /// session's legitimate migration. A resume moves the entry as part
    /// of the binding move (the live SESSION changed); enumerated in
    /// `rebind_session`'s signpost.
    conn_epoch: HashMap<ConnectionId, u64>,
    /// The highest LEAVE epoch this shard has processed per connection —
    /// the leave/migration race gate: a Migrate of a join whose leave
    /// was already processed is dead and must be rejected. Kept SEPARATE
    /// from `conn_epoch` on purpose: an entry there can come from the
    /// join itself (or a migration install) — the join is alive, not
    /// dead — and gating on it would reject legitimate re-migrations of
    /// the same join (see module docs, "Migration protocol").
    ///
    /// Conn-keyed BY DESIGN (Faz 2): a tombstone guards the id of the
    /// join that DIED — a detached session never died (no leave was
    /// processed for it), so it wrote none; dead old sessions keep their
    /// guards under their own ids where they belong.
    ///
    /// The value is `(highest dead epoch, tick the winning leave was
    /// processed at)`: the tick half drives the TTL sweep. It is written
    /// by the leave that owns the surviving (highest) epoch, so expiry
    /// always measures the age of the guard that is actually
    /// load-bearing; an equal-or-stale leave keeps the older entry whole,
    /// which is the conservative direction (its guard lives longer).
    /// Entries expire after `TOMBSTONE_TTL_TICKS` — see module docs,
    /// "Tombstone lifetime".
    conn_tombstone: HashMap<ConnectionId, (u64, u64)>,
    /// The tick index of the last tombstone sweep (`None` = never run).
    /// The sweep runs lazily in CONTROL when `ctx.tick` has advanced
    /// `TOMBSTONE_SWEEP_EVERY_TICKS` past it — no timer task, no extra
    /// awaited source.
    last_tombstone_sweep: Option<u64>,
    groups: HashMap<G, GroupState>,
    /// One outbound link per shard index (used for the neighbors'
    /// indices) — [`InProcLink`] wrappers around exactly the mailboxes
    /// passed in, moved not cloned, so queue capacity, FIFO order and
    /// drop timing are the channel's, unchanged. Non-neighbor slots keep
    /// their dummy senders (wrapped, never sent to).
    links: Vec<Box<dyn ShardLink<St, Sp>>>,
    /// The borrowed boundary view per neighbor (the RECEIVER side of the
    /// delta protocol): persistent records built incrementally from
    /// Fulls/Deltas, with the expected-sequence guard per neighbor.
    border: HashMap<usize, NeighborView<Sp>>,
    /// The SENDER side of the delta protocol: what each neighbor last
    /// accepted from us (ledger + seq + the force-Full flags). Created
    /// lazily on first export; a fresh actor starts empty, so a rebuilt
    /// shard's first exchange is always a Full.
    export: HashMap<usize, NeighborExport<Sp>>,
    /// Entities marked out by a successful `Migrate` send: (wire, the tick
    /// index at which the shard despawns them).
    pending_out: Vec<(u64, u64)>,
    /// Migrations that arrived early (install gate not open — see
    /// `handle_msg`): re-offered at the next tick's CONTROL, in send
    /// order.
    deferred: VecDeque<ShardMsg<St, Sp>>,
    run_every: u64,
    last_at: Option<Instant>,
    steps: u64,
    keepalive_every: Option<u64>,
    metrics_every: u64,
    budget_us: u64,
    m: RoomCounters,
    // measurement scaffolding for CROSS-SHARD §7 — remove or promote
    // after the delta decision (see `BorderStats`).
    /// Border-exchange counters accumulated since the last summary.
    bstats: BorderStats,
    /// Summary cadence in steps: `tick_hz` rounded — one window ≈ 1 s.
    border_every: u64,
    metrics: mpsc::Sender<MetricsEvent>,
    // -- Shard-RPC (the Faz 3 promotion; see `crate::rpc` and the module
    //    docs, "Shard-RPC and match-result"): byte-for-byte the room
    //    actor's state shape. -------------------------------------------
    /// In-flight external requests, per TRANSPORT SESSION (conn-keyed BY
    /// DESIGN — see the module docs: a resumed session must not inherit
    /// its dead session's in-flight work). FIFO per conn: deadlines are
    /// non-decreasing, so only the head can expire. This shard is the
    /// single authority on "is this request still pending" for the
    /// sessions it owns.
    pending: HashMap<ConnectionId, VecDeque<PendingRequest>>,
    /// Total in-flight external requests on this shard (the shard-wide
    /// cap; the config fields are shared with the room actor).
    pending_total: usize,
    /// This tick's queued RPC answers per transport session; drained in
    /// the broadcast phase (handed to the logic's `private`) and emptied.
    /// Cleared with the SESSION on leave/rejoin/detach/migrate-out.
    queued: HashMap<ConnectionId, Vec<RpcReply>>,
    /// Per-tick scratch for the rare path of the broadcast's RPC-answer
    /// hand-off; a field so its capacity survives across ticks.
    replies_buf: Vec<RpcReply>,
    /// Worker reports: this shard's inbox for delegated-request outcomes
    /// (drained non-blockingly in the 0b phase — no new await).
    completions: Inbox<Completion>,
    /// The sender half of the completion channel, cloned to each spawned
    /// worker (bounded at the shard-wide pending cap — a completion burst
    /// cannot exceed the number of in-flight workers).
    completions_tx: Mailbox<Completion>,
    /// This shard's match-result sink (the control plane's result seam;
    /// see [`GameLogic::match_result`]): a bounded mailbox, sent with the
    /// synchronous `try_send` on teardown (no await, best effort). Every
    /// shard of a logical room shares the registry's sink.
    result_sink: Option<Mailbox<crate::registry::MatchResult>>,
}

impl<W, G, St, Sp> ShardActor<W, G, St, Sp>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
    St: Debug + Send + 'static,
    // The strip rides every exchange and view; the bounds mirror what
    // the delta protocol does with it (diff via PartialEq, clone into
    // each neighbor's message, store in the actor's maps).
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Build a shard actor. `neighbors` is indexed by shard index (the
    /// unused slots may be any closed/unused mailbox — only the
    /// `ShardLogic::neighbors()` slots are sent to); each entry is
    /// wrapped into an in-process [`ShardLink`] here, so the registry's
    /// wiring shape is unchanged. `result_sink` is the logical room's
    /// match-result sink shared by all its shards (`None` = this shard
    /// reports no result).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: RoomConfig,
        index: usize,
        world: W,
        logic: Box<dyn ShardLogic<W, GroupKey = G, State = St, Strip = Sp>>,
        tick_rx: broadcast::Receiver<TickInfo>,
        shard_rx: Inbox<ShardMsg<St, Sp>>,
        neighbors: Vec<Mailbox<ShardMsg<St, Sp>>>,
        run_every: u64,
        metrics: mpsc::Sender<MetricsEvent>,
        result_sink: Option<Mailbox<crate::registry::MatchResult>>,
    ) -> Self {
        let keepalive_every = if config.keepalive_hz > 0.0 {
            if config.keepalive_hz > config.tick_hz {
                warn!(
                    room = %config.id,
                    shard = index,
                    "keepalive_hz exceeds tick_hz: keep-alive clamps to \
                     every step (the silence gain is lost); set \
                     keepalive_hz <= tick_hz"
                );
            }
            Some(((config.tick_hz / config.keepalive_hz).round() as u64).max(1))
        } else {
            None
        };
        let metrics_every = if config.metrics_cadence_hz > 0.0 {
            ((config.tick_hz / config.metrics_cadence_hz).round() as u64).max(1)
        } else {
            1
        };
        let budget_us = config.period().as_micros() as u64;
        // measurement scaffolding for CROSS-SHARD §7: the border summary
        // window ≈ one second of ticks.
        let border_every = (config.tick_hz.round() as u64).max(1);
        // The completion channel: bounded at the shard-wide pending cap
        // (the room actor's rule — a completion burst cannot exceed the
        // number of in-flight workers, which the cap bounds); drained
        // every tick's 0b phase, so a full channel only parks a worker
        // until the next tick, never the shard.
        let (completions_tx, completions) = mpsc::channel(config.max_pending_requests.max(1));
        Self {
            config,
            index,
            world,
            logic,
            tick_rx,
            inbox: Box::new(InProcLink::inbound(shard_rx)),
            conns: HashMap::new(),
            binding: HashMap::new(),
            conn_epoch: HashMap::new(),
            conn_tombstone: HashMap::new(),
            last_tombstone_sweep: None,
            groups: HashMap::new(),
            links: neighbors
                .into_iter()
                .map(InProcLink::outbound)
                .map(|l| Box::new(l) as Box<dyn ShardLink<St, Sp>>)
                .collect(),
            border: HashMap::new(),
            export: HashMap::new(),
            pending_out: Vec::new(),
            deferred: VecDeque::new(),
            run_every: run_every.max(1),
            last_at: None,
            steps: 0,
            keepalive_every,
            metrics_every,
            budget_us,
            m: RoomCounters::default(),
            bstats: BorderStats::default(),
            border_every,
            metrics,
            pending: HashMap::new(),
            pending_total: 0,
            queued: HashMap::new(),
            replies_buf: Vec::new(),
            completions,
            completions_tx,
            result_sink,
        }
    }

    /// Run until the ticker channel closes or a `Shutdown` is processed on
    /// a tick (same lifecycle discipline as the room actor).
    pub async fn run(mut self) {
        debug!(
            room = %self.config.id,
            shard = self.index,
            hz = self.config.tick_hz,
            "shard actor started"
        );
        loop {
            let t = match self.tick_rx.recv().await {
                Ok(t) => t,
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    self.m.lagged_events += 1;
                    self.m.lagged_ticks += missed;
                    warn!(
                        room = %self.config.id,
                        shard = self.index,
                        missed,
                        "shard lagged behind global ticker; next step \
                         catches up via dt (and its boundary exports / \
                         migrations are one tick late — the accepted \
                         degradation, see the module docs)"
                    );
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };
            if t.tick % self.run_every != 0 {
                continue;
            }
            if !self.step(&t) {
                break;
            }
        }
        self.logic.on_shutdown();
        // The match-result seam (the Faz 3 promotion; see the module docs,
        // "Shard-RPC and match-result"): THIS shard reports ITS final state
        // through the shared sink under the LOGICAL room id. One logical
        // room therefore yields one payload PER SHARD (the platform's
        // adapter concatenates/filters; nothing was added to
        // `MatchResult`). Same best-effort discipline as the room actor:
        // a full or gone sink drops the result and warns/debugs — a slow
        // consumer must not stall the shard's teardown, and the shard's
        // only await stays `tick_rx.recv()`.
        if let Some(result) = self.logic.match_result(&mut self.world)
            && let Some(sink) = &self.result_sink
        {
            match sink.try_send(crate::registry::MatchResult {
                room: self.config.id,
                payload: result,
            }) {
                Ok(()) => debug!(
                    room = %self.config.id,
                    shard = self.index,
                    "shard match result reported"
                ),
                Err(mpsc::error::TrySendError::Full(_)) => warn!(
                    room = %self.config.id,
                    shard = self.index,
                    "match result dropped: sink full"
                ),
                Err(mpsc::error::TrySendError::Closed(_)) => debug!(
                    room = %self.config.id,
                    shard = self.index,
                    "match result dropped: sink gone"
                ),
            }
        }
        debug!(
            room = %self.config.id,
            shard = self.index,
            members = self.conns.len(),
            "shard actor stopped"
        );
    }

    /// One full step: the six phases (see module docs), measured and
    /// flushed to the metrics channel once at the end. Synchronous — the
    /// shard's only await stays `tick_rx.recv()`. Returns `false` when the
    /// actor should stop.
    fn step(&mut self, t: &TickInfo) -> bool {
        self.steps += 1;

        // -- tick latency (same measurement as the room actor).
        let late_us = Instant::now().saturating_duration_since(t.at).as_micros() as u64;
        if self.steps == 1 {
            self.m.late_min_us = late_us;
            self.m.late_max_us = late_us;
        } else if late_us > self.m.late_max_us {
            self.m.late_max_us = late_us;
        }
        self.m.late_sum_us = self.m.late_sum_us.saturating_add(late_us);

        let t0 = Instant::now();
        let keep = self.step_phases(t);
        let step_us = t0.elapsed().as_micros() as u64;
        if self.steps == 1 {
            self.m.step_min_us = step_us;
            self.m.step_max_us = step_us;
        } else if step_us > self.m.step_max_us {
            self.m.step_max_us = step_us;
        }
        self.m.step_sum_us = self.m.step_sum_us.saturating_add(step_us);
        self.m.step_hist[hist_index(self.budget_us, step_us)] += 1;

        if self.steps.is_multiple_of(self.metrics_every)
            && let Err(mpsc::error::TrySendError::Full(_)) =
                self.metrics.try_send(MetricsEvent::Room(self.sample()))
        {
            self.m.metrics_dropped += 1;
        }

        // measurement scaffolding for CROSS-SHARD §7 — remove or promote
        // after the delta decision: one summary line per ~1 s of steps
        // (WINDOW deltas — the counters reset here), emitted only when
        // this shard actually exchanged something in the window, so a
        // quiet shard logs nothing and the steady state reads as clean
        // per-second rates.
        if self.steps.is_multiple_of(self.border_every)
            && !self.logic.neighbors().is_empty()
        {
            let s = std::mem::take(&mut self.bstats);
            if s.exports > 0 || s.imports > 0 {
                info!(
                    room = %self.config.id,
                    shard = self.index,
                    window_ticks = self.border_every,
                    exports = s.exports,
                    export_records = s.export_records,
                    export_bytes = s.export_bytes,
                    export_records_max = s.export_records_max,
                    export_us = s.export_us,
                    export_drops = s.export_drops,
                    imports = s.imports,
                    import_records = s.import_records,
                    import_bytes = s.import_bytes,
                    import_us = s.import_us,
                    // Delta-era additions (§6.4): the exchange mix, the
                    // resync traffic and the full-equivalent byte context.
                    delta_exchanges = s.delta_exchanges,
                    full_exchanges = s.full_exchanges,
                    full_resyncs_served = s.full_resyncs_served,
                    resync_requests_sent = s.resync_requests_sent,
                    delta_drops = s.delta_drops,
                    equiv_full_bytes = s.equiv_full_bytes,
                    "border_exchange_summary"
                );
            }
        }
        keep
    }

    /// The six phases. Synchronous. Returns `false` when the actor should
    /// stop.
    fn step_phases(&mut self, t: &TickInfo) -> bool {
        // -- time (same catch-up discipline as the room actor).
        let dt = {
            let last = self.last_at.replace(t.at);
            match last {
                Some(last) => {
                    let elapsed = t.at.saturating_duration_since(last);
                    let cap = self.config.period() * self.config.max_catchup;
                    if elapsed > cap {
                        warn!(
                            room = %self.config.id,
                            shard = self.index,
                            ?elapsed,
                            ?cap,
                            "long stall: catch-up dt clamped (sim temporarily \
                             slower than real time)"
                        );
                        cap
                    } else {
                        elapsed
                    }
                }
                None => self.config.period(),
            }
        };
        let ctx = TickCtx {
            room: self.config.id,
            tick: t.tick,
            dt,
        };

        // -- Tombstone TTL sweep (lazy, CONTROL). Bounded cost: the
        //    tombstone table only ever holds leaves of the last TTL
        //    window (the epoch table is pruned on leave), so this O(n)
        //    `retain` runs over a small set and amortizes to noise.
        //
        //    Why expiry preserves the race gate (the load-bearing
        //    argument): a Migrate processed AFTER its tombstone expired
        //    must have been enqueued more than TOMBSTONE_TTL_TICKS ticks
        //    after the corresponding leave — Migrates are generated only
        //    while the entity is alive (leave-processing despawns it) and
        //    travel into a BOUNDED per-shard FIFO inbox drained by every
        //    CONTROL phase, so queue residence beyond TTL ticks means
        //    this shard itself is stalled far beyond any healthy
        //    operating point. The pre-existing degradation notes (the
        //    one-tick alignment, the bounded blink — module docs) already
        //    assume non-stalled shards; inside that envelope no genuinely
        //    racing Migrate is still in flight when its tombstone expires.
        if self
            .last_tombstone_sweep
            .is_none_or(|at| ctx.tick.saturating_sub(at) >= TOMBSTONE_SWEEP_EVERY_TICKS)
        {
            let now = ctx.tick;
            self.conn_tombstone
                .retain(|_, (_, wrote)| now.saturating_sub(*wrote) < TOMBSTONE_TTL_TICKS);
            self.last_tombstone_sweep = Some(now);
        }

        // -- Phase 0 — CONTROL (drain the shard channel; the same messages
        //    the room handles on its control channel, plus the shard
        //    protocol — Migrate / Border). Deferred migrations (their
        //    install gate has not opened yet — see `handle_msg`) are
        //    re-offered FIRST, in send order.
        let deferred = std::mem::take(&mut self.deferred);
        for m in deferred {
            if !self.handle_msg(m, &ctx) {
                return false;
            }
        }
        // The drain itself crosses the ShardLink seam (`docs/DISTRIBUTED.md`
        // §3). Pulling everything queued NOW into a vec and then handling is
        // observably identical to the old interleaved `try_recv` loop:
        // nothing in `handle_msg` enqueues into THIS shard's own inbox
        // synchronously (sends go to neighbors' inboxes), and on Shutdown
        // any leftovers die with the actor's inbox either way.
        for m in self.inbox.drain() {
            if !self.handle_msg(m, &ctx) {
                return false;
            }
        }

        // -- Phase 0b — shard-RPC deferred completions (the Faz 3
        //    promotion; see `crate::rpc`): drain the worker reports
        //    (non-blocking — this shard never awaits a worker; the same
        //    try_recv discipline as the control drain above) and reconcile
        //    each report against the pending set. Exactly one answer per
        //    request is structural: a report for an id that is no longer
        //    pending (already answered, timed out below, its session left,
        //    OR its session migrated to another shard) is dropped and
        //    counted (`requests_late`).
        while let Ok(rep) = self.completions.try_recv() {
            let Some(deq) = self.pending.get_mut(&rep.conn) else {
                self.m.requests_late += 1;
                continue;
            };
            match deq.iter().position(|p| p.id == rep.id) {
                Some(idx) => {
                    // The inner op comes from the pending entry (the
                    // worker's report is outcome-only — this actor is the
                    // authority on the request's shape).
                    let op = deq[idx].op;
                    deq.remove(idx);
                    self.pending_total -= 1;
                    if deq.is_empty() {
                        self.pending.remove(&rep.conn);
                    }
                    self.queued.entry(rep.conn).or_default().push(RpcReply {
                        id: rep.id,
                        ok: rep.ok,
                        op,
                        reason: rep.reason,
                        payload: rep.payload,
                    });
                }
                None => self.m.requests_late += 1,
            }
        }
        // Sweep expired pending requests (the client-visible timeout):
        // per connection the deadlines are non-decreasing (same timeout,
        // FIFO arrivals), so only the head of each deque can be due. The
        // tick body stays synchronous: a wall-clock comparison, no await.
        // Cost when quiet: one `is_empty` probe (a shard with no pending
        // requests pays nothing below it).
        if !self.pending.is_empty() {
            let now = Instant::now();
            let mut due: Vec<(ConnectionId, PendingRequest)> = Vec::new();
            for (conn, deq) in self.pending.iter_mut() {
                if let Some(front) = deq.front()
                    && front.due <= now
                    && let Some(p) = deq.pop_front()
                {
                    self.pending_total -= 1;
                    due.push((*conn, p));
                }
            }
            self.pending.retain(|_, deq| !deq.is_empty());
            for (conn, p) in due {
                self.m.requests_timed_out += 1;
                self.queued.entry(conn).or_default().push(RpcReply {
                    id: p.id,
                    ok: false,
                    op: p.op,
                    reason: TIMEOUT_REASON.to_string(),
                    payload: bytes::Bytes::new(),
                });
            }
        }

        // -- Phase 0c — detach-hold sweep: the shard-side mirror of the
        //    room actor's (§14.4 — core owns the clock; timed holds fire
        //    on their deadline, combat-helds on `may_release`; the ended
        //    hold goes to `on_detach_expired` and then despawns or turns
        //    bot-fed). Runs BEFORE READ so an expired row is gone before
        //    this tick's pulls.
        if self.conns.values().any(|rc| rc.detached && !rc.bot_fed) {
            let now = Instant::now();
            let mut due: Vec<(PlayerId, ExpireTo)> = Vec::new();
            let mut ask: Vec<PlayerId> = Vec::new();
            for (&player, rc) in &self.conns {
                if !rc.detached || rc.bot_fed {
                    continue;
                }
                match rc.detach_deadline {
                    Some(dl) if now >= dl => due.push((player, rc.expire_to)),
                    Some(_) => {}
                    None => ask.push(player),
                }
            }
            for player in ask {
                if self.logic.may_release(&mut self.world, player) {
                    let to = self
                        .conns
                        .get(&player)
                        .map(|rc| rc.expire_to)
                        .unwrap_or(ExpireTo::Despawn);
                    due.push((player, to));
                }
            }
            for (player, to) in due {
                self.logic.on_detach_expired(&mut self.world, player, to);
                match to {
                    ExpireTo::Despawn => {
                        self.m.detach_expired_despawn += 1;
                        self.despawn_conn(player, false);
                        debug!(
                            room = %self.config.id,
                            shard = self.index,
                            %player,
                            "detach hold expired: despawn"
                        );
                    }
                    ExpireTo::AiHandover => {
                        self.m.detach_expired_ai += 1;
                        if let Some(rc) = self.conns.get_mut(&player) {
                            rc.bot_fed = true;
                            rc.detach_deadline = None;
                        }
                        debug!(
                            room = %self.config.id,
                            shard = self.index,
                            %player,
                            "detach hold expired: AI handover (bot_fed; Tur B seam)"
                        );
                    }
                }
            }
        }

        // -- Phase 1 — READ (the room's bounded pull: per-connection
        //    fairness budget + shard-level pull budget).
        let per_conn = self.config.max_actions_per_conn_per_tick;
        let mut budget = self.config.max_pending_actions;
        let mut actions: Vec<Action> = Vec::new();
        for r in self.conns.values_mut() {
            // A detached (or bot-fed) row has no live input source; skip
            // it exactly like the room's rotation does.
            if r.detached {
                continue;
            }
            for _ in 0..per_conn {
                if budget == 0 {
                    break;
                }
                match r.actions.try_recv() {
                    Ok(a) => {
                        budget -= 1;
                        actions.push(a);
                    }
                    Err(_) => break,
                }
            }
            if budget == 0 {
                break;
            }
        }

        // -- Phase 1.5 — BINDING TRANSLATION: byte-for-byte the room
        //    actor's step (Faz 2 — ONE mechanism on both actors; see the
        //    room's phase comment for the full rationale): every pulled
        //    action still names its transport session, and THIS table is
        //    the one authority for the conn ↔ PlayerId context. An
        //    unbound conn drops here — after a resume the old session has
        //    no binding row left, so its stray frames can never reach the
        //    world.
        actions.retain_mut(|a| match self.binding.get(&a.conn) {
            Some(&player) => {
                a.player = player;
                true
            }
            None => {
                debug!(
                    room = %self.config.id,
                    shard = self.index,
                    conn = %a.conn,
                    op = a.op,
                    "action dropped: connection not bound (stale/old session)"
                );
                false
            }
        });

        // -- Phase 2a — SPLIT the requests out of the pulled actions (the
        //    Faz 3 RPC promotion; byte-for-byte the room actor's split).
        //    A request is an action carrying the base-band envelope
        //    opcode; this actor decodes the envelope (a base message) and
        //    hands the rest to the logic at 2c. The split is in-order:
        //    both lists keep arrival order, and ALL fire-and-forget
        //    actions of the tick ingest BEFORE any request is handed to
        //    `handle_request` (the ordering contract of `crate::rpc`: a
        //    request sees the world after this tick's actions were
        //    applied). Cost when quiet: one u16 compare per pulled action.
        //
        //    A malformed envelope (or id = 0, which cannot correlate) is a
        //    NORMAL rejection: answered this tick, counted in the
        //    malformed bucket — a client bug, not a protocol violation.
        let mut requests: Vec<RpcRequest> = Vec::new();
        actions.retain_mut(|a| {
            if a.op != RPC_REQ_OP {
                return true;
            }
            match gsb_protocol::base::RpcRequest::decode(&a.payload[..]) {
                Ok(env) if env.id != 0 => {
                    requests.push(RpcRequest {
                        conn: a.conn,
                        // Already translated (phase 1.5): the logic sees
                        // the stable player key, while pending/replies
                        // stay session-scoped under `conn`.
                        player: a.player,
                        id: env.id,
                        // The wire type is `u32`; the op space is `u16`
                        // by protocol contract (same cast as the room).
                        op: env.op as u16,
                        payload: env.payload.into(),
                    });
                    false
                }
                _ => {
                    self.m.requests_rejected_malformed += 1;
                    self.queue_reply(
                        a.conn,
                        0,
                        a.op,
                        false,
                        "malformed request envelope (or correlation id = 0)".to_string(),
                        bytes::Bytes::new(),
                    );
                    false
                }
            }
        });

        // -- Phase 2b — CONVERT.
        self.logic.ingest(&mut self.world, &ctx, &mut actions);

        // -- Phase 2c — REQUESTS (the shard-side mirror of the room's
        //    request loop): duplicate-in-flight guard ABOVE the decision
        //    (every decision kind), then `handle_request`, then per-decision
        //    handling with caps enforced where the pending state lives.
        for req in &requests {
            // Duplicate id still in flight: reject WITHOUT re-processing —
            // a second reply for one id would breach exactly-one-answer,
            // and a retrying client must not buy a double-applied side
            // effect. The id becomes reusable once answered (no history).
            if self
                .pending
                .get(&req.conn)
                .is_some_and(|d| d.iter().any(|p| p.id == req.id))
            {
                self.m.requests_rejected_dup += 1;
                self.queue_reply(
                    req.conn,
                    req.id,
                    req.op,
                    false,
                    "duplicate request id (the request is still in flight)".to_string(),
                    bytes::Bytes::new(),
                );
                continue;
            }
            let decision = self.logic.handle_request(&mut self.world, &ctx, req);
            match decision {
                None => {
                    // Not a request this logic handles: a normal "no
                    // handler" rejection (the client learns immediately
                    // instead of waiting out its own timeout).
                    self.m.requests_rejected_no_handler += 1;
                    self.queue_reply(
                        req.conn,
                        req.id,
                        req.op,
                        false,
                        format!("no request handler for op {:#04x}", req.op),
                        bytes::Bytes::new(),
                    );
                }
                Some(crate::rpc::RequestDecision::Reply(payload)) => {
                    self.m.requests_local += 1;
                    self.queue_reply(req.conn, req.id, req.op, true, String::new(), payload);
                }
                Some(crate::rpc::RequestDecision::Reject(reason)) => {
                    self.m.requests_rejected_logic += 1;
                    self.queue_reply(req.conn, req.id, req.op, false, reason, bytes::Bytes::new());
                }
                Some(crate::rpc::RequestDecision::External(fut)) => {
                    // Caps (this actor's authority on pending state): a
                    // request past a cap is a normal rejection, same tick —
                    // the logic's `External` decision does not commit the
                    // registration. Same fields as the room's (the shared
                    // RoomConfig), so one sizing derivation covers both
                    // actors. Priority mirrors the room: over BOTH caps
                    // counts against the per-connection bucket (the
                    // client's own quota is the actionable one).
                    let per_conn = self
                        .pending
                        .get(&req.conn)
                        .map(VecDeque::len)
                        .unwrap_or(0);
                    let over_conn_cap =
                        per_conn >= self.config.max_pending_requests_per_conn;
                    let over_room_cap = self.pending_total >= self.config.max_pending_requests;
                    if over_conn_cap || over_room_cap {
                        if over_conn_cap {
                            self.m.requests_rejected_conn_cap += 1;
                        } else {
                            self.m.requests_rejected_room_cap += 1;
                        }
                        let reason = if over_conn_cap {
                            "pending request limit reached (per connection)".to_string()
                        } else {
                            "pending request limit reached (room)".to_string()
                        };
                        self.queue_reply(req.conn, req.id, req.op, false, reason, bytes::Bytes::new());
                        continue;
                    }
                    // Register it pending on THIS shard, then delegate.
                    let due = Instant::now() + self.config.request_timeout;
                    self.pending
                        .entry(req.conn)
                        .or_default()
                        .push_back(PendingRequest {
                            id: req.id,
                            op: req.op,
                            due,
                        });
                    self.pending_total += 1;
                    self.m.requests_external += 1;
                    // The worker task: resolves the future under a timeout
                    // RESOURCE guard (the same deadline this actor's sweep
                    // enforces as the client-visible authority; on expiry
                    // the worker reports nothing). The report rides the
                    // completion channel; the 0b phase of a later tick
                    // reconciles it.
                    let conn = req.conn;
                    let id = req.id;
                    let timeout = self.config.request_timeout;
                    let report_tx = self.completions_tx.clone();
                    tokio::spawn(async move {
                        match tokio::time::timeout(timeout, fut).await {
                            Ok(Ok(payload)) => {
                                let _ = report_tx.send(Completion::reply(conn, id, payload)).await;
                            }
                            Ok(Err(reason)) => {
                                let _ = report_tx.send(Completion::error(conn, id, reason)).await;
                            }
                            Err(_elapsed) => {
                                // Timed out: no report — the sweep owns the
                                // client-visible timeout. The worker exits.
                            }
                        }
                        // A failed report send means the shard shut down:
                        // the worker exits either way — its lifetime is
                        // bounded by the timeout in every case.
                    });
                }
            }
        }

        // -- Phase 3 — SYSTEMS.
        self.logic.update(&mut self.world, &ctx);

        // -- Phase 4 — MIGRATE.
        //    4a. Despawn the entities marked out by a SUCCESSFUL send last
        //        tick (the mark's tick index is this one — the protocol's
        //        exactly-once boundary, see module docs).
        if !self.pending_out.is_empty() {
            let mut due = Vec::new();
            let mut rest = Vec::new();
            for (wire, despawn_at) in self.pending_out.drain(..) {
                if despawn_at <= t.tick {
                    due.push(wire);
                } else {
                    rest.push((wire, despawn_at));
                }
            }
            for wire in due {
                self.logic.on_migrate_out(&mut self.world, wire);
            }
            self.pending_out = rest;
        }
        //    4b. Collect this tick's crossings and send them. A failed
        //        send (neighbor's channel full — the neighbor stalled for
        //        ~seconds) is NOT marked: the entity stays here and is
        //        re-collected on the next tick (the crossing is a function
        //        of position, which is unchanged until it moves back). The
        //        failed send hands the message back (`TrySendError`
        //        carries it), so the moved connection halves are rolled
        //        back into the connection table — a failed migration
        //        orphans nothing.
        let nb = self.logic.neighbors().to_vec();
        for b in nb {
            let migrations = self.logic.collect_migrations(&mut self.world, b);
            for mig in migrations {
                let player = mig.player.and_then(|p| {
                    let entry = self.conns.remove(&p)?;
                    // The binding row travels too (the receiving shard
                    // installs its own): the session stays bound to this
                    // player across the move, so control broadcasts still
                    // find the owner.
                    self.binding.remove(&entry.conn);
                    Some(PlayerMigration {
                        player: p,
                        conn: entry.conn,
                        // The epoch of the join this entity belongs to: the
                        // shard that last installed it recorded it.
                        epoch: self.conn_epoch.get(&entry.conn).copied().unwrap_or(0),
                        out: entry.out,
                        actions: entry.actions,
                        entity: entry.entity,
                        // A PARKED player's entity migrates like any other;
                        // its detach flags ride along so the receiving shard
                        // keeps skipping its dead halves (§3.2 + §7).
                        detached: entry.detached,
                        detach_deadline: entry.detach_deadline,
                        expire_to: entry.expire_to,
                        bot_fed: entry.bot_fed,
                        session_epoch: entry.session_epoch,
                    })
                });
                // The session whose request state dies with a COMMITTED
                // move (the Ok arm below); read before the send consumes
                // the message.
                let moving_conn = player.as_ref().map(|pm| pm.conn);
                match self.links[b].send(ShardMsg::Migrate {
                    from: self.index,
                    at_tick: t.tick,
                    wire: mig.wire,
                    state: mig.state,
                    player,
                }) {
                    Ok(()) => {
                        // The move committed: the session's RPC state does
                        // NOT travel (module docs, "Shard-RPC and
                        // match-result") — the worker futures of its
                        // in-flight requests were spawned HERE and report to
                        // THIS shard's completion channel, so carrying
                        // pending entries across would need cross-actor
                        // report forwarding. Same §11 posture as detach:
                        // session-scoped request state dies with the
                        // session's ownership move; stale reports land late
                        // here and are counted `requests_late`; the player
                        // re-requests on the receiving shard under a fresh
                        // id. (Dropped AFTER a successful send — on a failed
                        // one below, the connection is rolled back whole and
                        // keeps its in-flight work.)
                        if let Some(mc) = moving_conn {
                            self.drop_conn_request_state(mc);
                        }
                        self.pending_out.push((mig.wire, t.tick + 1));
                    }
                    // The link refused and handed the message back — the
                    // same value the raw TrySendError used to carry.
                    Err(full) => {
                        let msg = full.into_msg();
                        // Roll back the player's move (the entity stays;
                        // the row must remain registered and pull
                        // its input here until the retry lands) — including
                        // the binding row the send removed.
                        if let ShardMsg::Migrate {
                            player: Some(p),
                            wire,
                            ..
                        } = msg
                        {
                            self.conns.insert(
                                p.player,
                                RoomConn {
                                    conn: p.conn,
                                    out: p.out,
                                    actions: p.actions,
                                    entity: p.entity,
                                    group: self.logic.group_of(&self.world, p.player),
                                    batch: Vec::new(),
                                    detached: p.detached,
                                    detach_deadline: p.detach_deadline,
                                    expire_to: p.expire_to,
                                    bot_fed: p.bot_fed,
                                    session_epoch: p.session_epoch,
                                },
                            );
                            self.binding.insert(p.conn, p.player);
                            warn!(
                                room = %self.config.id,
                                shard = self.index,
                                neighbor = b,
                                %wire,
                                "migrate send failed (neighbor channel \
                                 full); the entity stays, the connection \
                                 is rolled back, and the crossing is \
                                 retried next tick"
                            );
                        }
                    }
                }
            }
        }

        // -- Phase 5 — BORDER (export this shard's boundary entities to
        //    every neighbor as a §6.4 delta exchange: a Full against the
        //    per-neighbor ledger when continuity is broken or the periodic
        //    cadence fires, otherwise just upserts+exits; a quiet strip
        //    ships NOTHING, which is the entire point of the delta).
        //
        //    measurement scaffolding for CROSS-SHARD §7 — remove or
        //    promote after the delta decision: the whole phase (collect +
        //    diff-vs-ledger + send) is timed and counted into `bstats`
        //    with the SAME helpers as the full-era baseline, so bytes/
        //    records/µs stay directly comparable. With no neighbors
        //    nothing is exchanged, so nothing is counted — an edge/
        //    unsharded topology pays no instrumentation cost beyond the
        //    emptiness check.
        let nb = self.logic.neighbors().to_vec();
        if !nb.is_empty() {
            let t5 = Instant::now();
            let records = self.logic.collect_border(&self.world);
            // The current strip as a wire-keyed map: the delta diff and
            // the ledger update both want membership tests, and duplicate
            // wires (a game bug if any) collapse deterministically to the
            // last record instead of corrupting the ledger bookkeeping.
            let current: HashMap<u64, BorderRecord<Sp>> =
                records.into_iter().map(|r| (r.wire, r)).collect();
            // Pin 3c: the low-frequency periodic Full — one comparison per
            // neighbor, taken once per tick.
            let periodic_full = t.tick.is_multiple_of(BORDER_FULL_EVERY_TICKS);
            let mut drops = 0u64;
            let mut delta_drops = 0u64;
            let mut shipped_records = 0u64;
            let mut shipped_bytes = 0u64;
            let mut records_max = 0usize;
            let mut delta_exchanges = 0u64;
            let mut full_exchanges = 0u64;
            let mut resyncs_served = 0u64;
            let mut equiv_full_bytes = 0u64;
            let mut attempts = 0u64;
            for b in &nb {
                let st = self.export.entry(*b).or_default();
                // A Full is forced by: the periodic cadence (3c), a send
                // failure on the last exchange (self-heal in ONE tick), an
                // explicit resync request from the neighbor, or first
                // contact (`needs_full` defaults true on a fresh actor —
                // which is also the rebuilt-incarnation path).
                let force_full = periodic_full || st.needs_full;
                let was_resync = st.resync_requested;
                st.seq += 1;
                let seq = st.seq;
                let tick = t.tick;
                // The delta payload is computed OWNED first so the ledger
                // commit on send success does not need to read the message
                // back: the exchange carries a clone of exactly these
                // upserts/exits (one small-allocation copy per send — the
                // honest price counted in phase 5).
                let (exchange, payload, recs_shipped, commit) = if force_full {
                    (
                        BorderExchange::Full {
                            seq,
                            tick,
                            entities: current.values().cloned().collect(),
                        },
                        border_payload_len(current.values()),
                        current.len(),
                        Commit::Full,
                    )
                } else {
                    // The delta: upserts = records missing from or changed
                    // against the ledger; exits = ledger ids gone from the
                    // strip. Both directions are explicit so neither new
                    // nor departed entities can be misread as "unchanged".
                    let mut upserts = Vec::new();
                    for r in current.values() {
                        match st.ledger.get(&r.wire) {
                            // Whole-payload equality IS the change test:
                            // any field difference ships an upsert. A looser
                            // equivalence (ignore-jitter) is the payload
                            // owner's business — implemented in its
                            // `PartialEq` or by quantizing at collection.
                            Some(prev) if *prev == *r => {}
                            _ => upserts.push(r.clone()),
                        }
                    }
                    let mut exits: Vec<u64> = st
                        .ledger
                        .keys()
                        .filter(|w| !current.contains_key(w))
                        .copied()
                        .collect();
                    exits.sort_unstable();
                    if upserts.is_empty() && exits.is_empty() {
                        // Nothing changed since this neighbor last
                        // accepted: ship nothing — this silent-tick skip
                        // IS the byte win being measured. The sequence
                        // number is NOT consumed (rolled back here):
                        // skipping a tick must not manufacture a gap the
                        // receiver would treat as loss.
                        st.seq -= 1;
                        continue;
                    }
                    (
                        BorderExchange::Delta {
                            seq,
                            tick,
                            upserts: upserts.clone(),
                            exits: exits.clone(),
                        },
                        delta_payload_len(&upserts, exits.len()),
                        upserts.len() + exits.len(),
                        Commit::Delta { upserts, exits },
                    )
                };
                attempts += 1;
                match self.links[*b].send(ShardMsg::Border {
                    from: self.index,
                    exchange,
                }) {
                    Ok(()) => {
                        // Commit: the neighbor WILL see this exchange (the
                        // bounded FIFO holds it until its CONTROL drain),
                        // so the ledger may advance to it.
                        match commit {
                            Commit::Full => {
                                let st = self.export.get_mut(b).expect("just inserted");
                                st.ledger = current.clone();
                                st.needs_full = false;
                                st.resync_requested = false;
                                full_exchanges += 1;
                                if was_resync {
                                    resyncs_served += 1;
                                }
                            }
                            Commit::Delta { upserts, exits } => {
                                let st = self.export.get_mut(b).expect("just inserted");
                                for r in upserts {
                                    st.ledger.insert(r.wire, r);
                                }
                                for w in exits {
                                    st.ledger.remove(&w);
                                }
                                delta_exchanges += 1;
                            }
                        }
                        shipped_records += recs_shipped as u64;
                        shipped_bytes += payload;
                        equiv_full_bytes += border_payload_len(current.values());
                        records_max = records_max.max(recs_shipped);
                    }
                    Err(_) => {
                        // The exchange did NOT reach the neighbor's queue:
                        // its ledger and view stay where they were, but the
                        // seq already moved — a plain delta next tick would
                        // look like a gap to the receiver. Force a Full
                        // instead (the one-tick self-heal): it re-baselines
                        // ledger AND receiver in one message.
                        drops += 1;
                        if matches!(commit, Commit::Delta { .. }) {
                            delta_drops += 1;
                        }
                        let st = self.export.get_mut(b).expect("just inserted");
                        st.needs_full = true;
                        warn!(
                            room = %self.config.id,
                            shard = self.index,
                            neighbor = b,
                            "border send failed (neighbor channel full); \
                             that neighbor is flagged for a Full re-sync \
                             on the next tick"
                        );
                    }
                }
            }
            let s = &mut self.bstats;
            // Exports counts EXCHANGES (queued or failed), not tick×neighbor
            // slots: a silently-skipped neighbor shipped nothing and must not
            // inflate the denominators the byte/record rates divide by.
            s.exports += attempts;
            s.export_records += shipped_records;
            s.export_bytes += shipped_bytes;
            s.export_records_max = s.export_records_max.max(records_max);
            s.export_us += t5.elapsed().as_micros() as u64;
            s.export_drops += drops;
            s.delta_exchanges += delta_exchanges;
            s.full_exchanges += full_exchanges;
            s.full_resyncs_served += resyncs_served;
            s.delta_drops += delta_drops;
            s.equiv_full_bytes += equiv_full_bytes;
        }

        // -- Phase 6 — BROADCAST (the room's broadcast phase with the
        //    borrowed boundary set folded into every group's snapshot).
        self.broadcast_phase(&ctx);
        true
    }

    /// Handle one shard-channel message (phase 0). Returns `false` when
    /// the actor should stop.
    fn handle_msg(&mut self, m: ShardMsg<St, Sp>, ctx: &TickCtx) -> bool {
        match m {
            ShardMsg::Join {
                conn,
                epoch,
                out,
                reply,
            } => {
                // A join supersedes any stale state this connection had
                // (same as the room actor) — including its request state
                // (a rejoin is a NEW session: in-flight requests and
                // queued answers of the old one are dropped; their late
                // worker reports are discarded by the 0b reconciliation).
                if let Some(&stale) = self.binding.get(&conn) {
                    let _ = self.conns.remove(&stale); // old halves drop
                    self.drop_conn_request_state(conn);
                    self.logic.on_leave(&mut self.world, stale);
                }
                // Identity-space exhaustion guard (structurally unreachable
                // at the default range — see module docs): the shard
                // refuses rather than mint a colliding id.
                if self.logic.serial_used() + 1 >= self.logic.serial_range() {
                    warn!(
                        room = %self.config.id,
                        shard = self.index,
                        %conn,
                        "shard wire-id range exhausted; join rejected as \
                         RoomFull (raise the range: SHARD_SERIAL_RANGE)"
                    );
                    let _ = reply.send(Err(CoreError::RoomFull(self.config.id.0)));
                    return true;
                }
                // The LOGIC mints the stable player identity (Faz 2).
                let admission = self.logic.on_join(&mut self.world, conn);
                self.m.joins += 1;
                let (act_tx, act_rx) = mpsc::channel(self.config.action_capacity);
                self.conn_epoch.insert(conn, epoch);
                self.binding.insert(conn, admission.player);
                self.conns.insert(
                    admission.player,
                    RoomConn {
                        conn,
                        out,
                        actions: act_rx,
                        entity: admission.entity,
                        group: self.logic.group_of(&self.world, admission.player),
                        batch: Vec::new(),
                        detached: false,
                        detach_deadline: None,
                        expire_to: ExpireTo::Despawn,
                        bot_fed: false,
                        session_epoch: 0,
                    },
                );
                let _ = reply.send(Ok((admission.entity, act_tx)));
                debug!(
                    room = %self.config.id,
                    shard = self.index,
                    %conn,
                    player = %admission.player,
                    entity = admission.entity,
                    epoch,
                    "player joined shard"
                );
                true
            }
            ShardMsg::Detach {
                conn,
                entity,
                identity,
            } => {
                // The registry's close broadcast: exactly the owning shard
                // runs the policy; the binding + entity guards make the
                // others no-ops (the same shape as a broadcast `Leave`).
                if let Some(&player) = self.binding.get(&conn)
                    && self.conns.get(&player).map(|c| c.entity) == Some(entity)
                {
                    let decision =
                        self.logic.on_disconnect(&mut self.world, player, &identity);
                    match decision {
                        Detach::Despawn => {
                            // Today's close semantics, unchanged.
                            self.despawn_conn(player, false);
                        }
                        Detach::Hold { grace, to } => {
                            // Park: keep row (stable key)/entity/slot and
                            // the binding row; core owns the clock (§14.4).
                            // The dead session's in-flight requests die with
                            // it (RECONNECT §11 — the room actor's detach
                            // semantics, now mirrored): pending entries are
                            // dropped and late worker reports are silently
                            // discarded by the 0b reconciliation.
                            let rc = self.conns.get_mut(&player).expect("guarded above");
                            rc.detached = true;
                            rc.expire_to = to;
                            rc.detach_deadline = grace.map(|g| Instant::now() + g);
                            self.drop_conn_request_state(conn);
                            debug!(
                                room = %self.config.id,
                                shard = self.index,
                                %conn,
                                %player,
                                entity,
                                ?grace,
                                ?to,
                                "player detached on shard (entity parked)"
                            );
                        }
                    }
                }
                true
            }
            ShardMsg::Resume {
                conn,
                epoch,
                identity,
                out,
                reply,
            } => {
                // §6 broadcast-resume: this shard accepts ONLY if its
                // ledger holds the identity — every other shard answers
                // "not here" without touching anything.
                let outcome = match self.logic.resume_lookup(&self.world, &identity) {
                    ResumeFound::Held(player) => {
                        // The parked row, by its STABLE key (Faz 2): one
                        // lookup instead of the pre-Faz-2 scan.
                        match self.conns.get(&player) {
                            Some(rc) if rc.detached => {
                                // Copy the guard/reply values out so the
                                // table borrow ends before the rebind.
                                let rc_epoch = rc.session_epoch;
                                let entity = rc.entity;
                                // Epoch guard (§7), one comparison — see
                                // the room actor's Resume arm for the full
                                // rationale.
                                if epoch != 0 && rc_epoch != 0 && epoch <= rc_epoch {
                                    self.m.resume_rejected_stale += 1;
                                    warn!(
                                        room = %self.config.id,
                                        shard = self.index,
                                        %player,
                                        %identity,
                                        resume_epoch = epoch,
                                        "resume rejected: stale epoch"
                                    );
                                    Err(CoreError::ResumeStale)
                                } else {
                                    let act_tx =
                                        self.rebind_session(player, conn, epoch, &identity, out);
                                    Ok(Some((entity, act_tx)))
                                }
                            }
                            _ => {
                                // Ledger/table divergence (the hold expired
                                // in this very tick's sweep, or the row is
                                // already live): counted stale; the
                                // dispatcher's all-miss fallback turns it
                                // into a transparent fresh join.
                                self.m.resume_rejected_stale += 1;
                                Ok(None)
                            }
                        }
                    }
                    ResumeFound::Ended => {
                        // Mechanism-level rejection; the dispatcher turns an
                        // all-shards-miss outcome into the transparent fresh
                        // join (§5). Counted so operators see how many
                        // attempts raced (or followed) a hold's end.
                        self.m.resume_rejected_stale += 1;
                        Ok(None)
                    }
                    ResumeFound::Never => Ok(None),
                };
                let _ = reply.send(outcome);
                true
            }
            ShardMsg::Leave { conn, entity, epoch } => {
                // Stale-leave guard (binding + entity id): only the entity
                // this player currently owns.
                if let Some(&player) = self.binding.get(&conn)
                    && self.conns.get(&player).map(|c| c.entity) == Some(entity)
                {
                    self.despawn_conn(player, false);
                    self.m.leaves += 1;
                    debug!(
                        room = %self.config.id,
                        shard = self.index,
                        %conn,
                        %player,
                        entity,
                        "player left shard"
                    );
                }
                // Prune the epoch entry — in BOTH arms (the entity-matched
                // despawn above and the broadcast leave this shard had no
                // entity for). Safety: `conn_epoch` is read at exactly one
                // place — stamping an outgoing Migrate for a connection
                // LIVE in `self.conns` — and once a leave of the live
                // join's epoch is processed here, that connection cannot
                // be live on this shard any more (either the arm above
                // just despawned it, or it never lived here). Any future
                // migrate-in re-inserts the entry (see the Migrate arm),
                // and a re-JOIN is safe because the per-connection
                // dispatcher serializes ops: the rejoin carries a strictly
                // newer epoch and is processed (on this FIFO shard
                // channel) before any newer migrate-out could stamp from
                // here. Without this removal the table grew by every
                // connection that ever joined.
                self.conn_epoch.remove(&conn);
                // Leave tombstone (see module docs): a late `Migrate` of
                // the join this leave ends must be rejected here — and in
                // every other shard that also saw the leave (it is
                // broadcast to all of them). The tombstone table is kept
                // SEPARATE from `conn_epoch` (the installed join's
                // epoch): a migration of an *alive* join carries that
                // epoch legitimately and must not be mistaken for dead.
                match self.conn_tombstone.entry(conn) {
                    Entry::Occupied(mut e) => {
                        if e.get().0 < epoch {
                            // Max-update BOTH halves together: the write
                            // tick belongs to the leave that owns the
                            // surviving (highest) epoch — see the field
                            // docs. An equal-or-stale leave keeps the
                            // older entry whole (conservative: its guard,
                            // being for an equal-or-newer death, lives
                            // longer).
                            e.insert((epoch, ctx.tick));
                        }
                    }
                    Entry::Vacant(e) => {
                        e.insert((epoch, ctx.tick));
                    }
                }
                true
            }
            ShardMsg::Migrate {
                from,
                at_tick,
                wire,
                state,
                player,
            } => {
                // Install gate (module docs, "Migration protocol"): the
                // crossing was sampled by the sender at tick `at_tick` and
                // takes effect here at `at_tick + 1` — the explicit
                // one-tick alignment. A message that arrived EARLY
                // (in-process delivery: the sender's tick body and this
                // shard's tick body interleave on the runtime) is
                // deferred until the gate opens — installing it now would
                // put the entity in two shards for one tick.
                if ctx.tick <= at_tick {
                    self.deferred.push_back(ShardMsg::Migrate {
                        from,
                        at_tick,
                        wire,
                        state,
                        player,
                    });
                    return true;
                }
                // The epoch gate: reject a `Migrate` whose join this shard
                // already knows to be dead (a leave of that epoch — or a
                // newer one — was processed first; the leave/migration
                // race, in EITHER order). The gate reads the TOMBSTONE
                // table, not `conn_epoch`: a migration of an alive join
                // carries the installed epoch legitimately. Only the
                // epoch half of the tuple gates; the tick half is the
                // sweep's bookkeeping.
                if let Some(p) = &player
                    && let Some((tomb_epoch, _wrote_at)) = self.conn_tombstone.get(&p.conn)
                    && *tomb_epoch >= p.epoch
                {
                    debug!(
                        room = %self.config.id,
                        shard = self.index,
                        %from,
                        wire,
                        conn = %p.conn,
                        "migrate dropped: the join is dead (leave \
                         processed first)"
                    );
                    return true;
                }
                self.logic.on_migrate_in(
                    &mut self.world,
                    wire,
                    state,
                    player.as_ref().map(|p| p.player),
                );
                if let Some(p) = player {
                    // The player moves here: the out channel and the
                    // action inbox were MOVED with the message (ownership
                    // transfer — the connection actor never notices), and
                    // the binding row is installed so control broadcasts
                    // find this shard. Invariant (see module docs): after
                    // passing the epoch gate this shard cannot already
                    // hold the player.
                    debug_assert!(!self.conns.contains_key(&p.player));
                    self.conn_epoch.insert(p.conn, p.epoch);
                    self.binding.insert(p.conn, p.player);
                    self.conns.insert(
                        p.player,
                        RoomConn {
                            conn: p.conn,
                            out: p.out,
                            actions: p.actions,
                            entity: p.entity,
                            group: self.logic.group_of(&self.world, p.player),
                            batch: Vec::new(),
                            detached: p.detached,
                            detach_deadline: p.detach_deadline,
                            expire_to: p.expire_to,
                            bot_fed: p.bot_fed,
                            session_epoch: p.session_epoch,
                        },
                    );
                }
                debug!(
                    room = %self.config.id,
                    shard = self.index,
                    %from,
                    wire,
                    at_tick,
                    "entity migrated in"
                );
                true
            }
            ShardMsg::Border { from, exchange } => {
                // The receiver half of the §6.4 protocol: a Full replaces
                // the view and re-baselines the expected sequence (accepted
                // at ANY time — the rebuilt-incarnation path); a Delta is
                // applied atomically ONLY on an exact sequence match, and a
                // mismatch rejects it wholesale, requests a resync and
                // quarantines the view until the healing Full arrives.
                //
                // measurement scaffolding for CROSS-SHARD §7 — remove or
                // promote after the delta decision: apply-side accounting.
                // In process there is no decode step (the message arrives
                // as structs); the map insert/remove IS the "decode +
                // apply" cost a wire deployment would pay on top of its
                // deserialization. Accounted through the same helpers as
                // the sender for comparability.
                let tr = Instant::now();
                let s = &mut self.bstats;
                match exchange {
                    BorderExchange::Full { seq, entities, .. } => {
                        let recs = entities.len();
                        let bytes = border_payload_len(entities.iter());
                        let view = self.border.entry(from).or_default();
                        view.recs = entities.into_iter().map(|r| (r.wire, r)).collect();
                        view.expected_seq = seq.wrapping_add(1);
                        view.stale_until_full = false;
                        s.imports += 1;
                        s.import_records += recs as u64;
                        s.import_bytes += bytes;
                    }
                    BorderExchange::Delta {
                        seq,
                        upserts,
                        exits,
                        ..
                    } => {
                        let view = self.border.entry(from).or_default();
                        if view.stale_until_full || seq != view.expected_seq {
                            // Continuity broken (a lost exchange, or this is
                            // a stale incarnation's message after a rebuild):
                            // never apply — an unknown-sized hole can leave
                            // ghosts and stale positions that no later delta
                            // can name. Quarantine + ask upstream for a Full
                            // (pin 3a). The request itself rides the same
                            // bounded mailbox with try_send: if THAT drops,
                            // the periodic cadence (3c) still heals us, just
                            // slower.
                            view.stale_until_full = true;
                            s.resync_requests_sent += 1;
                            // Only a FULL link logs: the transient case is
                            // worth a line, a closed one means the peer
                            // incarnation is gone and the death watcher
                            // already owns that story.
                            if let Some(link) = self.links.get_mut(from)
                                && let Err(e) =
                                    link.send(ShardMsg::ResyncRequest { from: self.index })
                                && matches!(e, LinkFull::Full { .. })
                            {
                                debug!(
                                    room = %self.config.id,
                                    shard = self.index,
                                    %from,
                                    "resync request dropped (neighbor channel \
                                     full); the periodic Full remains the \
                                     backstop"
                                );
                            }
                        } else {
                            let ups = upserts.len();
                            let exs = exits.len();
                            let bytes = delta_payload_len(&upserts, exs);
                            for r in upserts {
                                view.recs.insert(r.wire, r);
                            }
                            for w in exits {
                                view.recs.remove(&w);
                            }
                            view.expected_seq = seq.wrapping_add(1);
                            view.stale_until_full = false;
                            s.imports += 1;
                            s.import_records += (ups + exs) as u64;
                            s.import_bytes += bytes;
                        }
                    }
                }
                s.import_us += tr.elapsed().as_micros() as u64;
                true
            }
            ShardMsg::ResyncRequest { from } => {
                // A neighbor rejected our delta stream: serve it a Full on
                // the next phase 5 (the flag makes the answer deterministic
                // — no timing race, and the Full is generated after every
                // already-queued message, so ordering stays natural).
                let st = self.export.entry(from).or_default();
                st.needs_full = true;
                st.resync_requested = true;
                true
            }
            ShardMsg::Shutdown => false,
        }
    }

    /// Bind a resumed session onto its parked row (the shard-side
    /// mirror of the room actor's `rebind_session`): swap the channel
    /// halves, stamp the guard epoch, move THE binding row, and hand
    /// the logic its `on_resume` hook. Returns the fresh action sender
    /// for the reply.
    ///
    /// **The RebindKey shrink (Faz 2)** — the signpost enumeration, now a
    /// list of what must NOT be touched (see the room actor for the full
    /// rationale):
    /// - `binding` — MOVED below (`old conn → new conn`, same player):
    ///   the entire re-key surface;
    /// - `conns` — NOT re-keyed: the row's key IS the stable player id;
    ///   only `RoomConn.conn` and the channel halves are rewritten;
    /// - `conn_epoch` — MOVED as part of the binding move (remove the
    ///   dead session's entry, stamp the resume epoch under the new one:
    ///   outgoing Migrates of the LIVE session must carry ITS epoch so
    ///   the leave/migration gate pairs correctly);
    /// - `conn_tombstone` — NOT touched, deliberately: a tombstone is
    ///   keyed by the id of the join that DIED. A detached session never
    ///   died (its leave was never processed), so it wrote no tombstone;
    ///   tombstoned old sessions stay under their own (dead) ids where
    ///   their guards belong;
    /// - `deferred` — in-flight `Migrate`s carry the OLD id; they are
    ///   gated by epoch/tombstones exactly as before (a post-resume ghost
    ///   of the parked session loses to the new session's newer epoch on
    ///   any later leave);
    /// - `groups` — NOT touched (opaque `G`); rebuilt wholesale from
    ///   `conns` every broadcast phase, so a per-player group key
    ///   self-heals in one tick (same argument as the room actor).
    ///
    /// The ledger goes through [`GameLogic::on_resume`] (the park
    /// metadata itself traveled INSIDE the migrated player state — §14.2
    /// — so whichever shard now owns the entity also owns the record).
    fn rebind_session(
        &mut self,
        player: PlayerId,
        conn: ConnectionId,
        epoch: u64,
        identity: &str,
        out: mpsc::Sender<FrameBatch>,
    ) -> Mailbox<Action> {
        let (act_tx, act_rx) = mpsc::channel(self.config.action_capacity);
        let mut old_conn = conn;
        let mut entity = 0;
        if let Some(rc) = self.conns.get_mut(&player) {
            rc.out = out;
            rc.actions = act_rx;
            rc.detached = false;
            rc.bot_fed = false;
            rc.detach_deadline = None;
            rc.session_epoch = epoch;
            old_conn = rc.conn;
            rc.conn = conn;
            entity = rc.entity;
        }
        self.m.resumes += 1;
        // THE binding move + its session-epoch companion (see the
        // enumeration above): the old session loses both rows; the new
        // session owns the player from here on.
        let moved_epoch = self.conn_epoch.remove(&old_conn);
        self.binding.remove(&old_conn);
        self.binding.insert(conn, player);
        self.conn_epoch.insert(conn, epoch.max(moved_epoch.unwrap_or(0)));
        self.logic.on_resume(&mut self.world, identity, conn, player, entity);
        debug!(
            room = %self.config.id,
            shard = self.index,
            %old_conn,
            %conn,
            %player,
            epoch,
            "player resumed onto parked row on this shard (binding moved)"
        );
        act_tx
    }

    /// The shard's despawn funnel: remove the row, tear down its binding,
    /// run `on_leave`, count. (`on_leave` stays THE single despawn seam
    /// for snapshots and bookkeeping, exactly like the room actor.)
    fn despawn_conn(&mut self, player: PlayerId, count_as_leave: bool) {
        let Some(rc) = self.conns.remove(&player) else {
            return;
        };
        self.binding.remove(&rc.conn);
        self.conn_epoch.remove(&rc.conn);
        // The request state goes with the SESSION (the room actor's rule):
        // in-flight requests release their slots and any queued answer is
        // dropped (a reply to a gone session is not delivered); late
        // worker reports find no pending entry and are counted late.
        self.drop_conn_request_state(rc.conn);
        self.logic.on_leave(&mut self.world, player);
        if count_as_leave {
            self.m.leaves += 1;
        }
    }

    /// Queue one RPC answer for a connection's next (or this tick's, if
    /// broadcast has not run yet) private frame. All request paths —
    /// same-tick reply/reject, cap/duplicate rejects, the worker-report
    /// reconciliation, the timeout sweep — funnel through here, so the
    /// per-tick delivery point is exactly one. (The room actor's helper,
    /// byte-for-byte.)
    fn queue_reply(
        &mut self,
        conn: ConnectionId,
        id: u64,
        op: u16,
        ok: bool,
        reason: String,
        payload: bytes::Bytes,
    ) {
        self.queued.entry(conn).or_default().push(RpcReply {
            id,
            ok,
            op,
            reason,
            payload,
        });
    }

    /// Drop a connection's request state (pending set + queued answers).
    /// Called on leave, on join (a join supersedes the connection's prior
    /// state), on detach-park, and at migrate-out. Late worker reports for
    /// the dropped requests find no pending entry and are dropped by the
    /// 0b reconciliation; the workers themselves exit on their own (their
    /// report send fails against the dropped entry, or their timeout
    /// fires first). (The room actor's helper, byte-for-byte.)
    fn drop_conn_request_state(&mut self, conn: ConnectionId) {
        if let Some(deq) = self.pending.remove(&conn) {
            self.pending_total = self.pending_total.saturating_sub(deq.len());
        }
        self.queued.remove(&conn);
    }


    /// Phase 6: the room's broadcast phase, with the borrowed boundary set
    /// (the latest exchange per neighbor, flattened and sorted by wire for
    /// deterministic payload order) folded into every group's snapshot.
    fn broadcast_phase(&mut self, ctx: &TickCtx) {
        let snap_op = self.logic.snapshot_op();
        let priv_op = self.logic.private_op();

        // 6a. Recompute each player's group.
        for (player, rc) in self.conns.iter_mut() {
            rc.group = self.logic.group_of(&self.world, *player);
        }

        // 6b. Rebuild the group table (same as the room's 4b).
        let mut members: HashMap<G, Vec<PlayerId>> = HashMap::new();
        for (&player, rc) in &self.conns {
            members.entry(rc.group.clone()).or_default().push(player);
        }
        self.m.step_max_group =
            members.values().map(Vec::len).max().unwrap_or(0) as u32;
        let gone: Vec<G> = self
            .groups
            .keys()
            .filter(|g| !members.contains_key(*g))
            .cloned()
            .collect();
        for g in gone {
            self.groups.remove(&g);
        }
        for (g, m) in members {
            match self.groups.entry(g) {
                Entry::Vacant(e) => {
                    e.insert(GroupState {
                        last: None,
                        sent: None,
                        never_emitted_warned: false,
                        size_warned: false,
                    });
                }
                Entry::Occupied(mut e) => {
                    let st = e.get_mut();
                    st.sent = None;
                    if st.last.is_none() && !st.never_emitted_warned {
                        st.never_emitted_warned = true;
                        let group_key = e.key();
                        warn!(
                            room = %self.config.id,
                            shard = self.index,
                            ?group_key,
                            members = m.len(),
                            "snapshot group has members but has never emitted: \
                             ShardLogic::snapshot returned `false` on the \
                             group's first tick although a fresh group's \
                             first tick is a membership change and must \
                             emit (check the logic's per-group bookkeeping)"
                        );
                    }
                }
            }
        }

        // The borrowed boundary set (module docs, "Boundary visibility"):
        // the persistent per-neighbor views flattened. Sorted by wire so
        // the payload order is deterministic. A quarantined view
        // (`stale_until_full` — a rejected delta means an unknown-sized
        // hole) is EXCLUDED: rendering possibly-diverged borrowed entities
        // would be worse than their brief absence; the healing Full
        // restores them within a tick or two.
        let mut borrowed: Vec<BorderRecord<Sp>> = Vec::new();
        for recs in self.border.values() {
            if !recs.stale_until_full {
                borrowed.extend(recs.recs.values().cloned());
            }
        }
        borrowed.sort_unstable_by_key(|r| r.wire);
        // Own records win over the neighbor's one-tick-stale borrowed copy
        // of an entity that just crossed into this shard (otherwise it
        // would appear twice in one snapshot under the same wire id):
        // filter the borrowed set against the own wires (binary search on
        // the sorted own list — the border set is small).
        let mut own = self.logic.own_wires(&self.world);
        own.sort_unstable();
        borrowed.retain(|r| own.binary_search(&r.wire).is_err());

        // 6c. One snapshot per group: encode ONCE, freeze once, share —
        //     the room's 4c with the borrowed boundary set folded in and
        //     the scratch buffer reused across groups. The keep-alive
        //     machinery mirrors the room's semantics EXACTLY (the Faz 1
        //     `GameLogic::keepalive` promotion — the shard actor's first
        //     promoted capability, `docs/TRAIT-ARCHITECTURE.md` §4): on
        //     the cadence tick, whether the group emitted this tick or
        //     not, the logic decides what ships. Full-snapshot logics
        //     keep the default (an unchanged group re-sends its cached
        //     snapshot — bit-identical: an active group's `last` IS this
        //     tick's fresh full); a delta-mode logic returns `true` with
        //     a freshly encoded FULL that REPLACES this tick's payload,
        //     healing a client that lost one or more deltas within one
        //     keep-alive period whether its group is active or silent.
        let keep_due = self
            .keepalive_every
            .map(|every| self.steps.is_multiple_of(every))
            .unwrap_or(false);
        let mut buf = bytes::BytesMut::new();
        for (group, st) in self.groups.iter_mut() {
            buf.clear();
            let emitted =
                self.logic
                    .snapshot(&mut self.world, ctx, group, &borrowed, &mut buf);
            if emitted {
                if buf.len() > self.config.max_snapshot_bytes && !st.size_warned {
                    st.size_warned = true;
                    warn!(
                        room = %self.config.id,
                        shard = self.index,
                        bytes = buf.len(),
                        max = self.config.max_snapshot_bytes,
                        "snapshot exceeds max_snapshot_bytes (rUDP MTU \
                         readiness)"
                    );
                }
                self.m.snapshots += 1;
                let n = buf.len() as u64;
                self.m.snap_bytes = self.m.snap_bytes.saturating_add(n);
                if n > self.m.snap_bytes_max as u64 {
                    self.m.snap_bytes_max = n as u32;
                }
                if n > self.config.max_snapshot_bytes as u64 {
                    self.m.snap_overflows += 1;
                }
                let payload = buf.split_to(buf.len()).freeze();
                st.sent = Some(payload.clone());
                st.last = Some(payload);
            }
            // The keep-alive decision (only when there is a cached
            // snapshot): the logic may replace this tick's payload with a
            // freshly encoded one (a delta-mode full) or keep the default
            // (re-send `last`). Same shape, same counters, same cadence
            // derivation (`keepalive_every`, clamped/warned at
            // construction exactly like the room actor's).
            if keep_due && st.last.is_some() {
                if !emitted {
                    // An unchanged group: a keep-alive re-send happened.
                    self.m.keepalive_resends += 1;
                }
                buf.clear();
                if self
                    .logic
                    .keepalive(&mut self.world, ctx, group, st.last.as_ref(), &mut buf)
                {
                    // A freshly encoded payload (a delta-mode full):
                    // counted like any other encoded snapshot.
                    if buf.len() > self.config.max_snapshot_bytes && !st.size_warned {
                        st.size_warned = true;
                        warn!(
                            room = %self.config.id,
                            shard = self.index,
                            bytes = buf.len(),
                            max = self.config.max_snapshot_bytes,
                            "snapshot exceeds max_snapshot_bytes (rUDP MTU \
                             readiness)"
                        );
                    }
                    self.m.snapshots += 1;
                    let n = buf.len() as u64;
                    self.m.snap_bytes = self.m.snap_bytes.saturating_add(n);
                    if n > self.m.snap_bytes_max as u64 {
                        self.m.snap_bytes_max = n as u32;
                    }
                    if n > self.config.max_snapshot_bytes as u64 {
                        self.m.snap_overflows += 1;
                    }
                    let payload = buf.split_to(buf.len()).freeze();
                    st.sent = Some(payload.clone());
                    st.last = Some(payload);
                } else {
                    st.sent = st.last.clone();
                }
            }
        }

        self.m.snap_records =
            self.m.snap_records.saturating_add(self.logic.encoded_records());

        // 6d. Per-connection fan-out (same as the room's 4d).
        let mut dropped: u64 = 0;
        // Same batch-buffer reuse as the room's 4d (one floor slice was the
        // per-connection per-tick `Vec::with_capacity(2)`).
        let mut pbuf = bytes::BytesMut::new();
        // RPC answers are the rare case (the room's measured rule): in a
        // quiet shard this is ONE `is_empty` probe for the whole fan-out;
        // the per-connection map probe runs only on ticks that actually
        // owe an answer.
        let has_replies = !self.queued.is_empty();
        for (&player, rc) in self.conns.iter_mut() {
            // Detached/bot-fed rows ship nothing (dead or non-human
            // outbound half; §7 — the drop counter stays "slow client"
            // only). The group snapshot still carries the parked entity.
            if rc.detached {
                continue;
            }
            rc.batch.clear();
            if let Some(payload) = self
                .groups
                .get(&rc.group)
                .and_then(|st| st.sent.clone())
            {
                self.m.shipped_frames += 1;
                self.m.shipped_bytes = self.m.shipped_bytes.saturating_add(payload.len() as u64);
                rc.batch.push(gsb_protocol::FrameBody::new(snap_op, payload));
            }
            // This connection's queued RPC answers for the tick (Faz 3:
            // same-tick local replies and, on later ticks, the reconciled
            // worker reports / timeout sweeps). The logic encodes them
            // into the private frame alongside any ack / one-shot full.
            let replies: &[RpcReply] = if has_replies {
                self.replies_buf = self.queued.remove(&rc.conn).unwrap_or_default();
                &self.replies_buf
            } else {
                &[]
            };
            pbuf.clear();
            if self
                .logic
                .private(&mut self.world, player, &rc.group, replies, &mut pbuf)
            {
                self.m.private_frames += 1;
                self.m.shipped_frames += 1;
                self.m.shipped_bytes = self.m.shipped_bytes.saturating_add(pbuf.len() as u64);
                rc.batch.push(gsb_protocol::FrameBody::new(
                    priv_op,
                    pbuf.split_to(pbuf.len()).freeze(),
                ));
            }
            if !rc.batch.is_empty() {
                let batch = std::mem::take(&mut rc.batch);
                if let Err(e) = rc.out.try_send(batch) {
                    dropped += 1;
                    rc.batch = e.into_inner();
                }
            }
        }
        // Every connection was visited above, so anything left here
        // belongs to a connection removed from the table this same tick
        // (leave/migrate) that never got its frame: drop it — a request is
        // answered exactly once, and it was never delivered.
        if !self.queued.is_empty() {
            self.queued.clear();
        }
        self.m.dropped_frames += dropped;
    }

    /// Build this shard's metrics sample. The sample id is the logical
    /// room id shifted into the shard sub-space (`room << 16 | index` —
    /// see module docs, "Metrics identity") so every shard is its own
    /// report line (the collector keys by the sample id).
    fn sample(&self) -> RoomSample {
        RoomSample {
            room: RoomId(self.config.id.0.saturating_mul(1 << 16) + self.index as u64),
            emit_at: Instant::now(),
            steps: self.steps,
            budget_us: self.budget_us,
            lagged_events: self.m.lagged_events,
            lagged_ticks: self.m.lagged_ticks,
            step_min_us: self.m.step_min_us,
            step_max_us: self.m.step_max_us,
            step_sum_us: self.m.step_sum_us,
            step_hist: self.m.step_hist,
            step_fine_hist: self.m.step_fine_hist,
            late_min_us: self.m.late_min_us,
            late_max_us: self.m.late_max_us,
            late_sum_us: self.m.late_sum_us,
            dropped_frames: self.m.dropped_frames,
            dropped_actions: self.m.dropped_actions,
            keepalive_resends: self.m.keepalive_resends,
            snapshots: self.m.snapshots,
            snap_bytes: self.m.snap_bytes,
            snap_bytes_max: self.m.snap_bytes_max,
            snap_overflows: self.m.snap_overflows,
            snap_records: self.m.snap_records,
            shipped_bytes: self.m.shipped_bytes,
            shipped_frames: self.m.shipped_frames,
            private_frames: self.m.private_frames,
            joins: self.m.joins,
            leaves: self.m.leaves,
            detached: self.conns.values().filter(|rc| rc.detached).count() as u32,
            resumes: self.m.resumes,
            resume_rejected_stale: self.m.resume_rejected_stale,
            detach_expired_despawn: self.m.detach_expired_despawn,
            detach_expired_ai: self.m.detach_expired_ai,
            // Faz 3: this shard runs the RPC machinery (the room actor's
            // counters, mirrored one-to-one) — no longer pinned to zero.
            requests_local: self.m.requests_local,
            requests_external: self.m.requests_external,
            requests_rejected_malformed: self.m.requests_rejected_malformed,
            requests_rejected_dup: self.m.requests_rejected_dup,
            requests_rejected_no_handler: self.m.requests_rejected_no_handler,
            requests_rejected_logic: self.m.requests_rejected_logic,
            requests_rejected_conn_cap: self.m.requests_rejected_conn_cap,
            requests_rejected_room_cap: self.m.requests_rejected_room_cap,
            requests_timed_out: self.m.requests_timed_out,
            requests_late: self.m.requests_late,
            pending_requests: self.pending_total as u32,
            groups: self.groups.len() as u32,
            members: self.conns.len() as u32,
            max_group: self.m.step_max_group,
            metrics_dropped: self.m.metrics_dropped,
        }
    }
}

#[cfg(test)]
mod tests {
    //! The shard protocol tests (the required invariants of the sharding
    //! round, plus the race gates):
    //!
    //! - migration never drops or duplicates an entity — including a
    //!   ping-pong across the boundary (checked per tick index);
    //! - a migrated entity keeps its wire id, and two shards' id spaces
    //!   are disjoint (no cross-shard collision);
    //! - in-flight input survives the migration (the action channel moves
    //!   with the connection and is applied by the receiving shard);
    //! - the leave/migration race is deterministic: a `Migrate` whose join
    //!   is already dead (the leave was processed first) is rejected by
    //!   the epoch gate, and a fresh join (newer epoch) is still accepted;
    //! - boundary visibility: each shard's snapshot includes the neighbor's
    //!   boundary entities (the borrowed set).
    //!
    //! The harness drives two [`ShardActor`]s off a MANUAL ticker (a
    //! broadcast sender the test feeds tick by tick), so the ordering of
    //! the shards' tick processing is deterministic and the exactly-once
    //! invariant can be checked per tick index.

    use super::*;
    use crate::room::Admission;
    use std::time::Duration;

    use crate::channel::channel;

    /// The strip payload these protocol tests use: exactly what the old
    /// core-fixed record carried (identity + truncated position), so
    /// every assertion keeps its pre-generalization meaning while the
    /// envelope becomes the generic [`BorderRecord`] around a
    /// game-owned payload.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct TStrip {
        x: i32,
        y: i32,
    }

    /// Shorthand for building one expected/actual border record.
    fn rec(wire: u64, x: i32, y: i32) -> BorderRecord<TStrip> {
        BorderRecord {
            wire,
            state: TStrip { x, y },
        }
    }

    /// The test world: wire → (x, y, mode); `mode` ∈ {-1, 0, +1} = the
    /// per-tick x step. The map is [-10, 10): shard 0 owns x < 0, shard 1
    /// owns x >= 0 (one boundary at x = 0 — the smallest topology).
    #[derive(Default, Debug, Clone)]
    struct TWorld {
        ents: HashMap<u64, (f32, f32, i8)>,
    }

    /// The migration state (the demo's shape).
    #[derive(Debug, Clone)]
    struct TState {
        x: f32,
        y: f32,
        mode: i8,
    }

    /// An observation the test logic hands the harness over its (bounded,
    /// polled) channel — the test never reaches into a shard's world
    /// directly (no shared state, no locks).
    #[derive(Debug)]
    enum Obs {
        /// This shard's world content after its phase 3 (SYSTEMS):
        /// (shard, tick, [(wire, x, y, mode)]).
        Content(usize, u64, Vec<(u64, f32, f32, i8)>),
        /// This shard reported a wire as migrating out (phase 4):
        /// (shard, tick, wire).
        Migrate(usize, u64, u64),
    }

    /// The test shard logic: two shards over one map (see `TWorld`), one
    /// snapshot group, deterministic movement (a targeted entity steps
    /// `mode` in x per tick), wire ids from the range partitioning.
    struct TLogic {
        index: usize,
        next_serial: u64,
        player_ent: HashMap<PlayerId, u64>,
        ent_player: HashMap<u64, PlayerId>,
        last_tick: u64,
        obs: mpsc::Sender<Obs>,
        ops: mpsc::Sender<(PlayerId, u16)>,
    }

    impl TLogic {
        fn region_of(x: f32) -> usize {
            if x < 0.0 {
                0
            } else {
                1
            }
        }
    }

    // Faz 1 trait split: the shared contract (snapshot groups, tick seam,
    // membership) implements the `GameLogic` supertrait; the sharding seam
    // stays on `ShardLogic`.
    impl GameLogic<TWorld> for TLogic {
        type GroupKey = ();
        type Strip = TStrip;

        fn snapshot_op(&self) -> u16 {
            0x7100
        }
        fn private_op(&self) -> u16 {
            0x7101
        }
        fn group_of(&self, _w: &TWorld, _p: PlayerId) -> Self::GroupKey {}
        fn snapshot(
            &mut self,
            w: &mut TWorld,
            _ctx: &TickCtx,
            _g: &Self::GroupKey,
            borrowed: &[BorderRecord<TStrip>],
            out: &mut bytes::BytesMut,
        ) -> bool {
            // The test's wire format: own entities + the borrowed boundary
            // set, sorted by wire, 16 bytes per record (u64 wire LE,
            // i32 x LE, i32 y LE) — the harness parses it back from the
            // connection's out channel (test 5).
            let mut recs: Vec<(u64, i32, i32)> = w
                .ents
                .iter()
                .map(|(wire, (x, y, _))| (*wire, *x as i32, *y as i32))
                .collect();
            for b in borrowed {
                recs.push((b.wire, b.state.x, b.state.y));
            }
            recs.sort_unstable_by_key(|r| r.0);
            for (wire, x, y) in &recs {
                out.extend_from_slice(&wire.to_le_bytes());
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
            }
            true
        }
        fn on_join(&mut self, w: &mut TWorld, conn: ConnectionId) -> Admission {
            // Deterministic spawn: x = (conn.0 % 20) - 10 (conn 1 → -9 in
            // shard 0; conn 10 → 0 in shard 1; conn 11 → +1 in shard 1),
            // y = 0, no motion.
            let x = (conn.0 % 20) as f32 - 10.0;
            self.next_serial += 1;
            let wire = self.serial_base() + self.next_serial;
            w.ents.insert(wire, (x, 0.0, 0));
            // Test identity policy: the conn id doubles as the player id.
            let player = PlayerId(conn.0);
            self.player_ent.insert(player, wire);
            self.ent_player.insert(wire, player);
            Admission {
                player,
                entity: wire,
            }
        }
        fn on_leave(&mut self, w: &mut TWorld, player: PlayerId) {
            if let Some(wire) = self.player_ent.remove(&player) {
                self.ent_player.remove(&wire);
                w.ents.remove(&wire);
            }
        }
        fn ingest(&mut self, w: &mut TWorld, _ctx: &TickCtx, actions: &mut Vec<Action>) {
            for a in actions.drain(..) {
                let _ = self.ops.try_send((a.player, a.op));
                // The test's ops: 1000 = step +1/tick, 1001 = step -1/tick,
                // 1002 = stop.
                let mode = match a.op {
                    1000 => 1,
                    1001 => -1,
                    _ => 0,
                };
                if let Some(wire) = self.player_ent.get(&a.player).copied()
                    && let Some(e) = w.ents.get_mut(&wire)
                {
                    e.2 = mode;
                }
            }
        }
        fn update(&mut self, w: &mut TWorld, ctx: &TickCtx) {
            self.last_tick = ctx.tick;
            let mut steps: Vec<(u64, f32, f32, i8)> = Vec::new();
            for (&wire, (x, y, mode)) in &w.ents {
                if *mode != 0 {
                    steps.push((wire, x + *mode as f32, *y, *mode));
                }
            }
            for (wire, x, y, mode) in steps {
                if let Some(e) = w.ents.get_mut(&wire) {
                    *e = (x, y, mode);
                }
            }
            // The observation: the world content AFTER this tick's step
            // (phase 3 — before the phase-4 migration bookkeeping, which
            // is accounted for by the harness's "reported out last tick"
            // rule — see `owners_at`).
            let mut c: Vec<(u64, f32, f32, i8)> =
                w.ents.iter().map(|(wire, e)| (*wire, e.0, e.1, e.2)).collect();
            c.sort_unstable_by_key(|e| e.0);
            let _ = self.obs.try_send(Obs::Content(self.index, ctx.tick, c));
        }
    }

    impl ShardLogic<TWorld> for TLogic {
        type State = TState;

        fn index(&self) -> usize {
            self.index
        }
        fn shard_count(&self) -> usize {
            2
        }
        fn serial_base(&self) -> u64 {
            self.index as u64 * SHARD_SERIAL_RANGE
        }
        fn serial_range(&self) -> u64 {
            SHARD_SERIAL_RANGE
        }
        fn serial_used(&self) -> u64 {
            self.next_serial
        }
        fn neighbors(&self) -> &[usize] {
            if self.index == 0 {
                &[1]
            } else {
                &[0]
            }
        }
        fn collect_migrations(&mut self, w: &mut TWorld, neighbor: usize) -> Vec<Migrating<TState>> {
            let mut out = Vec::new();
            for (&wire, (x, y, mode)) in &w.ents {
                if TLogic::region_of(*x) == neighbor {
                    let _ = self.obs.try_send(Obs::Migrate(self.index, self.last_tick, wire));
                    out.push(Migrating {
                        wire,
                        state: TState {
                            x: *x,
                            y: *y,
                            mode: *mode,
                        },
                        player: self.ent_player.get(&wire).copied(),
                    });
                }
            }
            out
        }
        fn on_migrate_in(
            &mut self,
            w: &mut TWorld,
            wire: u64,
            state: TState,
            player: Option<PlayerId>,
        ) {
            w.ents.insert(wire, (state.x, state.y, state.mode));
            if let Some(p) = player {
                self.player_ent.insert(p, wire);
                self.ent_player.insert(wire, p);
            }
        }
        fn on_migrate_out(&mut self, w: &mut TWorld, wire: u64) {
            if let Some(p) = self.ent_player.remove(&wire) {
                self.player_ent.remove(&p);
            }
            w.ents.remove(&wire);
        }
        fn collect_border(&self, w: &TWorld) -> Vec<BorderRecord<TStrip>> {
            // Border = entities within 1 unit of the region edge (x = 0).
            w.ents
                .iter()
                .filter(|(_, (x, _, _))| x.abs() <= 1.0)
                .map(|(wire, (x, y, _))| rec(*wire, *x as i32, *y as i32))
                .collect()
        }
        fn own_wires(&self, w: &TWorld) -> Vec<u64> {
            w.ents.keys().copied().collect()
        }
    }

    /// The harness: two shards off a manual ticker; the test feeds ticks
    /// and observes each shard's world content (and the migration
    /// reports) through the obs channel.
    struct Harness {
        tick_tx: broadcast::Sender<TickInfo>,
        shard_txs: [Mailbox<ShardMsg<TState, TStrip>>; 2],
        obs: mpsc::Receiver<Obs>,
        ops: mpsc::Receiver<(PlayerId, u16)>,
        #[allow(dead_code)]
        handles: Vec<tokio::task::JoinHandle<()>>,
        t: u64,
        /// Migration reports: (tick, from-shard) → wires.
        migrated: HashMap<(u64, usize), Vec<u64>>,
        /// The latest completed tick's content, per shard.
        content: [Vec<(u64, f32, f32, i8)>; 2],
    }

    fn metrics_null() -> mpsc::Sender<MetricsEvent> {
        let (tx, _rx) = mpsc::channel(1);
        tx
    }

    impl Harness {
        fn new() -> Self {
            let (tick_tx, _) = broadcast::channel(64);
            let (tx0, rx0) = channel::<ShardMsg<TState, TStrip>>(128);
            let (tx1, rx1) = channel::<ShardMsg<TState, TStrip>>(128);
            let (obs_tx, obs_rx) = mpsc::channel(4096);
            let (ops_tx, ops_rx) = mpsc::channel(4096);
            let (dummy_tx, _dummy_rx) = channel::<ShardMsg<TState, TStrip>>(1);
            let cfg = RoomConfig {
                id: RoomId(7),
                keepalive_hz: 0.0, // silence the keep-alive re-sends
                metrics_cadence_hz: 0.0,
                ..Default::default()
            };
            let h0 = tokio::spawn(
                ShardActor::new(
                    cfg.clone(),
                    0,
                    TWorld::default(),
                    Box::new(TLogic {
                        index: 0,
                        next_serial: 0,
                        player_ent: HashMap::new(),
                        ent_player: HashMap::new(),
                        last_tick: 0,
                        obs: obs_tx.clone(),
                        ops: ops_tx.clone(),
                    }),
                    tick_tx.subscribe(),
                    rx0,
                    // Indexed by the receiver's shard index (the actor's
                    // lookup); slot 0 (itself) is the dummy.
                    vec![dummy_tx.clone(), tx1.clone()],
                    1,
                    metrics_null(),
                    None, // no result sink in the protocol harness
                )
                .run(),
            );
            let h1 = tokio::spawn(
                ShardActor::new(
                    cfg,
                    1,
                    TWorld::default(),
                    Box::new(TLogic {
                        index: 1,
                        next_serial: 0,
                        player_ent: HashMap::new(),
                        ent_player: HashMap::new(),
                        last_tick: 0,
                        obs: obs_tx.clone(),
                        ops: ops_tx,
                    }),
                    tick_tx.subscribe(),
                    rx1,
                    vec![tx0.clone(), dummy_tx],
                    1,
                    metrics_null(),
                    None, // no result sink in the protocol harness
                )
                .run(),
            );
            drop(obs_tx);
            Harness {
                tick_tx,
                shard_txs: [tx0, tx1],
                obs: obs_rx,
                ops: ops_rx,
                handles: vec![h0, h1],
                t: 0,
                migrated: HashMap::new(),
                content: [Vec::new(), Vec::new()],
            }
        }

        /// Feed one tick to the manual ticker and wait until BOTH shards
        /// have reported their content for it (the obs channel carries the
        /// per-shard, per-tick observations; a tick is "done" when both
        /// shards reported it). Returns the two shards' content.
        async fn tick(&mut self) -> [Vec<(u64, f32, f32, i8)>; 2] {
            self.t += 1;
            let t = self.t;
            self.tick_tx
                .send(TickInfo {
                    tick: t,
                    at: Instant::now(),
                })
                .expect("at least one subscriber");
            let mut content: [Vec<(u64, f32, f32, i8)>; 2] = [Vec::new(), Vec::new()];
            // Track which shards have REPORTED this tick (an empty shard
            // reports an empty vec, so emptiness is not a done-signal).
            let mut reported = [false; 2];
            let deadline = Instant::now() + Duration::from_secs(5);
            while !reported.iter().all(|&r| r) {
                let wait = deadline.saturating_duration_since(Instant::now());
                let item = match tokio::time::timeout(wait, self.obs.recv()).await {
                    Ok(x) => x.expect("obs channel closed"),
                    Err(_) => {
                        panic!(
                            "shards did not both process tick {} in time; \
                             reported={reported:?} got={content:?}",
                            t
                        );
                    }
                };
                match item {
                    Obs::Content(s, tick, c) if tick == t && !reported[s] => {
                        content[s] = c;
                        reported[s] = true;
                    }
                    Obs::Migrate(s, tick, wire) => {
                        self.migrated.entry((tick, s)).or_default().push(wire);
                    }
                    _ => {}
                }
            }
            self.content = content;
            self.content.clone()
        }

        /// Join `conn` to `shard` (the registry's home-shard routing is
        /// the server's business; the test picks the shard directly).
        /// The join is processed on the next tick; returns (wire, the
        /// action channel, the snapshot out channel).
        async fn join(
            &mut self,
            shard: usize,
            conn: ConnectionId,
            epoch: u64,
        ) -> (u64, mpsc::Sender<Action>, mpsc::Receiver<FrameBatch>) {
            let (reply_tx, reply_rx) = oneshot::channel();
            let (out_tx, out_rx) = mpsc::channel(64);
            self.shard_txs[shard]
                .send(ShardMsg::Join {
                    conn,
                    epoch,
                    out: out_tx,
                    reply: reply_tx,
                })
                .await
                .expect("shard channel open");
            // Drive the tick that processes the join (its CONTROL phase
            // sends the reply), then collect the reply. In the test logic
            // the entity id IS the wire id.
            let _ = self.tick().await;
            let (wire, actions) = reply_rx
                .await
                .expect("join reply delivered")
                .expect("join succeeded");
            (wire, actions, out_rx)
        }

        /// Send one input action (the connection actor's role).
        async fn act(&self, actions: &mpsc::Sender<Action>, conn: ConnectionId, op: u16) {
            actions
                .send(Action {
                    conn,
                    // Test identity policy (matches TLogic::on_join): the
                    // conn id doubles as the player id.
                    player: PlayerId(conn.0),
                    op,
                    payload: bytes::Bytes::new(),
                })
                .await
                .expect("action channel open");
        }

        /// The registry's leave broadcast: to ALL shards (exactly one owns
        /// the connection; the others no-op on the entity-id guard).
        async fn leave(&self, conn: ConnectionId, entity: EntityId, epoch: u64) {
            for shard in 0..2 {
                self.shard_txs[shard]
                    .send(ShardMsg::Leave {
                        conn,
                        entity,
                        epoch,
                    })
                    .await
                    .expect("shard channel open");
            }
        }

        /// The ops the shards ingested, in order (drain).
        async fn ops_drained(&mut self) -> Vec<(PlayerId, u16)> {
            let mut out = Vec::new();
            while let Ok(op) = self.ops.try_recv() {
                out.push(op);
            }
            out
        }
    }

    // -----------------------------------------------------------------
    // Table-pruning locks: the connection tables must not grow forever
    // with connection churn (`conn_epoch` pruned on Leave; tombstones
    // TTL'd + swept). These drive a BARE actor (not spawned): the
    // harness above can only see observable wire behavior — the right
    // level for the protocol invariants, but too coarse for "is this
    // exact map entry gone". A child module may touch the private
    // tables directly; the assertions below are the behavior locks.
    // -----------------------------------------------------------------

    /// An unspawned shard actor for the table tests above/below.
    fn bare_shard(index: usize) -> ShardActor<TWorld, (), TState, TStrip> {
        let (_tick_tx, tick_rx) = broadcast::channel(64);
        let (_self_tx, rx) = channel::<ShardMsg<TState, TStrip>>(16);
        // Two dummy neighbor slots (TLogic::neighbors targets 0 and 1).
        let (n0, _n0_rx) = channel::<ShardMsg<TState, TStrip>>(8);
        let (n1, _n1_rx) = channel::<ShardMsg<TState, TStrip>>(8);
        let (obs, _obs_rx) = mpsc::channel(16);
        let (ops, _ops_rx) = mpsc::channel(16);
        ShardActor::new(
            RoomConfig {
                id: RoomId(9),
                keepalive_hz: 0.0,
                metrics_cadence_hz: 0.0,
                ..Default::default()
            },
            index,
            TWorld::default(),
            Box::new(TLogic {
                index,
                next_serial: 0,
                player_ent: HashMap::new(),
                ent_player: HashMap::new(),
                last_tick: 0,
                obs,
                ops,
            }),
            tick_rx,
            rx,
            vec![n0, n1],
            1,
            metrics_null(),
            None, // no result sink
        )
    }

    fn tctx(tick: u64) -> TickCtx {
        TickCtx {
            room: RoomId(9),
            tick,
            dt: Duration::from_secs_f64(1.0 / 30.0),
        }
    }

    fn tinfo(tick: u64) -> TickInfo {
        TickInfo {
            tick,
            at: Instant::now(),
        }
    }

    /// Drive one Join through `handle_msg`; returns the minted entity.
    async fn join_direct(
        a: &mut ShardActor<TWorld, (), TState, TStrip>,
        conn: ConnectionId,
        epoch: u64,
        tick: u64,
    ) -> EntityId {
        let (reply_tx, reply_rx) = oneshot::channel();
        let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(8);
        assert!(
            a.handle_msg(
                ShardMsg::Join {
                    conn,
                    epoch,
                    out: out_tx,
                    reply: reply_tx
                },
                &tctx(tick)
            ),
            "a join must never stop the actor"
        );
        reply_rx
            .await
            .expect("join reply delivered")
            .expect("join ok")
            .0
    }

    fn ghost_migrate(
        conn: ConnectionId,
        epoch: u64,
        entity: EntityId,
        at_tick: u64,
    ) -> ShardMsg<TState, TStrip> {
        let (out, _out_rx) = mpsc::channel::<FrameBatch>(8);
        let (_act_tx, act_rx) = mpsc::channel::<Action>(8);
        ShardMsg::Migrate {
            from: 0,
            at_tick,
            wire: entity,
            state: TState {
                x: -7.0,
                y: 0.0,
                mode: 0,
            },
            player: Some(PlayerMigration {
                // Test identity policy: the conn id doubles as the player.
                player: PlayerId(conn.0),
                conn,
                epoch,
                entity,
                out,
                actions: act_rx,
                detached: false,
                detach_deadline: None,
                expire_to: crate::room::ExpireTo::Despawn,
                bot_fed: false,
                session_epoch: 0,
            }),
        }
    }

    /// Table-prune lock 1 — a Leave removes the connection's `conn_epoch`
    /// entry in BOTH arms: the entity-matched despawn AND the broadcast
    /// leave this shard held no matching entity for. The re-join path is
    /// asserted too (the prune is only safe because re-joins and
    /// migrate-ins re-insert).
    #[tokio::test]
    async fn leave_prunes_the_epoch_entry() {
        let mut a = bare_shard(0);

        // Arm 1: the entity-matched despawn.
        let conn = ConnectionId(4);
        let entity = join_direct(&mut a, conn, 7, 10).await;
        assert_eq!(a.conn_epoch.get(&conn), Some(&7));
        assert!(a.handle_msg(
            ShardMsg::Leave {
                conn,
                entity,
                epoch: 7
            },
            &tctx(11)
        ));
        assert!(
            !a.conn_epoch.contains_key(&conn),
            "the matched leave must prune the epoch entry"
        );
        assert_eq!(
            a.conn_tombstone.get(&conn),
            Some(&(7, 11)),
            "and write its tombstone (epoch, write tick)"
        );

        // Arm 2: the registry broadcasts every leave to all shards; here
        // the leave carries an entity this shard does NOT hold for that
        // connection (the stale-leave guard keeps the connection row —
        // it belongs to a live join), yet its epoch entry is still
        // pruned: the leave proves that join is dead HERE.
        let other = ConnectionId(5);
        let other_entity = join_direct(&mut a, other, 3, 12).await;
        assert_eq!(a.conn_epoch.get(&other), Some(&3));
        assert!(a.handle_msg(
            ShardMsg::Leave {
                conn: other,
                entity: other_entity.wrapping_add(1),
                epoch: 3
            },
            &tctx(13)
        ));
        assert!(
            a.conns.contains_key(&PlayerId(other.0)),
            "the stale-leave guard keeps the live join's row"
        );
        assert!(
            !a.conn_epoch.contains_key(&other),
            "the unmatched arm still prunes the epoch entry"
        );

        // Re-join safety (the prune's documented counterpart): a fresh
        // join carries a strictly newer epoch and re-inserts.
        join_direct(&mut a, other, 4, 14).await;
        assert_eq!(
            a.conn_epoch.get(&other),
            Some(&4),
            "re-join re-inserts the epoch entry"
        );
    }

    /// Table-prune lock 2 — the tombstone gate keeps rejecting a stale
    /// Migrate within the TTL window, then the tombstone expires at the
    /// first sweep past TTL + cadence, after which the same migration is
    /// accepted again (the observable accept-path of expiry; the
    /// migrate-in re-insertion of `conn_epoch` is asserted as well).
    #[tokio::test]
    async fn stale_migrate_rejected_then_tombstone_expires() {
        let mut a = bare_shard(1);
        let conn = ConnectionId(6);
        let wire = join_direct(&mut a, conn, 2, 100).await;
        assert!(a.handle_msg(
            ShardMsg::Leave {
                conn,
                entity: wire,
                epoch: 2
            },
            &tctx(101)
        ));
        assert_eq!(
            a.conn_tombstone.get(&conn),
            Some(&(2, 101)),
            "the leave wrote the tombstone (epoch, write tick)"
        );

        // The ghost arrives WITHIN the TTL window. `at_tick < ctx.tick`
        // so the install gate is open — only the epoch gate can stop it.
        // Existing behavior preserved: rejected.
        assert!(a.handle_msg(ghost_migrate(conn, 2, wire, 99), &tctx(102)));
        assert!(
            !a.conns.contains_key(&PlayerId(conn.0)),
            "the ghost migrate must not install the dead join"
        );
        assert_eq!(
            a.conn_tombstone.get(&conn),
            Some(&(2, 101)),
            "rejection leaves the tombstone untouched"
        );

        // Advance the clock through CONTROL phases (which run the lazy
        // sweep). First sweep ever → runs immediately at tick 200:
        // tombstone age 99 < TTL, kept. At tick 712 (512 past the last
        // sweep) the next sweep fires: age 611 >= TTL → expired.
        assert!(a.step_phases(&tinfo(200)));
        assert_eq!(
            a.conn_tombstone.get(&conn),
            Some(&(2, 101)),
            "inside the TTL window the sweep keeps the guard"
        );
        assert!(a.step_phases(&tinfo(712)));
        assert!(
            !a.conn_tombstone.contains_key(&conn),
            "past TTL + a sweep boundary the tombstone expires"
        );

        // Observable accept-path: the same (stale) migration now passes
        // the gate — proving expiry opened it — and migrate-in
        // re-inserts the pruned epoch entry (see CHANGE 1's comment).
        assert!(a.handle_msg(ghost_migrate(conn, 2, wire, 700), &tctx(713)));
        assert!(
            a.conns.contains_key(&PlayerId(conn.0)),
            "with the tombstone expired the gate no longer rejects"
        );
        assert_eq!(
            a.conn_epoch.get(&conn),
            Some(&2),
            "migrate-in re-inserts the epoch entry"
        );
    }

    /// Whether shard `s` reported `wire` as migrating out at tick `t-1`
    /// (the subtlety: that despawn happens in tick `t`'s phase 4 — AFTER
    /// the content observation — so the raw content of tick `t` still
    /// shows it there, and the protocol ownership is "raw content minus
    /// last tick's reports").
    fn reported_out(migrated: &HashMap<(u64, usize), Vec<u64>>, t: u64, s: usize, wire: u64) -> bool {
        migrated
            .get(&(t.saturating_sub(1), s))
            .map(|v| v.contains(&wire))
            .unwrap_or(false)
    }

    /// The shard(s) that own `wire` at tick index `t` per the protocol.
    fn owners_at(
        content: &[Vec<(u64, f32, f32, i8)>; 2],
        migrated: &HashMap<(u64, usize), Vec<u64>>,
        t: u64,
        wire: u64,
    ) -> Vec<usize> {
        (0..2)
            .filter(|&s| {
                content[s].iter().any(|(w, _, _, _)| *w == wire)
                    && !reported_out(migrated, t, s, wire)
            })
            .collect()
    }

    /// Required test 1 — migration never drops or duplicates an entity,
    /// including a ping-pong across the boundary. The entity walks right
    /// (shard 0 → 1), then left (1 → 0), then right again: at EVERY tick
    /// index it is in exactly one shard, and its position advances by
    /// exactly the tick's mode (no teleports, no gaps — a one-tick loss
    /// would show up as a double step).
    #[tokio::test]
    async fn migration_never_drops_or_duplicates() {
        let mut h = Harness::new();
        let conn = ConnectionId(1);
        let (wire, actions, _out) = h.join(0, conn, 1).await;
        // join() consumed tick 1: the entity is in shard 0 at x = -9.
        assert_eq!(h.content[0].len(), 1);
        assert_eq!(h.content[0][0].0, wire);
        let mut prev_x = h.content[0][0].1;

        // Walk right from x = -9; the crossing into shard 1 happens when
        // the post-step position reaches x = 0.
        h.act(&actions, conn, 1000).await;
        // Flip direction once in shard 1 (it crosses back), then again.
        let mut flipped = false;
        for _ in 0..40 {
            let c = h.tick().await;
            let t = h.t;
            // Exactly one shard owns the entity at this tick index.
            let owners = owners_at(&c, &h.migrated, t, wire);
            assert_eq!(
                owners.len(),
                1,
                "tick {t}: wire {wire} owned by {owners:?} (must be exactly one)"
            );
            let Some((x, mode)) = c[owners[0]].iter().find_map(|e| {
                if e.0 == wire {
                    Some((e.1, e.3))
                } else {
                    None
                }
            }) else {
                panic!("tick {t}: owner {} lost the entity", owners[0]);
            };
            // Position continuity: the step applied this tick equals the
            // mode in force for the tick (ingest runs before the step in
            // the same tick, so the tick's own content mode is the one
            // applied).
            assert!(
                (x - prev_x - mode as f32).abs() < 1e-6,
                "tick {t}: position jumped {prev_x} -> {x} (mode {mode})"
            );
            prev_x = x;
            // Flip direction once the entity is in shard 1 (it will cross
            // back), then again once it is back in shard 0.
            if owners[0] == 1 && !flipped {
                h.act(&actions, conn, 1001).await;
                flipped = true;
            } else if owners[0] == 0 && flipped {
                h.act(&actions, conn, 1000).await;
            }
        }
        // The ping-pong actually happened (the entity crossed into shard 1
        // and back).
        assert!(flipped, "the entity never crossed into shard 1");
        assert!(
            h.migrated
                .iter()
                .any(|((_, s), v)| *s == 1 && v.contains(&wire)),
            "the entity never crossed back into shard 0"
        );
    }

    /// Required test 2 — wire identity: a migrated entity keeps its id;
    /// the two shards' id spaces are disjoint (no cross-shard collision).
    #[tokio::test]
    async fn wire_identity_stable_and_disjoint() {
        let mut h = Harness::new();
        let (w0, actions0, _o0) = h.join(0, ConnectionId(1), 1).await; // x = -9, shard 0
        let (w1, _a1, _o1) = h.join(1, ConnectionId(10), 1).await; // x = 0, shard 1
        // Disjoint ranges: shard 0 below 2^20, shard 1 at/above it.
        assert!(w0 < SHARD_SERIAL_RANGE, "shard 0 minted out of range: {w0}");
        assert!(
            (SHARD_SERIAL_RANGE..2 * SHARD_SERIAL_RANGE).contains(&w1),
            "shard 1 minted out of range: {w1}"
        );
        assert_ne!(w0, w1);
        // Walk w0 into shard 1; it must arrive under the SAME id.
        h.act(&actions0, ConnectionId(1), 1000).await;
        let mut crossed_at = None;
        for _ in 0..30 {
            let c = h.tick().await;
            if c[1].iter().any(|(w, _, _, _)| *w == w0)
                && !reported_out(&h.migrated, h.t, 1, w0)
            {
                crossed_at = Some(h.t);
                break;
            }
        }
        let Some(t) = crossed_at else {
            panic!("w0 never crossed into shard 1");
        };
        // The crossing was reported by shard 0 at t-1 (it despawns at t).
        assert!(
            reported_out(&h.migrated, t, 0, w0),
            "shard 0 did not report the crossing of w0"
        );
        // Both entities coexist in shard 1 under their own ids (no
        // collision: w0 and w1 are distinct records in the same world).
        let c = h.content;
        let ids: Vec<u64> = c[1].iter().map(|(w, _, _, _)| *w).collect();
        assert!(ids.contains(&w0) && ids.contains(&w1), "ids: {ids:?}");
        assert_eq!(ids.len(), 2);
    }

    /// Required test 3 — in-flight input survives the migration: the
    /// action is in the channel while the connection is in transit; it is
    /// applied by the RECEIVING shard (the channel moved with the
    /// connection).
    #[tokio::test]
    async fn in_flight_action_survives_migration() {
        let mut h = Harness::new();
        let conn = ConnectionId(1);
        let (wire, actions, _out) = h.join(0, conn, 1).await;
        // Walk right; cross into shard 1.
        h.act(&actions, conn, 1000).await;
        let mut crossed_at = None;
        for _ in 0..30 {
            h.tick().await;
            if reported_out(&h.migrated, h.t, 0, wire) {
                crossed_at = Some(h.t);
                break;
            }
        }
        let Some(t_cross) = crossed_at else {
            panic!("no crossing");
        };
        // IN FLIGHT now: shard 0 just sent the migration (tick t_cross);
        // the connection halves are in transit (or just installed in
        // shard 1). Queue a mode change — it lands in the same channel
        // object the Migrate message carried to shard 1, wherever that
        // object currently sits.
        h.act(&actions, conn, 1001).await;
        // The next tick: shard 1 pulls it in its READ phase and applies it
        // (mode -1) in its step. If the action had been lost, the mode
        // would still be +1.
        let c = h.tick().await;
        let t = h.t;
        let owners = owners_at(&c, &h.migrated, t, wire);
        assert_eq!(owners.len(), 1, "tick {t}: owners {owners:?}");
        let (x, mode) = c[owners[0]]
            .iter()
            .find(|(w, _, _, _)| *w == wire)
            .map(|e| (e.1, e.3))
            .expect("entity present");
        assert_eq!(
            mode, -1,
            "the in-flight action was not applied by the receiving shard \
             (tick {t}, crossing at {t_cross}): mode {mode}"
        );
        // The entity stepped LEFT (toward shard 0) on this tick — the
        // in-flight action's mode, not the pre-migration one (+1). The
        // action was queued after the receiving shard's READ for the
        // spawn tick, so it is applied exactly one tick later: from x = 1
        // (the spawn tick's own +1 step) to x = 0.
        assert!((x - 0.0).abs() < 1e-6, "position {x} (expected 0)");
        // The action was ingested exactly once (shard 0's READ for the
        // crossing tick already ran before the send; only shard 1 can
        // pull it now).
        let ops = h.ops_drained().await;
        let n = ops.iter().filter(|(p, op)| *p == PlayerId(conn.0) && *op == 1001).count();
        assert_eq!(n, 1, "ops: {ops:?}");
    }

    /// Faz 2 lock — player identity is stable ACROSS SHARD MIGRATION: the
    /// same human keeps ONE [`PlayerId`] from before the crossing to
    /// after it, and the receiving shard ingests its input under that id
    /// (the identity rides the `PlayerMigration`, exactly like the wire
    /// id rides the entity state). Combined with the room-side resume
    /// locks this pins the contract "resume/migration move the SESSION,
    /// never the player".
    #[tokio::test]
    async fn player_identity_is_stable_across_migration() {
        let mut h = Harness::new();
        let conn = ConnectionId(1);
        let pid = PlayerId(conn.0); // the test logic's minting policy
        let (wire, actions, _out) = h.join(0, conn, 1).await;
        // One action BEFORE the migration: ingested by shard 0 under pid.
        h.act(&actions, conn, 1001).await;
        h.tick().await;
        // Walk right; cross into shard 1.
        h.act(&actions, conn, 1000).await;
        let mut crossed_at = None;
        for _ in 0..30 {
            h.tick().await;
            if reported_out(&h.migrated, h.t, 0, wire) {
                crossed_at = Some(h.t);
                break;
            }
        }
        assert!(crossed_at.is_some(), "no crossing");
        // Let the receiving shard install the row and pull one more input.
        h.tick().await;
        h.act(&actions, conn, 1002).await;
        h.tick().await;

        // Every observed op — on EITHER side of the seam — belongs to the
        // SAME stable player.
        let ops = h.ops_drained().await;
        assert!(
            ops.contains(&(pid, 1001)) && ops.contains(&(pid, 1002)),
            "input observed before AND after the migration: {ops:?}"
        );
        assert!(
            ops.iter().all(|(p, _)| *p == pid),
            "every action carries the SAME player id across migration: {ops:?}"
        );
    }

    /// Required test 4 — the leave/migration race: a `Migrate` whose join
    /// is already dead (the leave was processed first) is rejected by the
    /// epoch gate; a fresh join of the same connection (newer epoch) is
    /// still accepted.
    #[tokio::test]
    async fn ghost_migrate_after_leave_is_rejected() {
        let mut h = Harness::new();
        let conn = ConnectionId(3);
        let (wire, _actions, _out) = h.join(0, conn, 1).await; // x = -7, shard 0, epoch 1
        let _ = h.tick().await; // steady
        // The leave (the registry's broadcast; epoch 1 = this join).
        h.leave(conn, wire, 1).await;
        let c = h.tick().await;
        // The entity is gone from both shards.
        assert!(
            c.iter().all(|v| v.iter().all(|(w, _, _, _)| *w != wire)),
            "the leave did not despawn the entity: {c:?}"
        );
        // The GHOST: a Migrate of the dead join (epoch 1) reaches shard 1
        // (simulating an in-flight migration that lost the race).
        let (ghost_out, _ghost_out_rx) = mpsc::channel::<FrameBatch>(8);
        let (_ghost_act_tx, ghost_act_rx) = mpsc::channel::<Action>(8);
        h.shard_txs[1]
            .send(ShardMsg::Migrate {
                from: 0,
                at_tick: h.t,
                wire,
                state: TState {
                    x: -7.0,
                    y: 0.0,
                    mode: 0,
                },
                player: Some(PlayerMigration {
                    player: PlayerId(conn.0),
                    conn,
                    epoch: 1,
                    entity: wire,
                    out: ghost_out,
                    actions: ghost_act_rx,
                    detached: false,
                    detach_deadline: None,
                    expire_to: crate::room::ExpireTo::Despawn,
                    bot_fed: false,
                    session_epoch: 0,
                }),
            })
            .await
            .expect("shard channel open");
        let c = h.tick().await;
        assert!(
            c.iter().all(|v| v.iter().all(|(w, _, _, _)| *w != wire)),
            "the ghost migrate resurrected the entity (the epoch gate \
             failed): {c:?}"
        );
        // A FRESH join of the same connection (epoch 2) must still be
        // accepted (the gate rejects only the dead join's epoch).
        let (wire2, _a2, _o2) = h.join(1, conn, 2).await;
        let c = h.content;
        assert!(
            c[1].iter().any(|(w, _, _, _)| *w == wire2),
            "the fresh join (epoch 2) was not accepted: {c:?}"
        );
        assert_ne!(wire, wire2, "fresh joins mint fresh identities");
    }

    /// Required test 5 — boundary visibility: each shard's snapshot
    /// includes the neighbor's boundary entities (the borrowed set), so
    /// a player at the boundary sees across the line.
    #[tokio::test]
    async fn boundary_entities_are_visible_to_both_sides() {
        let mut h = Harness::new();
        // conn 1 at x = -9 (shard 0), conn 11 at x = +1 (shard 1 — already
        // on the border: |x| <= 1).
        let (w0, actions0, mut out0) = h.join(0, ConnectionId(1), 1).await;
        let (w1, _a1, mut out1) = h.join(1, ConnectionId(11), 1).await;
        // Walk conn 1 toward the boundary (it reaches x = -1, exported by
        // shard 0, in a few ticks).
        h.act(&actions0, ConnectionId(1), 1000).await;

        /// Parse one snapshot frame into (wire, x, y) records.
        fn parse_records(payload: &bytes::Bytes) -> Vec<(u64, i32, i32)> {
            assert_eq!(payload.len() % 16, 0, "record-aligned payload");
            (0..payload.len() / 16)
                .map(|i| {
                    let b = &payload[i * 16..i * 16 + 16];
                    let wire = u64::from_le_bytes(b[0..8].try_into().unwrap());
                    let x = i32::from_le_bytes(b[8..12].try_into().unwrap());
                    let y = i32::from_le_bytes(b[12..16].try_into().unwrap());
                    (wire, x, y)
                })
                .collect()
        }
        async fn read_snapshot(
            rx: &mut mpsc::Receiver<FrameBatch>,
        ) -> Option<Vec<(u64, i32, i32)>> {
            let batch = rx.recv().await.expect("snapshot stream alive");
            batch
                .into_iter()
                .find(|f| f.op == 0x7100)
                .map(|f| parse_records(&f.payload))
        }
        // Until each shard's snapshot shows BOTH w0 (shard 0's entity) and
        // w1 (shard 1's entity) — the own record under its own id and the
        // borrowed record under the neighbor's id (disjoint ranges: no
        // collision in the union view).
        let mut seen0: Option<Vec<(u64, i32, i32)>> = None;
        let mut seen1: Option<Vec<(u64, i32, i32)>> = None;
        for _ in 0..60 {
            if seen0.is_none()
                && let Some(r) = read_snapshot(&mut out0).await
                && r.iter().any(|(w, _, _)| *w == w0)
                && r.iter().any(|(w, _, _)| *w == w1)
            {
                seen0 = Some(r);
            }
            if seen1.is_none()
                && let Some(r) = read_snapshot(&mut out1).await
                && r.iter().any(|(w, _, _)| *w == w1)
                && r.iter().any(|(w, _, _)| *w == w0)
            {
                seen1 = Some(r);
            }
            if seen0.is_some() && seen1.is_some() {
                break;
            }
            let _ = h.tick().await;
        }
        let Some(r0) = seen0 else {
            panic!("shard 0's snapshot never included the borrowed entity w1");
        };
        let Some(r1) = seen1 else {
            panic!("shard 1's snapshot never included the borrowed entity w0");
        };
        // Each view has exactly the two records, under distinct wires.
        assert_eq!(r0.len(), 2, "shard 0 view: {r0:?}");
        assert_eq!(r1.len(), 2, "shard 1 view: {r1:?}");
        let wires0: Vec<u64> = r0.iter().map(|r| r.0).collect();
        let wires1: Vec<u64> = r1.iter().map(|r| r.0).collect();
        assert!(wires0.contains(&w0) && wires0.contains(&w1));
        assert!(wires1.contains(&w0) && wires1.contains(&w1));
    }

    // -----------------------------------------------------------------
    // Faz 1 keep-alive promotion (behavior lock): a SILENT group on a
    // SHARDED room receives its cached full snapshot on the keep-alive
    // cadence — the shard-side mirror of the room actor's
    // `unchanged_group_is_silent_until_keepalive`. Drives a bare
    // (unspawned) [`ShardActor`] synchronously, like the table-prune
    // locks above: the assertions read only the connection's wire bytes
    // and the actor's own counters.
    // -----------------------------------------------------------------

    /// A single-group logic that goes silent once its content is out:
    /// `on_join` dirties the group, an emission cleans it — so after the
    /// join tick EVERY step is unchanged, which is exactly the state the
    /// keep-alive cadence exists to interrupt.
    struct KaLogic {
        dirty: bool,
        next_wire: u64,
    }

    impl GameLogic<TWorld> for KaLogic {
        type GroupKey = ();
        type Strip = TStrip;

        fn snapshot_op(&self) -> u16 {
            0x7180
        }
        fn private_op(&self) -> u16 {
            0x7181
        }
        fn group_of(&self, _w: &TWorld, _p: PlayerId) -> Self::GroupKey {}

        fn snapshot(
            &mut self,
            w: &mut TWorld,
            _ctx: &TickCtx,
            _g: &Self::GroupKey,
            _borrowed: &[BorderRecord<TStrip>],
            out: &mut bytes::BytesMut,
        ) -> bool {
            if !self.dirty {
                return false; // unchanged since the last emission
            }
            self.dirty = false;
            let mut recs: Vec<(u64, i32, i32)> = w
                .ents
                .iter()
                .map(|(wire, (x, y, _))| (*wire, *x as i32, *y as i32))
                .collect();
            recs.sort_unstable_by_key(|r| r.0);
            for (wire, x, y) in &recs {
                out.extend_from_slice(&wire.to_le_bytes());
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
            }
            true
        }

        fn on_join(&mut self, w: &mut TWorld, conn: ConnectionId) -> Admission {
            self.next_wire += 1;
            let x = (conn.0 % 20) as f32 - 10.0;
            w.ents.insert(self.next_wire, (x, 0.0, 0));
            self.dirty = true; // membership changed ⇒ must emit
            Admission {
                player: PlayerId(conn.0),
                entity: self.next_wire,
            }
        }

        fn on_leave(&mut self, _w: &mut TWorld, _player: PlayerId) {}
        fn ingest(&mut self, _w: &mut TWorld, _c: &TickCtx, actions: &mut Vec<Action>) {
            actions.clear();
        }
        fn update(&mut self, _w: &mut TWorld, _c: &TickCtx) {}
    }

    impl ShardLogic<TWorld> for KaLogic {
        type State = TState;

        fn index(&self) -> usize {
            0
        }
        fn shard_count(&self) -> usize {
            1
        }
        fn serial_base(&self) -> u64 {
            0
        }
        fn serial_range(&self) -> u64 {
            SHARD_SERIAL_RANGE
        }
        fn serial_used(&self) -> u64 {
            self.next_wire
        }
        fn neighbors(&self) -> &[usize] {
            &[]
        }
        fn collect_migrations(
            &mut self,
            _w: &mut TWorld,
            _nb: usize,
        ) -> Vec<Migrating<TState>> {
            Vec::new()
        }
        fn on_migrate_in(
            &mut self,
            _w: &mut TWorld,
            _wire: u64,
            _state: TState,
            _player: Option<PlayerId>,
        ) {
        }
        fn on_migrate_out(&mut self, _w: &mut TWorld, _wire: u64) {}
        fn collect_border(&self, _w: &TWorld) -> Vec<BorderRecord<TStrip>> {
            Vec::new()
        }
        fn own_wires(&self, w: &TWorld) -> Vec<u64> {
            w.ents.keys().copied().collect()
        }
    }

    /// The lock: `keepalive_hz = 10` under a 30 Hz shard → a re-send
    /// every 3rd step. The group emits on the join step; steps 2..=8 are
    /// silent EXCEPT steps 3 and 6, which ship the CACHED full snapshot
    /// (byte-identical to the join emission) — and nothing else ever
    /// reaches the wire. Mirrors the room-side semantics: same cadence
    /// derivation, same cache, same resend counter.
    #[tokio::test]
    async fn sharded_keepalive_resends_cached_snapshot_to_silent_group() {
        let (_tick_tx, tick_rx) = broadcast::channel(64);
        let (_self_tx, rx) = channel::<ShardMsg<TState, TStrip>>(16);
        let (n0, _n0_rx) = channel::<ShardMsg<TState, TStrip>>(8);
        let (n1, _n1_rx) = channel::<ShardMsg<TState, TStrip>>(8);
        let mut a = ShardActor::new(
            RoomConfig {
                id: RoomId(11),
                tick_hz: 30.0,
                keepalive_hz: 10.0,
                metrics_cadence_hz: 0.0,
                ..Default::default()
            },
            0,
            TWorld::default(),
            Box::new(KaLogic {
                dirty: false,
                next_wire: 0,
            }),
            tick_rx,
            rx,
            vec![n0, n1],
            1,
            metrics_null(),
            None, // no result sink
        );

        // The join (its own CONTROL message; the group is dirty).
        let (out_tx, mut out_rx) = mpsc::channel::<FrameBatch>(16);
        let (reply_tx, reply_rx) = oneshot::channel();
        assert!(a.handle_msg(
            ShardMsg::Join {
                conn: ConnectionId(1),
                epoch: 1,
                out: out_tx,
                reply: reply_tx,
            },
            &tctx(1),
        ));
        let _wire = reply_rx.await.expect("join reply").expect("join ok");

        // Steps 2..=8 change nothing. Only the cadence steps (3 and 6)
        // may ship anything besides the join emission of step 1. Driven
        // through `step` (not `step_phases`) so the actor's own step
        // counter advances — the cadence is measured in ACTOR steps.
        assert!(a.step(&tinfo(1)), "step 1 runs");
        for t in 2..=8u64 {
            assert!(a.step(&tinfo(t)), "step {t} runs");
        }

        // Exactly three batches reached the member: the join emission plus
        // two keep-alive re-sends, all byte-identical (the CACHE, not a
        // fresh encode — the default hook re-sends `last`).
        let mut got = Vec::new();
        while let Ok(batch) = out_rx.try_recv() {
            got.push(batch);
        }
        assert_eq!(
            got.len(),
            3,
            "one emission (step 1) + two keep-alive re-sends (steps 3 and \
             6), nothing else: {got:?}"
        );
        let payloads: Vec<Vec<u8>> = got
            .iter()
            .map(|b| {
                assert_eq!(b.len(), 1, "one frame per batch");
                b[0].payload.to_vec()
            })
            .collect();
        for (i, p) in payloads.iter().enumerate() {
            assert_eq!(
                p, &payloads[0],
                "keep-alive re-send {i} must be the cached snapshot bytes"
            );
        }
        assert_eq!(
            payloads[0].len(),
            16,
            "the payload is one entity record (u64 wire LE + i32 x LE + \
             i32 y LE)"
        );
        // The mechanism counter agrees with the wire.
        assert_eq!(
            a.m.keepalive_resends, 2,
            "two unchanged-group re-sends (steps 3 and 6)"
        );
    }

    // -----------------------------------------------------------------
    // Delta border exchange (CROSS-SHARD §6.4, the four-pin contract).
    // The rig below wires two BARE (unspawned) actors with real bounded
    // channels whose RECEIVING ends the test holds: every inter-shard
    // message crosses the test's hands, so a loss can be simulated
    // exactly (drop one message) and the resync traffic observed without
    // any network or timing dependence.
    // -----------------------------------------------------------------

    // -----------------------------------------------------------------
    // ShardLink seam structural locks (`docs/DISTRIBUTED.md` §3): the
    // in-process link must be a TRANSPARENT wrapper — FIFO order
    // preserved on drain, and a refused send hands the EXACT message back
    // with no partial state (the migration rollback reads its payload out
    // of the error alone).
    // -----------------------------------------------------------------

    /// Drive an [`InProcLink`] directly: FIFO order on drain; a send onto
    /// a full link returns `LinkFull::Full` carrying the rejected message
    /// without disturbing what is already queued; a drained link accepts
    /// again; a link whose receive end is gone reports `Closed`.
    #[test]
    fn inproc_link_preserves_fifo_and_drop_semantics() {
        let (tx, rx) = channel::<ShardMsg<TState, TStrip>>(2);
        let mut link = InProcLink {
            tx: Some(tx),
            rx: Some(rx),
        };
        // Fill to capacity, then one more: the third send must be refused
        // whole — nothing of it queued.
        for from in 0..2usize {
            assert!(
                link.send(ShardMsg::ResyncRequest { from }).is_ok(),
                "send {from} onto an empty/capacity-2 link"
            );
        }
        match link.send(ShardMsg::ResyncRequest { from: 2 }).unwrap_err() {
            LinkFull::Full {
                msg: ShardMsg::ResyncRequest { from },
            } => assert_eq!(from, 2, "the EXACT refused message comes back"),
            other => panic!("expected Full carrying the message, got {other:?}"),
        }
        // FIFO preserved and no partial state: exactly the two accepted
        // sends, in order; the rejected third did not squeeze in.
        let drained = link.drain();
        assert_eq!(drained.len(), 2, "only the accepted sends deliver");
        for (i, m) in drained.iter().enumerate() {
            match m {
                ShardMsg::ResyncRequest { from } => assert_eq!(*from, i),
                other => panic!("unexpected message in drain: {other:?}"),
            }
        }
        // An emptied link accepts again (the channel semantics, not a
        // poisoned wrapper).
        assert!(link.send(ShardMsg::ResyncRequest { from: 7 }).is_ok());
        assert!(link.send(ShardMsg::ResyncRequest { from: 8 }).is_ok());
        assert!(link.send(ShardMsg::ResyncRequest { from: 9 }).is_err());
        let drained = link.drain();
        assert_eq!(drained.len(), 2);
        assert!(matches!(drained[0], ShardMsg::ResyncRequest { from: 7 }));
        assert!(matches!(drained[1], ShardMsg::ResyncRequest { from: 8 }));
        assert!(link.drain().is_empty(), "drain empties fully");

        // Closed: a link whose receive end is gone refuses with `Closed`
        // (not Full), still handing the message back; a send-only link's
        // drain is simply empty.
        let (tx, rx) = channel::<ShardMsg<TState, TStrip>>(1);
        drop(rx);
        let mut dead = InProcLink {
            tx: Some(tx),
            rx: None,
        };
        match dead.send(ShardMsg::ResyncRequest { from: 5 }).unwrap_err() {
            LinkFull::Closed {
                msg: ShardMsg::ResyncRequest { from },
            } => assert_eq!(from, 5),
            other => panic!("expected Closed carrying the message, got {other:?}"),
        }
        assert!(dead.drain().is_empty());
    }

    /// A TLogic for the border rig; its observation channels are dead
    /// (every send is `let _ =` ignored) — the tests read protocol state,
    /// not world observations.
    fn rig_logic(index: usize) -> TLogic {
        let (obs, _obs_rx) = mpsc::channel(16);
        let (ops, _ops_rx) = mpsc::channel(16);
        TLogic {
            index,
            next_serial: 0,
            player_ent: HashMap::new(),
            ent_player: HashMap::new(),
            last_tick: 0,
            obs,
            ops,
        }
    }

    fn rig_actor(index: usize, neighbors: Vec<Mailbox<ShardMsg<TState, TStrip>>>) -> ShardActor<TWorld, (), TState, TStrip> {
        let (_tick_tx, tick_rx) = broadcast::channel(64);
        let (_self_tx, rx) = channel::<ShardMsg<TState, TStrip>>(16);
        ShardActor::new(
            RoomConfig {
                id: RoomId(13),
                keepalive_hz: 0.0,
                metrics_cadence_hz: 0.0,
                ..Default::default()
            },
            index,
            TWorld::default(),
            Box::new(rig_logic(index)),
            tick_rx,
            rx,
            neighbors,
            1,
            metrics_null(),
            None, // no result sink
        )
    }

    /// Two bare shard actors (0 and 1, mutual neighbors) wired through
    /// channels the TEST controls. `put` seeds strip entities directly —
    /// the strip is `|x| <= 1`, and x stays strictly inside the owner's
    /// region so no migration path ever fires under these tests.
    struct BorderRig {
        s0: ShardActor<TWorld, (), TState, TStrip>,
        s1: ShardActor<TWorld, (), TState, TStrip>,
        /// What s0 exports to s1 lands here (test-held receiving end).
        tx01: Mailbox<ShardMsg<TState, TStrip>>,
        rx01: Inbox<ShardMsg<TState, TStrip>>,
        /// What s1 sends back (resync requests) lands here.
        _tx10: Mailbox<ShardMsg<TState, TStrip>>,
        rx10: Inbox<ShardMsg<TState, TStrip>>,
    }

    impl BorderRig {
        fn new() -> Self {
            let (tx01, rx01) = mpsc::channel(16);
            let (tx10, rx10) = mpsc::channel(16);
            // Slot fillers for the unused self-slots (never targeted:
            // TLogic::neighbors is [1] for index 0 and [0] for index 1).
            let (d0, _d0rx) = channel::<ShardMsg<TState, TStrip>>(1);
            let (d1, _d1rx) = channel::<ShardMsg<TState, TStrip>>(1);
            let s0 = rig_actor(0, vec![d0, tx01.clone()]);
            let s1 = rig_actor(1, vec![tx10.clone(), d1]);
            BorderRig {
                s0,
                s1,
                tx01,
                rx01,
                _tx10: tx10,
                rx10,
            }
        }

        /// Run shard 0's phases at this tick index (its exports land in
        /// `rx01`).
        fn step0(&mut self, tick: u64) {
            assert!(self.s0.step_phases(&tinfo(tick)), "shard 0 keeps running");
        }

        /// Take everything shard 0 exported (WITHOUT delivering): the
        /// test inspects each message and decides deliver vs drop — the
        /// exact seam a lost exchange needs.
        fn drain01(&mut self) -> Vec<ShardMsg<TState, TStrip>> {
            let mut out = Vec::new();
            while let Ok(m) = self.rx01.try_recv() {
                out.push(m);
            }
            out
        }

        /// Feed messages into shard 1's CONTROL handler.
        fn deliver_to_s1(&mut self, msgs: Vec<ShardMsg<TState, TStrip>>) {
            for m in msgs {
                assert!(self.s1.handle_msg(m, &tctx(999)), "s1 keeps running");
            }
        }

        /// Feed messages into shard 0's CONTROL handler.
        fn deliver_to_s0(&mut self, msgs: Vec<ShardMsg<TState, TStrip>>) {
            for m in msgs {
                assert!(self.s0.handle_msg(m, &tctx(999)), "s0 keeps running");
            }
        }
    }

    /// Seed a boundary entity straight into a shard's world.
    fn put(a: &mut ShardActor<TWorld, (), TState, TStrip>, wire: u64, x: f32, y: f32) {
        a.world.ents.insert(wire, (x, y, 0));
    }

    /// Assert the batch is exactly one Border carrying a Full; return its
    /// (seq, entities). Generic over the strip payload so every rig
    /// (positional and rich) reuses one helper.
    fn expect_full<S: Debug>(msgs: &[ShardMsg<TState, S>]) -> (u64, &[BorderRecord<S>]) {
        assert_eq!(msgs.len(), 1, "exactly one export message: {msgs:?}");
        match &msgs[0] {
            ShardMsg::Border {
                exchange: BorderExchange::Full { seq, entities, .. },
                ..
            } => (*seq, entities.as_slice()),
            other => panic!("expected a Full exchange, got {other:?}"),
        }
    }

    /// Assert the batch is exactly one Border carrying a Delta; return
    /// its (seq, upserts, exits). Generic over the strip payload.
    fn expect_delta<S: Debug>(
        msgs: &[ShardMsg<TState, S>],
    ) -> (u64, &[BorderRecord<S>], &[u64]) {
        assert_eq!(msgs.len(), 1, "exactly one export message: {msgs:?}");
        match &msgs[0] {
            ShardMsg::Border {
                exchange:
                    BorderExchange::Delta {
                        seq,
                        upserts,
                        exits,
                        ..
                    },
                ..
            } => (*seq, upserts.as_slice(), exits.as_slice()),
            other => panic!("expected a Delta exchange, got {other:?}"),
        }
    }

    /// Delta lock 1 — an entity entering the strip appears in the
    /// neighbor's view; moving updates it in place; leaving removes it
    /// (no ghost). The bootstrap is an explicit Full; every later step is
    /// a minimal delta.
    #[tokio::test]
    async fn delta_exchange_applies_upserts_and_exits_correctly() {
        let mut r = BorderRig::new();

        // Enter: first contact ships the whole strip as a Full...
        put(&mut r.s0, 100, -1.0, 0.0);
        r.step0(1);
        let msgs = r.drain01();
        let (seq, entities) = expect_full(&msgs);
        assert_eq!(
            entities,
            [rec(100, -1, 0)],
            "bootstrap Full carries the strip"
        );
        r.deliver_to_s1(msgs);
        assert_eq!(r.s1.border[&0].recs.len(), 1, "view established");
        assert_eq!(
            r.s1.border[&0].expected_seq,
            seq + 1,
            "the receiver expects the next sequence"
        );

        // Move: only the changed record ships, as an upsert delta.
        put(&mut r.s0, 100, -1.0, 1.0);
        r.step0(2);
        let msgs = r.drain01();
        let (_seq2, upserts, exits) = expect_delta(&msgs);
        assert_eq!(upserts, [rec(100, -1, 1)]);
        assert!(exits.is_empty(), "a move is not an exit");
        r.deliver_to_s1(msgs);
        assert_eq!(r.s1.border[&0].recs[&100].state.y, 1, "position updated");

        // A second entity enters: only IT is new.
        put(&mut r.s0, 101, -1.0, 5.0);
        r.step0(3);
        let msgs = r.drain01();
        let (_seq3, upserts, _exits3) = expect_delta(&msgs);
        assert_eq!(upserts, [rec(101, -1, 5)]);
        r.deliver_to_s1(msgs);
        assert_eq!(r.s1.border[&0].recs.len(), 2);

        // Leave: an explicit exit record — the borrowed view must drop
        // the entity (a full-era wholesale replace never had this failure
        // mode; a delta without exits would ghost forever).
        let _ = r.s0.world.ents.remove(&101);
        r.step0(4);
        let msgs = r.drain01();
        let (_seq4, upserts, exits) = expect_delta(&msgs);
        assert!(upserts.is_empty(), "a leave is not an upsert");
        assert_eq!(exits, [101]);
        r.deliver_to_s1(msgs);
        assert_eq!(
            r.s1.border[&0].recs.len(),
            1,
            "no ghost after the exit"
        );
        assert!(!r.s1.border[&0].recs.contains_key(&101));
    }

    /// Delta lock 2 (§6.4 pin 3a) — a lost delta is DETECTED, not silently
    /// diverged: the receiver rejects the next delta on its sequence
    /// mismatch, quarantines the view, sends a ResyncRequest upstream, and
    /// the serving Full restores a correct complete view.
    #[tokio::test]
    async fn seq_gap_triggers_resync_full() {
        let mut r = BorderRig::new();

        // Bootstrap: Full(seq=1) delivered → expected becomes 2.
        put(&mut r.s0, 100, -1.0, 0.0);
        r.step0(1);
        let msgs = r.drain01();
        r.deliver_to_s1(msgs);

        // THE LOSS: the next delta (seq=2, y→1) never arrives.
        put(&mut r.s0, 100, -1.0, 1.0);
        r.step0(2);
        let lost = r.drain01();
        assert_eq!(lost.len(), 1, "the delta was sent — then dropped by us");
        // ...and discarded. Nothing delivered.

        // The NEXT delta (seq=3) carries the wrong sequence number.
        put(&mut r.s0, 100, -1.0, 2.0);
        r.step0(3);
        let msgs = r.drain01();
        let (seq, _, _) = expect_delta(&msgs);
        assert_eq!(seq, 3, "the sender stamped consecutively");
        r.deliver_to_s1(msgs); // rejected INSIDE handle_msg

        assert!(
            r.s1.border[&0].stale_until_full,
            "the mismatch quarantines the view"
        );
        assert_eq!(
            r.s1.border[&0].recs[&100].state.y, 0,
            "nothing after the last GOOD exchange was applied (no \
             half-applied state)"
        );
        assert_eq!(
            r.s1.bstats.resync_requests_sent, 1,
            "exactly one resync request went upstream"
        );
        // The request crossed back over the controlled channel:
        let requests: Vec<_> = {
            let mut out = Vec::new();
            while let Ok(m) = r.rx10.try_recv() {
                out.push(m);
            }
            out
        };
        assert!(
            requests
                .iter()
                .any(|m| matches!(m, ShardMsg::ResyncRequest { from: 1 })),
            "ResyncRequest flowed to the neighbor: {requests:?}"
        );
        r.deliver_to_s0(requests);

        // The healing Full: even with NO further changes the flagged
        // neighbor gets a Full next tick, and it restores the COMPLETE
        // current truth (including what the lost delta carried).
        r.step0(4);
        let msgs = r.drain01();
        let (_, entities) = expect_full(&msgs);
        assert_eq!(
            entities,
            [rec(100, -1, 2)],
            "the healing Full re-baselines everything"
        );
        r.deliver_to_s1(msgs);
        assert!(!r.s1.border[&0].stale_until_full, "quarantine lifted");
        assert_eq!(r.s1.border[&0].recs[&100].state.y, 2, "view correct again");
        assert!(
            !r.s1.border[&0].stale_until_full
                && r.s1.border[&0].expected_seq == 5,
            "sequence re-baselined past the healing Full"
        );
    }

    /// Delta lock 3 (pin 3b) — a rebuilt shard's fresh incarnation leads
    /// with a FULL (its sender state starts empty), and the receiver
    /// resets cleanly: the dead incarnation's records cannot ghost.
    #[tokio::test]
    async fn rebuilt_shard_first_exchange_is_full_and_resets_receiver() {
        let mut r = BorderRig::new();

        // Incarnation A establishes a populated view on shard 1.
        put(&mut r.s0, 100, -1.0, 0.0);
        r.step0(1);
        let msgs = r.drain01();
        r.deliver_to_s1(msgs);
        assert_eq!(r.s1.border[&0].recs.len(), 1);

        // REBUILD: a brand-new actor for shard 0 — fresh world (the new
        // incarnation respawned different entities), fresh export state,
        // its own channel to the SAME receiver.
        let (tx01p, mut rx01p) = mpsc::channel(16);
        let (d, _drx) = channel::<ShardMsg<TState, TStrip>>(1);
        std::mem::forget(_drx);
        let mut s0p = rig_actor(0, vec![d, tx01p]);
        put(&mut s0p, 200, -1.0, 7.0);
        assert!(s0p.step_phases(&tinfo(50)), "rebuilt shard runs");

        let mut first = Vec::new();
        while let Ok(m) = rx01p.try_recv() {
            first.push(m);
        }
        let (seq, entities) = expect_full(&first);
        assert_eq!(
            entities,
            [rec(200, -1, 7)],
            "the FRESH incarnation's first exchange is a Full of ITS strip"
        );
        r.deliver_to_s1(first);

        // The receiver reset cleanly: exactly the new incarnation's
        // records, old-incarnation ghost gone, sequence re-baselined.
        let view = &r.s1.border[&0];
        assert_eq!(view.recs.len(), 1, "whole-view replacement: {view:?}");
        assert!(view.recs.contains_key(&200), "new entity present");
        assert!(
            !view.recs.contains_key(&100),
            "the dead incarnation's record must not survive as a ghost"
        );
        assert_eq!(
            view.expected_seq,
            seq + 1,
            "expected sequence re-baselined from the new stream"
        );
        assert!(!view.stale_until_full);
    }

    /// Delta lock 4 (pin 3c) — the periodic sigorta: a quiet neighbor is
    /// shipped NOTHING on ordinary ticks (the byte win), but the 256-tick
    /// cadence forces a Full even with zero changes.
    #[tokio::test]
    async fn periodic_full_fires_on_cadence() {
        let mut r = BorderRig::new();

        // Bootstrap + one change establish a non-empty ledger.
        put(&mut r.s0, 100, -1.0, 0.0);
        r.step0(1);
        {
            let msgs = r.drain01();
            r.deliver_to_s1(msgs);
        }
        put(&mut r.s0, 100, -1.0, 1.0);
        r.step0(2);
        {
            let msgs = r.drain01();
            r.deliver_to_s1(msgs);
        }

        // Quiet tick: no changes ⇒ NOTHING ships (this skip is the point
        // of the whole exercise).
        r.step0(3);
        assert!(r.drain01().is_empty(), "an unchanged strip ships nothing");

        // ...but the cadence tick forces a Full regardless of quietness.
        assert!(r.s0.world.ents.len() == 1, "still just the one entity");
        r.step0(BORDER_FULL_EVERY_TICKS);
        let msgs = r.drain01();
        let (_, entities) = expect_full(&msgs);
        assert_eq!(entities.len(), 1, "the Full carries the whole strip");

        // And quietness resumes right after.
        r.step0(BORDER_FULL_EVERY_TICKS + 1);
        assert!(r.drain01().is_empty(), "no change after the cadence ⇒ silent");

        // Counter cross-check within this run: two Fulls (bootstrap +
        // periodic), one delta, zero drops.
        assert_eq!(r.s0.bstats.full_exchanges, 2);
        assert_eq!(r.s0.bstats.delta_exchanges, 1);
        assert_eq!(r.s0.bstats.export_drops, 0);
    }

    /// Delta lock 5 (backpressure correctness) — a try_send failure on a
    /// DELTA marks that neighbor for a Full, which arrives on the very
    /// next tick carrying the data the dropped delta would have brought:
    /// divergence heals within ONE tick instead of the 256-tick cadence.
    #[tokio::test]
    async fn send_failure_marks_neighbor_for_full_resync() {
        let mut r = BorderRig::new();

        // Bootstrap normally.
        put(&mut r.s0, 100, -1.0, 0.0);
        r.step0(1);
        {
            let msgs = r.drain01();
            r.deliver_to_s1(msgs);
        }

        // Saturate the neighbor mailbox: nothing else fits.
        while r
            .tx01
            .try_send(ShardMsg::ResyncRequest { from: 999 })
            .is_ok()
        {}

        // A strip change now ships a delta — which MUST fail.
        put(&mut r.s0, 100, -1.0, 1.0);
        r.step0(2);
        assert_eq!(
            r.s0.bstats.delta_drops, 1,
            "the failed delta is counted"
        );
        assert!(
            r.s0.export[&1].needs_full,
            "the failure flags the neighbor for a Full"
        );

        // Unblock the channel (drain the dummies AND anything else).
        while r.rx01.try_recv().is_ok() {}

        // Next tick, NO further changes: the flag alone forces a Full —
        // and it carries the position update the dropped delta had.
        r.step0(3);
        let msgs = r.drain01();
        let (_, entities) = expect_full(&msgs);
        assert_eq!(
            entities,
            [rec(100, -1, 1)],
            "the healing Full contains what the dropped delta carried"
        );
        r.deliver_to_s1(msgs);
        assert_eq!(
            r.s1.border[&0].recs[&100].state.y, 1,
            "the receiver converged despite the loss"
        );
        assert!(!r.s0.export[&1].needs_full, "flag consumed");
    }

    /// Delta lock 6 (pin 4) — the own-wins filter applies IDENTICALLY to
    /// records that entered the view via a delta: an entity that just
    /// migrated INTO this shard wins over the neighbor's stale borrowed
    /// copy, so the snapshot lists it once, at the OWN position.
    #[tokio::test]
    async fn own_wins_filter_applies_to_delta_applied_records() {
        let mut r = BorderRig::new();

        // Shard 1 gains its own member at x = 0 (conn 10 ⇒ x = 0 per the
        // test logic's spawn rule; region 1, so nothing migrates).
        let (out_tx, mut out_rx) = mpsc::channel::<FrameBatch>(16);
        let (reply_tx, reply_rx) = oneshot::channel();
        assert!(r.s1.handle_msg(
            ShardMsg::Join {
                conn: ConnectionId(10),
                epoch: 1,
                out: out_tx,
                reply: reply_tx,
            },
            &tctx(1),
        ));
        let w_own = reply_rx
            .await
            .expect("join reply")
            .expect("join ok")
            .0;

        // Bootstrap an EMPTY strip from shard 0 (Full, first contact),
        // then apply a DELTA that inserts the stale borrowed copy of the
        // just-crossed own entity — the exact crossing-tick shape of pin
        // 4. The copy enters the view THROUGH the delta path.
        r.step0(1); // empty strip, first contact ⇒ Full{entities: []}
        {
            let msgs = r.drain01();
            r.deliver_to_s1(msgs);
        }
        r.s1.handle_msg(
            ShardMsg::Border {
                from: 0,
                exchange: BorderExchange::Delta {
                    seq: 2, // matches the expected sequence after the Full
                    tick: 2,
                    upserts: vec![rec(w_own, -9, 0)],
                    exits: vec![],
                },
            },
            &tctx(2),
        );
        assert_eq!(
            r.s1.border[&0].recs.get(&w_own).map(|b| b.state.x),
            Some(-9),
            "the stale copy IS in the borrowed view (delta applied)"
        );

        // Broadcast: the snapshot must contain the entity EXACTLY ONCE,
        // at the OWN (fresh) position — the borrowed copy filtered.
        assert!(r.s1.step_phases(&tinfo(3)));
        let mut seen = Vec::new();
        while let Ok(batch) = out_rx.try_recv() {
            for f in batch {
                if f.op == 0x7100 {
                    seen.extend_from_slice(&f.payload);
                }
            }
        }
        assert_eq!(
            seen.len(),
            16,
            "one 16-byte record total (own + filtered borrowed)"
        );
        let wire = u64::from_le_bytes(seen[0..8].try_into().unwrap());
        let x = i32::from_le_bytes(seen[8..12].try_into().unwrap());
        let y = i32::from_le_bytes(seen[12..16].try_into().unwrap());
        assert_eq!(
            (wire, x, y),
            (w_own, 0, 0),
            "the OWN record won over the delta-applied stale copy"
        );
    }
    // -----------------------------------------------------------------
    // Rich-strip locks: the visibility-strip payload is the GAME's type
    // ([`GameLogic::Strip`]). These locks prove the generalization does
    // what the core-fixed record could not: a payload field beyond
    // position must survive BOTH exchange paths (Full bootstrap and
    // Delta upsert), and a change in ANY payload field — not just the
    // coordinates — must fire the delta diff.
    // -----------------------------------------------------------------

    /// A strip payload with one field BEYOND position (a facing-like
    /// quantity; the combat/prediction shape this generalization exists
    /// for).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct TRich {
        x: i32,
        y: i32,
        facing: i16,
    }

    /// A minimal shard logic whose strip carries [`TRich`]. The payload is
    /// assembled from game state (`TWorld`'s third slot read as facing) —
    /// exactly the ownership split under test: the core could never have
    /// derived this field. `update` is a no-op, so entities stay put and
    /// only an explicit mutation changes anything.
    struct RichLogic {
        index: usize,
    }

    impl GameLogic<TWorld> for RichLogic {
        type GroupKey = ();
        type Strip = TRich;

        fn snapshot_op(&self) -> u16 {
            0x7300
        }
        fn private_op(&self) -> u16 {
            0x7301
        }
        fn group_of(&self, _w: &TWorld, _p: PlayerId) -> Self::GroupKey {}
        fn snapshot(
            &mut self,
            _w: &mut TWorld,
            _c: &TickCtx,
            _g: &Self::GroupKey,
            _borrowed: &[BorderRecord<TRich>],
            _out: &mut bytes::BytesMut,
        ) -> bool {
            false // no members join in these tests; nothing ever emits
        }
        fn on_join(&mut self, w: &mut TWorld, conn: ConnectionId) -> Admission {
            let wire = self.index as u64 * SHARD_SERIAL_RANGE + conn.0;
            w.ents.insert(wire, ((conn.0 % 20) as f32 - 10.0, 0.0, 0));
            Admission {
                player: PlayerId(conn.0),
                entity: wire,
            }
        }
        fn on_leave(&mut self, _w: &mut TWorld, _player: PlayerId) {}
        fn ingest(&mut self, _w: &mut TWorld, _c: &TickCtx, actions: &mut Vec<Action>) {
            actions.clear();
        }
        fn update(&mut self, _w: &mut TWorld, _c: &TickCtx) {}
    }

    impl ShardLogic<TWorld> for RichLogic {
        type State = TState;

        fn index(&self) -> usize {
            self.index
        }
        fn shard_count(&self) -> usize {
            2
        }
        fn serial_base(&self) -> u64 {
            self.index as u64 * SHARD_SERIAL_RANGE
        }
        fn serial_range(&self) -> u64 {
            SHARD_SERIAL_RANGE
        }
        fn serial_used(&self) -> u64 {
            0
        }
        fn neighbors(&self) -> &[usize] {
            if self.index == 0 {
                &[1]
            } else {
                &[0]
            }
        }
        fn collect_migrations(
            &mut self,
            _w: &mut TWorld,
            _nb: usize,
        ) -> Vec<Migrating<TState>> {
            Vec::new()
        }
        fn on_migrate_in(
            &mut self,
            _w: &mut TWorld,
            _wire: u64,
            _state: TState,
            _player: Option<PlayerId>,
        ) {
        }
        fn on_migrate_out(&mut self, _w: &mut TWorld, _wire: u64) {}
        fn collect_border(&self, w: &TWorld) -> Vec<BorderRecord<TRich>> {
            // The strip: same |x| <= 1 frame as TLogic, plus the CUSTOM
            // field from game state.
            w.ents
                .iter()
                .filter(|(_, (x, _, _))| x.abs() <= 1.0)
                .map(|(wire, (x, y, facing))| BorderRecord {
                    wire: *wire,
                    state: TRich {
                        x: *x as i32,
                        y: *y as i32,
                        facing: *facing as i16,
                    },
                })
                .collect()
        }
        fn own_wires(&self, w: &TWorld) -> Vec<u64> {
            w.ents.keys().copied().collect()
        }
    }

    /// Two bare [`RichLogic`] actors wired through channels the TEST
    /// controls (the [`BorderRig`] pattern, typed over [`TRich`]).
    struct RichRig {
        s0: ShardActor<TWorld, (), TState, TRich>,
        s1: ShardActor<TWorld, (), TState, TRich>,
        /// What s0 exports to s1 lands here (test-held receiving end).
        rx01: Inbox<ShardMsg<TState, TRich>>,
        /// What s1 sends back lands here (never drained yet: no rich
        /// lock exercises the resync round trip; held so the channel
        /// stays open).
        #[allow(dead_code)]
        _rx10: Inbox<ShardMsg<TState, TRich>>,
    }

    impl RichRig {
        fn new() -> Self {
            let (tx01, rx01) = mpsc::channel(16);
            let (tx10, _rx10) = mpsc::channel(16);
            let build = |index: usize, tx: Mailbox<ShardMsg<TState, TRich>>, other: Mailbox<ShardMsg<TState, TRich>>| {
                let (_tick_tx, tick_rx) = broadcast::channel(64);
                let (_self_tx, rx) = channel::<ShardMsg<TState, TRich>>(16);
                ShardActor::new(
                    RoomConfig {
                        id: RoomId(15),
                        keepalive_hz: 0.0,
                        metrics_cadence_hz: 0.0,
                        ..Default::default()
                    },
                    index,
                    TWorld::default(),
                    Box::new(RichLogic { index }),
                    tick_rx,
                    rx,
                    vec![tx, other],
                    1,
                    metrics_null(),
                    None, // no result sink
                )
            };
            let (d0, _d0rx) = mpsc::channel(1);
            let (d1, _d1rx) = mpsc::channel(1);
            RichRig {
                s0: build(0, d0, tx01),
                s1: build(1, tx10, d1),
                rx01,
                _rx10,
            }
        }

        /// Run shard 0's phases at this tick index (its exports land in
        /// `rx01`).
        fn step0(&mut self, tick: u64) {
            assert!(self.s0.step_phases(&tinfo(tick)), "shard 0 keeps running");
        }

        /// Take everything shard 0 exported (WITHOUT delivering).
        fn drain01(&mut self) -> Vec<ShardMsg<TState, TRich>> {
            let mut out = Vec::new();
            while let Ok(m) = self.rx01.try_recv() {
                out.push(m);
            }
            out
        }

        /// Feed messages into shard 1's CONTROL handler.
        fn deliver_to_s1(&mut self, msgs: Vec<ShardMsg<TState, TRich>>) {
            for m in msgs {
                assert!(self.s1.handle_msg(m, &tctx(999)), "s1 keeps running");
            }
        }
    }

    /// Seed a boundary entity straight into a shard's world (the third
    /// tuple slot is the FACING source for [`RichLogic`]'s strip).
    fn put_rich(a: &mut ShardActor<TWorld, (), TState, TRich>, wire: u64, x: f32, facing: i8) {
        a.world.ents.insert(wire, (x, 0.0, facing));
    }

    /// Rich lock 1 — a strip record whose payload has a field beyond
    /// position arrives INTACT through both paths: the Full bootstrap on
    /// first contact, and the Delta upsert after ONLY the custom field
    /// changed. The receiving view (typed over the SAME logic-defined
    /// payload) holds the exact values the sender's logic assembled.
    #[tokio::test]
    async fn rich_strip_record_survives_full_and_delta_paths() {
        let mut r = RichRig::new();

        // Bootstrap: first contact ships the whole strip as a Full, with
        // the custom field intact.
        put_rich(&mut r.s0, 100, -1.0, 7);
        r.step0(1);
        let msgs = r.drain01();
        let (seq, entities) = expect_full(&msgs);
        assert_eq!(
            entities,
            [BorderRecord {
                wire: 100,
                state: TRich {
                    x: -1,
                    y: 0,
                    facing: 7
                }
            }],
            "the Full bootstrap carries the RICH record whole"
        );
        r.deliver_to_s1(msgs);
        assert_eq!(
            r.s1.border[&0].recs[&100].state,
            TRich {
                x: -1,
                y: 0,
                facing: 7
            },
            "the FULL path preserved every payload field"
        );
        assert_eq!(
            r.s1.border[&0].expected_seq,
            seq + 1,
            "receiver sequence re-baselined by the Full"
        );

        // Change ONLY the custom field (position untouched): the next
        // exchange is a delta whose upsert carries the new value intact.
        r.s0.world.ents.get_mut(&100).unwrap().2 = 9;
        r.step0(2);
        let msgs = r.drain01();
        let (_seq2, upserts, exits) = expect_delta(&msgs);
        assert_eq!(
            upserts,
            [BorderRecord {
                wire: 100,
                state: TRich {
                    x: -1,
                    y: 0,
                    facing: 9
                }
            }],
            "the DELTA upsert carries the custom field"
        );
        assert!(exits.is_empty());
        r.deliver_to_s1(msgs);
        assert_eq!(
            r.s1.border[&0].recs[&100].state.facing, 9,
            "the DELTA path preserved the custom field end to end"
        );
        assert_eq!(
            (r.s1.border[&0].recs[&100].state.x, r.s1.border[&0].recs[&100].state.y),
            (-1, 0),
            "position unchanged alongside it"
        );
    }

    /// Rich lock 2 — the delta diff keys off WHOLE-payload equality: a
    /// change confined to the custom field fires an upsert, a tick with
    /// no change of any field ships NOTHING (the silent-tick skip that
    /// is the delta's entire point). A position-only diff would miss the
    /// first half; an always-ship design would waste the second.
    #[tokio::test]
    async fn delta_diff_fires_on_custom_field_change() {
        let mut r = RichRig::new();

        // Bootstrap (Full) and settle the ledger.
        put_rich(&mut r.s0, 100, -1.0, 3);
        r.step0(1);
        {
            let msgs = r.drain01();
            r.deliver_to_s1(msgs);
        }

        // Quiet tick: no field changed ⇒ NOTHING ships.
        r.step0(2);
        assert!(
            r.drain01().is_empty(),
            "an unchanged strip ships nothing"
        );

        // Change ONLY the custom field: the next tick ships exactly one
        // upsert, carrying the new facing at the unchanged position.
        r.s0.world.ents.get_mut(&100).unwrap().2 = 4;
        r.step0(3);
        let msgs = r.drain01();
        let (_seq, upserts, exits) = expect_delta(&msgs);
        assert_eq!(
            upserts,
            [BorderRecord {
                wire: 100,
                state: TRich {
                    x: -1,
                    y: 0,
                    facing: 4
                }
            }],
            "a custom-field-only change fires the delta"
        );
        assert!(exits.is_empty(), "no exit: the entity never left");
        r.deliver_to_s1(msgs);
        assert_eq!(
            r.s1.border[&0].recs[&100].state.facing, 4,
            "the receiving view took the custom-field update"
        );

        // And quietness resumes once the change was accepted.
        r.step0(4);
        assert!(r.drain01().is_empty(), "no further change ⇒ silent again");
    }

}
