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
//! ## The tick body (six phases, synchronous)
//!
//! ```text
//! Phase 0  │  CONTROL:  drain the shard channel: Join / Leave / Migrate /
//!           │           Border / Shutdown (try_recv — non-blocking)
//! Phase 1  │  READ:     pull actions from each connection's channel
//!           │           (same bounded pull as the room)
//! Phase 2  │  CONVERT:  actions → component writes (ShardLogic)
//! Phase 3  │  SYSTEMS:  run the ordered game systems (ShardLogic)
//! Phase 4  │  MIGRATE:  despawn the entities marked out last tick; then,
//!           │           for each neighbor, collect the entities that moved
//!           │           into the neighbor's region and send them over
//!           │           (full state + the player's moved channels)
//! Phase 5  │  BORDER:   export this shard's boundary entities (full
//!           │           state, idempotent) to every neighbor
//! Phase 6  │  BROADCAST: one snapshot per group, including the borrowed
//!           │           boundary entities (see below)
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
//! ## Boundary visibility (borrowed entities)
//!
//! A player on a shard boundary must see entities in the neighboring
//! shard — otherwise enemies vanish at the line. Every tick (phase 5) each
//! shard exports its **boundary entities** (game-defined: the demo exports
//! the entities within one border width of its region edges) to all
//! neighbors as a `Border` exchange; each shard keeps the latest exchange
//! per neighbor and includes the borrowed records in **every** group's
//! snapshot (phase 6 passes them to `ShardLogic::snapshot`). The exchange
//! is *full state, not a delta*: a dropped or delayed exchange self-heals
//! on the next tick (the borrowed set is re-sent whole), so a lagging
//! shard costs at most one tick of stale boundary data — never a gap.
//!
//! **Wire-identity interaction:** borrowed records carry the *neighbor's*
//! wire ids. The ranges are disjoint (below), so a client's view — own
//! shard's entities plus the borrowed boundary set — can never contain
//! the same id for two different entities, and the snapshot's "no
//! change" ledger is a plain union map over wire ids. One subtlety the
//! core enforces: an entity that just crossed INTO this shard appears as
//! its own record AND (for one tick) as the neighbor's stale borrowed
//! copy of itself — the actor filters the borrowed set against
//! [`ShardLogic::own_wires`] so the own (fresh) record wins and the
//! snapshot never lists one entity twice.
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

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::fmt::Debug;
use std::hash::Hash;
use std::time::Instant;

use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, warn};

use crate::channel::{FrameBatch, Inbox, Mailbox};
use crate::error::CoreError;
use crate::id::{ConnectionId, EntityId, RoomId};
use crate::metrics::{MetricsEvent, RoomSample, hist_index};
use crate::room::{Action, GroupState, RoomConn, RoomConfig, RoomCounters, TickCtx};
use crate::ticker::TickInfo;

/// Identities per shard in the wire-id range partitioning (see module
/// docs, "Wire identity"): 2^20 ≈ 100× the measured 10k single-room wall
/// in per-shard lifetime spawn churn.
pub const SHARD_SERIAL_RANGE: u64 = 1 << 20;

/// A neighbor shard's boundary entity, as included in this shard's
/// snapshots (mirrors the wire `EntityRecord`: identity + truncated
/// position). `Copy` on purpose: the export is cloned per neighbor and the
/// borrowed set is re-sent whole every tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorrowedRecord {
    pub wire: u64,
    pub x: i32,
    pub y: i32,
}

/// One neighbor's boundary view (full state, idempotent — see module
/// docs, "Boundary visibility").
#[derive(Debug)]
pub struct BorderExchange {
    /// The tick index the neighbor sampled the set at (diagnostics; the
    /// consumer uses the latest exchange it has, whatever its age).
    pub tick: u64,
    pub entities: Vec<BorrowedRecord>,
}

/// A player's channel halves, moved with a migrating player entity
/// (ownership transfer — see module docs, "Migration protocol").
#[derive(Debug)]
pub struct PlayerMigration {
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
}

