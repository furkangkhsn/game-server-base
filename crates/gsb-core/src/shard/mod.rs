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

mod actor;
mod border;
mod effect;
mod link;
mod logic;
mod msg;
mod seam;

#[cfg(test)]
mod tests;

pub use actor::ShardActor;
pub(crate) use border::*;
pub use border::{BorderExchange, BorderRecord};
pub(crate) use effect::*;
pub use effect::{
    EFFECT_BUDGET_PER_TICK, EFFECT_FORWARD_TTL_TICKS, EFFECT_MAX_AGE_TICKS, EFFECT_MAX_HOPS,
    EFFECT_RETRY_CAP, EFFECT_WINDOW, EffectId, EffectOutcome, EmitRefused, RemoteEffect,
};
pub(crate) use link::*;
pub use logic::ShardLogic;
pub use msg::{Migrating, PlayerMigration, ResumeReply, ShardMsg};
pub use seam::{CrossSeam, Lent, SeamStage};

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