/// A migrating entity: the full game state plus the owning connection,
/// when the entity is a player (NPCs have no connection).
pub struct Migrating<S> {
    /// The entity's wire identity (kept across the migration).
    pub wire: u64,
    /// The full component state (game-shaped).
    pub state: S,
    pub conn: Option<ConnectionId>,
}

/// Messages between shards and from the registry to a shard. One bounded
/// channel per shard: control (join/leave/shutdown) and the shard
/// protocol (migrate/border) share it — all are drained with `try_recv`
/// at the tick boundary, and the exchange traffic is small (a few
/// messages per neighbor per tick).
#[derive(Debug)]
pub enum ShardMsg<S> {
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
    /// The neighbor's boundary entities (full state; replaces whatever the
    /// shard has for that neighbor).
    Border { from: usize, exchange: BorderExchange },
    /// Stop the shard (drops the world).
    Shutdown,
}

/// Game-side behaviour of a shard. Implemented by the game crate; the core
/// never inspects the world `W` or the state `S::State`.
///
/// Everything a [`crate::room::RoomLogic`] has (the tick seam), plus the
/// sharding seam: the region topology (`neighbors`), the migration
/// protocol (`collect_migrations` / `on_migrate_in` / `on_migrate_out`),
/// the boundary export (`collect_border`), and the wire-id range
/// (`serial_base` / `serial_range` / `serial_used`).
pub trait ShardLogic<W>: Send {
    /// Opaque key partitioning the shard's connections into snapshot
    /// groups (same contract as `RoomLogic::GroupKey`).
    type GroupKey: Eq + Hash + Clone + Debug;
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

    /// Opcode under which the shard ships group snapshots.
    fn snapshot_op(&self) -> u16;
    /// Opcode under which the shard ships the per-connection private frame.
    fn private_op(&self) -> u16;

    /// Which snapshot group `conn` belongs to (re-evaluated every tick).
    fn group_of(&self, world: &W, conn: ConnectionId) -> Self::GroupKey;

    /// Encode the complete, self-contained snapshot of one group: the
    /// shard's own world **plus** the `borrowed` boundary records of the
    /// neighboring shards (a player at the boundary must see across the
    /// line — see module docs, "Boundary visibility"). Same "no change"
    /// contract as `RoomLogic::snapshot` (and "no change" *includes* the
    /// borrowed content: a neighbor's boundary entity moving is a content
    /// change for every group here).
    fn snapshot(
        &mut self,
        world: &mut W,
        ctx: &TickCtx,
        group: &Self::GroupKey,
        borrowed: &[BorrowedRecord],
        out: &mut bytes::BytesMut,
    ) -> bool;

    /// Encode a per-connection private frame. The connection's group is
    /// passed in (see [`RoomLogic::private`] for the rationale).
    /// Default: none.
    fn private(
        &mut self,
        _world: &mut W,
        _conn: ConnectionId,
        _group: &Self::GroupKey,
        _out: &mut bytes::BytesMut,
    ) -> bool {
        false
    }

    /// A player entered the shard: create its entity and return its wire
    /// id (minted from this shard's range — see module docs).
    fn on_join(&mut self, world: &mut W, conn: ConnectionId) -> EntityId;

    /// A player left the shard: remove its entity.
    fn on_leave(&mut self, world: &mut W, conn: ConnectionId);

    /// Phase 2 — convert buffered actions into component writes.
    fn ingest(&mut self, world: &mut W, ctx: &TickCtx, actions: &mut Vec<Action>);

    /// Phase 3 — run the game systems for this tick.
    fn update(&mut self, world: &mut W, ctx: &TickCtx);

    /// Called when the shard shuts down (world is dropped right after).
    fn on_shutdown(&mut self) {}

    /// Entity records encoded during the most recent broadcast phase
    /// (same contract as `RoomLogic::encoded_records`).
    fn encoded_records(&mut self) -> u64 {
        0
    }

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
    /// its wire id. `conn` is present for player entities (the shard
    /// handles the connection table itself); the logic must record the
    /// conn→entity / entity→conn bookkeeping so that `on_leave` and the
    /// next `collect_migrations` see it.
    fn on_migrate_in(
        &mut self,
        world: &mut W,
        wire: u64,
        state: Self::State,
        conn: Option<ConnectionId>,
    );

    /// Remove an entity that migrated out (phase 4, the mark fired):
    /// despawn it and clean its bookkeeping.
    fn on_migrate_out(&mut self, world: &mut W, wire: u64);

    /// This shard's boundary entities (full wire records) for the phase-5
    /// export to the neighbors. The set is re-sent WHOLE every tick
    /// (idempotent): a dropped exchange self-heals on the next tick.
    fn collect_border(&self, world: &W) -> Vec<BorrowedRecord>;

    /// The wire ids of this shard's OWN entities (the snapshot's own
    /// records). Used to keep a migrating entity from appearing twice in
    /// one snapshot — as its own record AND as the neighbor's one-tick-
    /// stale borrowed copy of itself (module docs, "Boundary
    /// visibility"): when both are present, the own record wins.
    fn own_wires(&self, world: &W) -> Vec<u64>;
}

/// The shard actor. Owns one shard's world, its connection table (the
/// shard's share of the room's connections), and its group table — the
/// same ownership discipline as the room actor (`RoomActor<W, G>`), plus
/// the shard protocol state (neighbor mailboxes, the latest border
/// exchange per neighbor, the pending migrate-out marks, the deferred
/// migrations, the conn-epoch table). `G` is the snapshot group key, `St`
/// the migration state (see [`ShardLogic`]).
pub struct ShardActor<W, G, St> {
    config: RoomConfig,
    index: usize,
    world: W,
    logic: Box<dyn ShardLogic<W, GroupKey = G, State = St>>,
    tick_rx: broadcast::Receiver<TickInfo>,
    shard_rx: Inbox<ShardMsg<St>>,
    conns: HashMap<ConnectionId, RoomConn<G>>,
    /// The epoch of the join this shard currently holds per connection
    /// (set on Join and on Migrate-in; carried in outgoing Migrate
    /// messages — see module docs, "Migration protocol").
    conn_epoch: HashMap<ConnectionId, u64>,
    /// The highest LEAVE epoch this shard has processed per connection —
    /// the leave/migration race gate: a Migrate of a join whose leave
    /// was already processed is dead and must be rejected. Kept SEPARATE
    /// from `conn_epoch` on purpose: an entry there can come from the
    /// join itself (or a migration install) — the join is alive, not
    /// dead — and gating on it would reject legitimate re-migrations of
    /// the same join (see module docs, "Migration protocol").
    conn_tombstone: HashMap<ConnectionId, u64>,
    groups: HashMap<G, GroupState>,
    /// One mailbox per shard index (used for the neighbors' indices).
    neighbors: Vec<Mailbox<ShardMsg<St>>>,
    /// The latest border exchange per neighbor (replaced wholesale — the
    /// exchange is full state).
    border: HashMap<usize, Vec<BorrowedRecord>>,
    /// Entities marked out by a successful `Migrate` send: (wire, the tick
    /// index at which the shard despawns them).
    pending_out: Vec<(u64, u64)>,
    /// Migrations that arrived early (install gate not open — see
    /// `handle_msg`): re-offered at the next tick's CONTROL, in send
    /// order.
    deferred: VecDeque<ShardMsg<St>>,
    run_every: u64,
    last_at: Option<Instant>,
    steps: u64,
    keepalive_every: Option<u64>,
    metrics_every: u64,
    budget_us: u64,
    m: RoomCounters,
    metrics: mpsc::Sender<MetricsEvent>,
}

impl<W, G, St> ShardActor<W, G, St>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
    St: Debug + Send + 'static,
{
    /// Build a shard actor. `neighbors` is indexed by shard index (the
    /// unused slots may be any closed/unused mailbox — only the
    /// `ShardLogic::neighbors()` slots are sent to).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: RoomConfig,
        index: usize,
        world: W,
        logic: Box<dyn ShardLogic<W, GroupKey = G, State = St>>,
        tick_rx: broadcast::Receiver<TickInfo>,
        shard_rx: Inbox<ShardMsg<St>>,
        neighbors: Vec<Mailbox<ShardMsg<St>>>,
        run_every: u64,
        metrics: mpsc::Sender<MetricsEvent>,
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
        Self {
            config,
            index,
            world,
            logic,
            tick_rx,
            shard_rx,
            conns: HashMap::new(),
            conn_epoch: HashMap::new(),
            conn_tombstone: HashMap::new(),
            groups: HashMap::new(),
            neighbors,
            border: HashMap::new(),
            pending_out: Vec::new(),
            deferred: VecDeque::new(),
            run_every: run_every.max(1),
            last_at: None,
            steps: 0,
            keepalive_every,
            metrics_every,
            budget_us,
            m: RoomCounters::default(),
            metrics,
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
        while let Ok(m) = self.shard_rx.try_recv() {
            if !self.handle_msg(m, &ctx) {
                return false;
            }
        }

        // -- Phase 1 — READ (the room's bounded pull: per-connection
        //    fairness budget + shard-level pull budget).
        let per_conn = self.config.max_actions_per_conn_per_tick;
        let mut budget = self.config.max_pending_actions;
        let mut actions: Vec<Action> = Vec::new();
        for r in self.conns.values_mut() {
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

        // -- Phase 2 — CONVERT.
        self.logic.ingest(&mut self.world, &ctx, &mut actions);

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
                let player = mig.conn.and_then(|c| {
                    let entry = self.conns.remove(&c)?;
                    Some(PlayerMigration {
                        conn: c,
                        // The epoch of the join this entity belongs to: the
                        // shard that last installed it recorded it.
                        epoch: self.conn_epoch.get(&c).copied().unwrap_or(0),
                        out: entry.out,
                        actions: entry.actions,
                        entity: entry.entity,
                    })
                });
                match self.neighbors[b].try_send(ShardMsg::Migrate {
                    from: self.index,
                    at_tick: t.tick,
                    wire: mig.wire,
                    state: mig.state,
                    player,
                }) {
                    Ok(()) => {
                        self.pending_out.push((mig.wire, t.tick + 1));
                    }
                    Err(mpsc::error::TrySendError::Full(msg)
                    | mpsc::error::TrySendError::Closed(msg)) => {
                        // Roll back the connection's move (the entity stays;
                        // the connection must remain registered and pull
                        // its input here until the retry lands).
                        if let ShardMsg::Migrate {
                            player,
                            wire,
                            ..
                        } = msg
                        {
                            if let Some(p) = player {
                                self.conns.insert(
                                    p.conn,
                                    RoomConn {
                                        out: p.out,
                                        actions: p.actions,
                                        entity: p.entity,
                                        group: self.logic.group_of(&self.world, p.conn),
                                        batch: Vec::new(),
                                    },
                                );
                            }
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

        // -- Phase 5 — BORDER (export this shard's boundary entities,
        //    WHOLE, to every neighbor: the exchange is idempotent, so a
        //    dropped one self-heals next tick).
        let records = self.logic.collect_border(&self.world);
        for b in self.logic.neighbors() {
            if self.neighbors[*b]
                .try_send(ShardMsg::Border {
                    from: self.index,
                    exchange: BorderExchange {
                        tick: t.tick,
                        entities: records.clone(),
                    },
                })
                .is_err()
            {
                warn!(
                    room = %self.config.id,
                    shard = self.index,
                    neighbor = b,
                    "border send failed (neighbor channel full); the \
                     exchange re-sends in full next tick"
                );
            }
        }

        // -- Phase 6 — BROADCAST (the room's broadcast phase with the
        //    borrowed boundary set folded into every group's snapshot).
        self.broadcast_phase(&ctx);
        true
    }

    /// Handle one shard-channel message (phase 0). Returns `false` when
    /// the actor should stop.
    fn handle_msg(&mut self, m: ShardMsg<St>, ctx: &TickCtx) -> bool {
        match m {
            ShardMsg::Join {
                conn,
                epoch,
                out,
                reply,
            } => {
                // A join supersedes any stale state this connection had
                // (same as the room actor).
                if let Some(old) = self.conns.remove(&conn) {
                    self.logic.on_leave(&mut self.world, conn);
                    let _ = old; // the old channel halves drop with the entry
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
                let entity = self.logic.on_join(&mut self.world, conn);
                self.m.joins += 1;
                let (act_tx, act_rx) = mpsc::channel(self.config.action_capacity);
                self.conn_epoch.insert(conn, epoch);
                self.conns.insert(
                    conn,
                    RoomConn {
                        out,
                        actions: act_rx,
                        entity,
                        group: self.logic.group_of(&self.world, conn),
                        batch: Vec::new(),
                    },
                );
                let _ = reply.send(Ok((entity, act_tx)));
                debug!(
                    room = %self.config.id,
                    shard = self.index,
                    %conn,
                    entity,
                    epoch,
                    "player joined shard"
                );
                true
            }
            ShardMsg::Leave { conn, entity, epoch } => {
                // Stale-leave guard (the entity id): only the entity this
                // connection currently owns.
                if self.conns.get(&conn).map(|c| c.entity) == Some(entity) {
                    self.conns.remove(&conn);
                    self.logic.on_leave(&mut self.world, conn);
                    self.m.leaves += 1;
                    debug!(
                        room = %self.config.id,
                        shard = self.index,
                        %conn,
                        entity,
                        "player left shard"
                    );
                }
                // Leave tombstone (see module docs): a late `Migrate` of
                // the join this leave ends must be rejected here — and in
                // every other shard that also saw the leave (it is
                // broadcast to all of them). The tombstone table is kept
                // SEPARATE from `conn_epoch` (the installed join's
                // epoch): a migration of an *alive* join carries that
                // epoch legitimately and must not be mistaken for dead.
                match self.conn_tombstone.entry(conn) {
                    Entry::Occupied(mut e) => {
                        if *e.get() < epoch {
                            *e.get_mut() = epoch;
                        }
                    }
                    Entry::Vacant(e) => {
                        e.insert(epoch);
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
                // carries the installed epoch legitimately.
                if let Some(p) = &player
                    && let Some(tomb) = self.conn_tombstone.get(&p.conn)
                    && *tomb >= p.epoch
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
                    player.as_ref().map(|p| p.conn),
                );
                if let Some(p) = player {
                    // The connection moves here: the out channel and the
                    // action inbox were MOVED with the message (ownership
                    // transfer — the connection actor never notices).
                    // Invariant (see module docs): after passing the epoch
                    // gate this shard cannot already hold the connection.
                    debug_assert!(!self.conns.contains_key(&p.conn));
                    self.conn_epoch.insert(p.conn, p.epoch);
                    self.conns.insert(
                        p.conn,
                        RoomConn {
                            out: p.out,
                            actions: p.actions,
                            entity: p.entity,
                            group: self.logic.group_of(&self.world, p.conn),
                            batch: Vec::new(),
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
                // Full state, idempotent: replace whatever we have.
                self.border.insert(from, exchange.entities);
                true
            }
            ShardMsg::Shutdown => false,
        }
    }

    /// Phase 6: the room's broadcast phase, with the borrowed boundary set
    /// (the latest exchange per neighbor, flattened and sorted by wire for
    /// deterministic payload order) folded into every group's snapshot.
    fn broadcast_phase(&mut self, ctx: &TickCtx) {
        let snap_op = self.logic.snapshot_op();
        let priv_op = self.logic.private_op();

        // 6a. Recompute each connection's group.
        for (conn, rc) in self.conns.iter_mut() {
            rc.group = self.logic.group_of(&self.world, *conn);
        }

        // 6b. Rebuild the group table (same as the room's 4b).
        let mut members: HashMap<G, Vec<ConnectionId>> = HashMap::new();
        for (conn, rc) in &self.conns {
            members.entry(rc.group.clone()).or_default().push(*conn);
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
        // the latest exchange per neighbor, flattened. Sorted by wire so
        // the payload order is deterministic (the ledger comparison is
        // map-based and order-independent anyway).
        let mut borrowed: Vec<BorrowedRecord> = Vec::new();
        for recs in self.border.values() {
            borrowed.extend_from_slice(recs);
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

        // 6c. One snapshot per group (encode once, freeze once, share).
        let keep_due = self
            .keepalive_every
            .map(|every| self.steps.is_multiple_of(every))
            .unwrap_or(false);
        for (group, st) in self.groups.iter_mut() {
            let mut buf = bytes::BytesMut::new();
            if self.logic.snapshot(&mut self.world, ctx, group, &borrowed, &mut buf) {
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
                let payload = buf.freeze();
                st.sent = Some(payload.clone());
                st.last = Some(payload);
            } else if keep_due {
                if st.last.is_some() {
                    self.m.keepalive_resends += 1;
                }
                st.sent = st.last.clone();
            }
        }

        self.m.snap_records =
            self.m.snap_records.saturating_add(self.logic.encoded_records());

        // 6d. Per-connection fan-out (same as the room's 4d).
        let mut dropped: u64 = 0;
        // Same batch-buffer reuse as the room's 4d (one floor slice was the
        // per-connection per-tick `Vec::with_capacity(2)`).
        let mut pbuf = bytes::BytesMut::new();
        for (conn, rc) in self.conns.iter_mut() {
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
            pbuf.clear();
            if self.logic.private(&mut self.world, *conn, &rc.group, &mut pbuf) {
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
            // Shards do not run the RPC machinery yet (the pending set
            // lives in the single-room actor; see `crate::rpc`): the
            // request counters stay zero by construction.
            requests_local: 0,
            requests_external: 0,
            requests_rejected_malformed: 0,
            requests_rejected_dup: 0,
            requests_rejected_no_handler: 0,
            requests_rejected_logic: 0,
            requests_rejected_conn_cap: 0,
            requests_rejected_room_cap: 0,
            requests_timed_out: 0,
            requests_late: 0,
            pending_requests: 0,
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
    use std::time::Duration;

    use crate::channel::channel;

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
        conn_ent: HashMap<ConnectionId, u64>,
        ent_conn: HashMap<u64, ConnectionId>,
        last_tick: u64,
        obs: mpsc::Sender<Obs>,
        ops: mpsc::Sender<(ConnectionId, u16)>,
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

    impl ShardLogic<TWorld> for TLogic {
        type GroupKey = ();
        type State = TState;

        fn index(&self) -> usize {
            self.index
        }
        fn shard_count(&self) -> usize {
            2
        }
        fn snapshot_op(&self) -> u16 {
            0x7100
        }
        fn private_op(&self) -> u16 {
            0x7101
        }
        fn group_of(&self, _w: &TWorld, _c: ConnectionId) -> Self::GroupKey {}
        fn snapshot(
            &mut self,
            w: &mut TWorld,
            _ctx: &TickCtx,
            _g: &Self::GroupKey,
            borrowed: &[BorrowedRecord],
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
                recs.push((b.wire, b.x, b.y));
            }
            recs.sort_unstable_by_key(|r| r.0);
            for (wire, x, y) in &recs {
                out.extend_from_slice(&wire.to_le_bytes());
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
            }
            true
        }
        fn on_join(&mut self, w: &mut TWorld, conn: ConnectionId) -> EntityId {
            // Deterministic spawn: x = (conn.0 % 20) - 10 (conn 1 → -9 in
            // shard 0; conn 10 → 0 in shard 1; conn 11 → +1 in shard 1),
            // y = 0, no motion.
            let x = (conn.0 % 20) as f32 - 10.0;
            self.next_serial += 1;
            let wire = self.serial_base() + self.next_serial;
            w.ents.insert(wire, (x, 0.0, 0));
            self.conn_ent.insert(conn, wire);
            self.ent_conn.insert(wire, conn);
            wire
        }
        fn on_leave(&mut self, w: &mut TWorld, conn: ConnectionId) {
            if let Some(wire) = self.conn_ent.remove(&conn) {
                self.ent_conn.remove(&wire);
                w.ents.remove(&wire);
            }
        }
        fn ingest(&mut self, w: &mut TWorld, _ctx: &TickCtx, actions: &mut Vec<Action>) {
            for a in actions.drain(..) {
                let _ = self.ops.try_send((a.conn, a.op));
                // The test's ops: 1000 = step +1/tick, 1001 = step -1/tick,
                // 1002 = stop.
                let mode = match a.op {
                    1000 => 1,
                    1001 => -1,
                    _ => 0,
                };
                if let Some(wire) = self.conn_ent.get(&a.conn).copied()
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
                        conn: self.ent_conn.get(&wire).copied(),
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
            conn: Option<ConnectionId>,
        ) {
            w.ents.insert(wire, (state.x, state.y, state.mode));
            if let Some(c) = conn {
                self.conn_ent.insert(c, wire);
                self.ent_conn.insert(wire, c);
            }
        }
        fn on_migrate_out(&mut self, w: &mut TWorld, wire: u64) {
            if let Some(c) = self.ent_conn.remove(&wire) {
                self.conn_ent.remove(&c);
            }
            w.ents.remove(&wire);
        }
        fn collect_border(&self, w: &TWorld) -> Vec<BorrowedRecord> {
            // Border = entities within 1 unit of the region edge (x = 0).
            w.ents
                .iter()
                .filter(|(_, (x, _, _))| x.abs() <= 1.0)
                .map(|(wire, (x, y, _))| BorrowedRecord {
                    wire: *wire,
                    x: *x as i32,
                    y: *y as i32,
                })
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
        shard_txs: [Mailbox<ShardMsg<TState>>; 2],
        obs: mpsc::Receiver<Obs>,
        ops: mpsc::Receiver<(ConnectionId, u16)>,
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
            let (tx0, rx0) = channel::<ShardMsg<TState>>(128);
            let (tx1, rx1) = channel::<ShardMsg<TState>>(128);
            let (obs_tx, obs_rx) = mpsc::channel(4096);
            let (ops_tx, ops_rx) = mpsc::channel(4096);
            let (dummy_tx, _dummy_rx) = channel::<ShardMsg<TState>>(1);
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
                        conn_ent: HashMap::new(),
                        ent_conn: HashMap::new(),
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
                        conn_ent: HashMap::new(),
                        ent_conn: HashMap::new(),
                        last_tick: 0,
                        obs: obs_tx.clone(),
                        ops: ops_tx,
                    }),
                    tick_tx.subscribe(),
                    rx1,
                    vec![tx0.clone(), dummy_tx],
                    1,
                    metrics_null(),
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
        async fn ops_drained(&mut self) -> Vec<(ConnectionId, u16)> {
            let mut out = Vec::new();
            while let Ok(op) = self.ops.try_recv() {
                out.push(op);
            }
            out
        }
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
        let n = ops.iter().filter(|(c, op)| *c == conn && *op == 1001).count();
        assert_eq!(n, 1, "ops: {ops:?}");
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
                    conn,
                    epoch: 1,
                    entity: wire,
                    out: ghost_out,
                    actions: ghost_act_rx,
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
}
