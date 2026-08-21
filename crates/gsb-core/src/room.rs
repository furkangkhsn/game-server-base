//! The room actor: one per room/map, owning its world and its tick loop.
//!
//! A room runs the five-phase tick driven by the **global ticker** (a
//! `broadcast` channel, see [`crate::ticker`]):
//!
//! ```text
//! global ticker ──TickInfo (broadcast)──▶ room actor   (the only await)
//!                                            │
//!  Phase 0  │  CONTROL:   pull join/leave/shutdown from the control channel
//!  Phase 1  │  READ:      pull actions from each connection's channel
//!  Phase 2  │  CONVERT:   actions → component writes (RoomLogic)
//!  Phase 3  │  SYSTEMS:   run the ordered game systems   (RoomLogic)
//!  Phase 4  │  BROADCAST: one snapshot per group (RoomLogic::snapshot)
//!           │             → freeze → shared Bytes fan-out + private frames
//!             └─────────────────────────────────────────────────────────┘
//! ```
//!
//! Everything is pulled with `try_recv` — the tick body is fully
//! synchronous. Per-connection action channels isolate users: one flooding
//! connection can only fill its own channel, never delay a tick or another
//! connection.
//!
//! **Broadcast model (per-group full snapshots).** Connections are
//! partitioned into snapshot groups by the game logic
//! ([`RoomLogic::group_of`]): `()` means "one group per room" (the demo),
//! `ConnectionId` means "one snapshot per connection". Each tick the room
//! encodes each group's **entire** snapshot **once**, `freeze()`s it, and
//! fans the resulting `Bytes` out to the group's members by reference
//! (Arc refcount — the payload is never copied). Membership (join/leave)
//! is expressed by presence in the snapshot: there are no spawn/remove
//! events. The game logic decides "nothing changed for this group"
//! (`RoomLogic::snapshot` returning `false`, including membership
//! changes); when nothing changed anywhere, the room ships nothing except
//! on a keep-alive tick, when each group re-sends its last cached snapshot
//! so a client that lost its last packet cannot stay stale forever
//! (`RoomConfig::keepalive_hz`). A dropped batch (slow client) costs at
//! most one snapshot of staleness: every snapshot is self-contained
//! (no delta, no history), so the next one heals the gap.
//!
//! Time handling: each room tracks the last tick it stepped at. The step
//! `dt` is the wall-clock difference, so ticks missed while busy are
//! absorbed into a single catch-up step and the simulation stays
//! frame-rate independent (the same real-time displacement at 15 Hz or
//! 100 Hz). `dt` is capped at `RoomConfig::max_catchup` periods so a
//! pathological stall produces temporary slow-motion instead of a giant
//! step. A room running slower than the global ticker simply steps on
//! every k-th global tick.
//!
//! The room actor owns **no** game types: the world is an opaque `W` and
//! the group key an opaque `G`; all game behaviour is delegated to
//! [`RoomLogic`].
//!
//! **Metrics:** the room's counters live in the room's own local state
//! ([`RoomCounters`]) and are flushed once per step over the metrics
//! channel with a *synchronous* unbounded send — the room's only `await`
//! stays `tick_rx.recv()` and the tick body stays fully synchronous
//! (see [`crate::metrics`] for the design and the constraint rationale).

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, warn};

use crate::channel::{FrameBatch, Inbox, Mailbox};
use crate::error::CoreError;
use crate::id::{ConnectionId, EntityId, RoomId};
use crate::metrics::{MetricsEvent, RoomSample, FINE_HIST_BINS, HIST_BINS};
use crate::ticker::TickInfo;

/// A client action forwarded by the connection actor. The payload is still
/// encoded; the game crate decodes it against its own message types.
#[derive(Debug)]
pub struct Action {
    pub conn: ConnectionId,
    pub op: u16,
    pub payload: bytes::Bytes,
}

/// Static configuration for a room.
#[derive(Debug, Clone)]
pub struct RoomConfig {
    pub id: RoomId,
    /// Simulation rate in ticks per second. Must divide the global ticker
    /// rate: the room steps on every k-th global tick.
    pub tick_hz: f64,
    /// Capacity of the control channel (join/leave/shutdown).
    pub control_capacity: usize,
    /// Capacity of each connection's action channel.
    pub action_capacity: usize,
    /// Per-connection per-tick pull budget (fairness cut, READ phase): a
    /// single connection's input can contribute at most this many actions
    /// to one tick, no matter how much its bounded channel is holding.
    /// The excess *stays in the channel* — it is pulled on later ticks
    /// (deferred, not dropped by the room); if a connection outpaces the
    /// budget sustainedly, its own `try_send` hits the full channel and
    /// drops *its own* newest input (counted by the connection actor,
    /// attributed to it). This is what stops one flooding connection from
    /// pushing *other* connections' earlier input out of a tick.
    ///
    /// Default 16: 480 actions/s at the default 30 Hz — ~70× the measured
    /// demo input rate (one MOVE_TO per 150 ms ≈ 6.7/s, the load test's
    /// steady state at the 10k-connection wall) — while cutting a
    /// flooder's per-tick contribution from the channel cap (256) to 16.
    pub max_actions_per_conn_per_tick: usize,
    /// Room-level pull budget (READ phase): the total number of actions the
    /// room pulls in one tick. This is a *pull* bound, not a drop bound —
    /// when it is exhausted the room simply pulls no more this tick and the
    /// remainder waits in the senders' bounded channels (the room never
    /// drops an action; see [`RoomCounters::dropped_actions`]). It bounds
    /// the tick's ingest cost, which is what the 10k-connection load wall
    /// measured (the room's serial step path, not its memory).
    pub max_pending_actions: usize,
    /// Per-room membership cap (capacity, JOIN phase): when the room
    /// already holds this many members, a *new* join is rejected with
    /// [`crate::error::CoreError::RoomFull`] — the room never creates the
    /// entity, and the connection actor replies with a gentle `ERROR`
    /// frame (code 8) instead of a silent close (the connection stays
    /// alive and may join another room). A re-join of a connection that is
    /// already a member is never rejected (it supersedes its own state).
    ///
    /// Default `Some(10_000)`: the measured single-room load wall (load
    /// test C1: the 33.3 ms step budget is breached between 9k and 10k
    /// members; at 10k the room runs at 23.2 Hz with 52 771 dropped
    /// fan-out frames and 2.17 s of late ticks). The cap makes that
    /// degraded regime structurally unreachable; `None` = unlimited.
    pub max_players: Option<usize>,
    /// Cap for catch-up `dt`, in periods: after a long stall the next step
    /// simulates at most this many periods (temporary slow-motion).
    pub max_catchup: u32,
    /// Warn (log) when a group's snapshot payload exceeds this many bytes.
    /// rUDP MTU readiness: an oversized snapshot cannot ride a datagram, so
    /// a sustained warning is the signal to split the group (AOI) or lower
    /// its emission rate.
    pub max_snapshot_bytes: usize,
    /// Keep-alive rate for unchanged groups, in Hz. When a group is
    /// unchanged the room ships nothing for it — but every
    /// `tick_hz / keepalive_hz` steps each group re-sends its last cached
    /// snapshot, so a client that lost its last packet cannot stay stale
    /// forever. `<= 0` disables keep-alive.
    ///
    /// Must be `<= tick_hz`: the room cannot keep alive faster than it
    /// ticks. The registry rejects a room with `keepalive_hz > tick_hz` at
    /// creation ([`crate::error::CoreError::KeepaliveRate`]); direct
    /// construction (library use) warns once and clamps the cadence to
    /// every step, which defeats the silence gain.
    pub keepalive_hz: f64,
    /// Rate, in Hz, at which the room emits a metrics sample over the
    /// (bounded) metrics channel. The room samples at most every
    /// `tick_hz / metrics_cadence_hz` steps; the collector only ever uses
    /// the *latest* sample per report period, so sampling faster than the
    /// report cadence discards samples (A2). Tying this to the collector's
    /// report cadence (default 1 Hz) means each room sends ~1 sample per
    /// report instead of one per step — 30× less traffic on the channel, and
    /// since the counters are cumulative nothing is lost.
    ///
    /// `> tick_hz` clamps to every step (you cannot sample faster than you
    /// step); `<= 0` means "every step" as well.
    pub metrics_cadence_hz: f64,
}

impl Default for RoomConfig {
    fn default() -> Self {
        Self {
            id: RoomId(0),
            tick_hz: 30.0,
            control_capacity: 128,
            action_capacity: 256,
            max_actions_per_conn_per_tick: 16,
            max_pending_actions: 65536,
            max_players: Some(10_000),
            max_catchup: 4,
            max_snapshot_bytes: 1400,
            keepalive_hz: 1.0,
            metrics_cadence_hz: 1.0,
        }
    }
}

impl RoomConfig {
    pub fn period(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.tick_hz)
    }
}

/// Control messages to a room. Low frequency; processed at the next tick
/// boundary (deterministic: joins and leaves take effect *on* a tick, never
/// mid-simulation; join/leave latency is at most one tick).
#[derive(Debug)]
pub enum RoomControl {
    /// A player joined: register its outbound channel, create its entity,
    /// and hand the connection actor the sender of the new per-connection
    /// action channel.
    ///
    /// The reply is a `Result`: the join protocol can *structurally* fail
    /// (a full room — [`crate::error::CoreError::RoomFull`] — rejects
    /// without creating the entity). The room's member count is the
    /// authority on capacity; the registry (which counts connections, not
    /// room members) never decides it.
    Join {
        conn: ConnectionId,
        out: mpsc::Sender<FrameBatch>,
        reply: oneshot::Sender<Result<(EntityId, Mailbox<Action>), CoreError>>,
    },
    /// A player left: remove its entity and channel.
    ///
    /// Carries the entity this leave refers to: a *stale* leave (its
    /// connection re-joined in the meantime) is ignored, so it can never
    /// despawn the new entity.
    Leave {
        conn: ConnectionId,
        entity: EntityId,
    },
    /// Stop the room (drops the world).
    Shutdown,
}

/// Per-tick metadata handed to the game logic.
#[derive(Debug, Clone, Copy)]
pub struct TickCtx {
    pub room: RoomId,
    /// Global tick index (from the ticker; all rooms share one clock).
    pub tick: u64,
    /// Time since the previous step (covers any ticks missed in between).
    pub dt: Duration,
}

/// Game-side behaviour of a room. Implemented by the game crate; the core
/// never inspects the world `W` or the group key `GroupKey`.
pub trait RoomLogic<W>: Send {
    /// Opaque key partitioning the room's connections into snapshot groups.
    /// `()` = one group per room (everyone sees the whole world);
    /// `ConnectionId` = one snapshot per connection; anything else (e.g. a
    /// zone id) is a legitimate future grouping. `Debug` so the room's
    /// group diagnostics can name a misbehaving group.
    type GroupKey: Eq + Hash + Clone + Debug;

    /// Opcode under which the room ships group snapshots.
    fn snapshot_op(&self) -> u16;
    /// Opcode under which the room ships the per-connection private frame
    /// produced by [`Self::private`].
    fn private_op(&self) -> u16;

    /// Which snapshot group `conn` belongs to. Re-evaluated every tick: a
    /// group may depend on the world (e.g. the zone an entity is in).
    fn group_of(&self, world: &W, conn: ConnectionId) -> Self::GroupKey;

    /// Encode the group's snapshot payload into `out`.
    ///
    /// Return `false` when the group is unchanged since **this group's**
    /// last emitted snapshot — and "no change" **includes** membership
    /// (join/leave). The room then ships nothing to the group, except on a
    /// keep-alive tick (see [`Self::keepalive`]).
    ///
    /// The payload format is the logic's protocol decision:
    /// - a **full, self-contained** snapshot (the default, what
    ///   `all`/`team`/`pvs` and the shard rooms ship): no delta, no
    ///   history — the payload alone defines the group's entire world, and
    ///   a lost packet is healed by the next one;
    /// - a **full/delta stream** (the spatial AOI ships this): the payload
    ///   is marked full or delta on the wire, deltas apply on top of the
    ///   client's last accepted payload, and the logic must guarantee the
    ///   client can always get a full again: every fresh group member
    ///   receives a one-shot full via [`Self::private`], and every
    ///   keep-alive tick ships a fresh full via [`Self::keepalive`] (so a
    ///   client that lost one or more deltas heals within one keep-alive
    ///   period). The sequence number (global tick index) in the payload
    ///   is the client's loss detector.
    ///
    /// **Bookkeeping must be per-group.** The room calls this once per
    /// existing group, per tick, in *unspecified* order (a `HashMap`
    /// iteration, stable within a run but not to be depended on). Your
    /// "unchanged?" decision and your last-emitted bookkeeping must
    /// therefore be keyed by `group`: one call must not change another
    /// group's answer in the same tick. A single shared ledger is only
    /// correct for one-group rooms (`GroupKey = ()`) — the demo's `last`
    /// field is exactly that. With several groups, the group visited first
    /// consumes the change and rewrites the shared ledger, and every group
    /// visited afterwards sees "no change" for the rest of the run: their
    /// members starve (they receive only keep-alive re-sends of a cache
    /// that is stale from the start, or of nothing at all), and the room
    /// cannot detect it — silence is also the legitimate state of a
    /// genuinely unchanged group.
    fn snapshot(
        &mut self,
        world: &mut W,
        ctx: &TickCtx,
        group: &Self::GroupKey,
        out: &mut bytes::BytesMut,
    ) -> bool;

    /// Encode a per-connection private frame (delivered only to `conn`,
    /// alongside the group snapshot).
    ///
    /// The connection's group is passed in: the room re-evaluates it every
    /// tick (see [`Self::group_of`]) and hands the current value over, so a
    /// logic that needs "which group is this connection in" must not
    /// re-derive it — each re-derivation is extra table lookups per
    /// connection per tick (measured: part of the idle floor).
    ///
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

    /// Produce the payload to ship to a group on a **keep-alive tick**
    /// (the cadence is due). Called after `snapshot` for the same tick,
    /// whether it emitted a payload or the group was unchanged; on a
    /// keep-alive tick this method decides what actually goes out.
    ///
    /// The default re-sends the group's last cached snapshot (`last`) —
    /// correct for full, self-contained logics: for an unchanged group it
    /// is the very snapshot that healed a lost packet, and for an active
    /// group `last` *is* this tick's fresh full (the method was just
    /// called after `snapshot` set it), so re-sending is bit-identical to
    /// keeping the tick's payload. Return `false` to keep that behaviour.
    ///
    /// A delta-mode logic must return `true` with a freshly encoded
    /// **full** snapshot in `out` instead: re-sending the last *delta* is
    /// meaningless (a client that missed it has no baseline to apply it
    /// against; a current client would double-apply it), while a fresh
    /// full — shipped on the cadence tick whether the group is active or
    /// silent, replacing an active group's tick delta (a superset of it)
    /// — heals any client that lost one or more deltas, bounding the
    /// recovery time to the keep-alive period.
    fn keepalive(
        &mut self,
        _world: &mut W,
        _ctx: &TickCtx,
        _group: &Self::GroupKey,
        _last: Option<&bytes::Bytes>,
        _out: &mut bytes::BytesMut,
    ) -> bool {
        false
    }

    /// A player entered the room: create (or restore) its entity and return
    /// its id.
    fn on_join(&mut self, world: &mut W, conn: ConnectionId) -> EntityId;

    /// A player left the room: remove its entity.
    fn on_leave(&mut self, world: &mut W, conn: ConnectionId);

    /// Phase 2 — convert buffered actions into component writes.
    fn ingest(&mut self, world: &mut W, ctx: &TickCtx, actions: &mut Vec<Action>);

    /// Phase 3 — run the game systems for this tick.
    fn update(&mut self, world: &mut W, ctx: &TickCtx);

    /// Called when the room shuts down (world is dropped right after).
    fn on_shutdown(&mut self) {}

    /// The number of entity records the logic encoded during the most
    /// recent broadcast phase (summed over all groups). The room polls
    /// this exactly **once per step, immediately after the broadcast
    /// phase** (it is the broadcast phase's own metric: the payload is
    /// opaque to the core, so the record count can only come from the
    /// logic that encoded it).
    ///
    /// This is the *overlap* measurement the load test reports: divided by
    /// the broadcastable entity count it says how many times the same
    /// entity was encoded into how many groups' snapshots in one tick
    /// (1.0 for one-group rooms; up to the block overlap for cell AOI; the
    /// visibility-table out-degree for PVS). Default: `0` (untracked) —
    /// the core's own test logics need not implement it.
    fn encoded_records(&mut self) -> u64 {
        0
    }
}

/// Per-connection state in a room (or shard) connection table. `pub(crate)`
/// because the shard actor reuses the same table shape (a shard's `conns`
/// is the shard's share of the room's connections — see `crate::shard`).
pub(crate) struct RoomConn<G> {
    pub(crate) out: mpsc::Sender<FrameBatch>,
    /// This connection's input, written by its connection actor as frames
    /// arrive; pulled non-blockingly at each step.
    pub(crate) actions: Inbox<Action>,
    pub(crate) entity: EntityId,
    /// Snapshot group this connection belongs to (recomputed every tick via
    /// [`RoomLogic::group_of`]).
    pub(crate) group: G,
    /// The fan-out batch buffer, reused across ticks (the measured floor
    /// carried one `Vec::with_capacity(2)` per connection per tick; the
    /// capacity is retained so the 0-2-frame batch never allocates again
    /// after warm-up — see `docs/ROADMAP.md`, the floor breakdown).
    pub(crate) batch: FrameBatch,
}

/// Per-group broadcast state, kept across ticks. `pub(crate)` because the
/// shard actor reuses the same group table shape (see `crate::shard`).
pub(crate) struct GroupState {
    /// Last snapshot payload emitted for the group; re-sent on keep-alive
    /// ticks when the group is unchanged.
    pub(crate) last: Option<bytes::Bytes>,
    /// The payload fanned out to the members this tick (the emitted
    /// snapshot or the keep-alive re-send); `None` = nothing shipped.
    pub(crate) sent: Option<bytes::Bytes>,
    /// A group that has members but has never emitted is in contract
    /// violation (a fresh group's first tick is a membership change and
    /// must emit) — warn once for it instead of every tick.
    pub(crate) never_emitted_warned: bool,
    /// A snapshot over `max_snapshot_bytes` is a standing property of the
    /// group (its content does not shrink on its own), so warn once for it
    /// rather than on every tick of every room.
    pub(crate) size_warned: bool,
}

/// The room's local metric counters (all cumulative; see [`crate::metrics`]).
/// Owned by the room and never shared: each step the room builds a
/// [`RoomSample`] from them and hands it to the collector over the
/// metrics channel (synchronous unbounded send — no await). `pub(crate)`
/// because the shard actor reuses the same counter shape (a shard's sample
/// is a [`RoomSample`] under its derived sample id — see `crate::shard`).
///
/// Manual `Default` (not derived): `[u32; FINE_HIST_BINS]` exceeds the
/// derived-`Default` array bound (32); every field is a zero.
#[derive(Debug)]
pub(crate) struct RoomCounters {
    /// Broadcast `Lagged` occurrences / missed tick indices.
    pub(crate) lagged_events: u64,
    pub(crate) lagged_ticks: u64,
    /// Step body duration µs: min / max / sum + histogram (binning).
    pub(crate) step_min_us: u64,
    pub(crate) step_max_us: u64,
    pub(crate) step_sum_us: u64,
    pub(crate) step_hist: [u64; HIST_BINS],
    /// Fine step-duration histogram (fixed 8 µs bins, `[0, 4096 µs)` —
    /// sub-budget resolution; steps at/above the cap stay in `step_hist`
    /// only. See `metrics::FINE_HIST_*`.
    pub(crate) step_fine_hist: [u32; FINE_HIST_BINS],
    /// Tick processing latency µs (step start − ticker `at`): min/max/sum.
    pub(crate) late_min_us: u64,
    pub(crate) late_max_us: u64,
    pub(crate) late_sum_us: u64,
    /// Outbound batches dropped at the fan-out (slow client), cumulative.
    pub(crate) dropped_frames: u64,
    /// Input actions dropped by the room, cumulative. **Always 0 since the
    /// READ phase became a bounded pull** (per-connection per-tick budget +
    /// room-level pull budget — see the phase's comment): overflow stays in
    /// the senders' bounded channels and the only input-loss point is a
    /// connection's own full action channel, counted *there*, attributed to
    /// its sender (see `ConnSample::actions_dropped`). The counter is kept
    /// for the report's format compatibility.
    pub(crate) dropped_actions: u64,
    /// Keep-alive re-sends, cumulative.
    pub(crate) keepalive_resends: u64,
    /// Group snapshots encoded, cumulative (+ encoded bytes, max payload).
    pub(crate) snapshots: u64,
    pub(crate) snap_bytes: u64,
    pub(crate) snap_bytes_max: u32,
    /// Snapshots whose payload exceeded `max_snapshot_bytes`, cumulative.
    pub(crate) snap_overflows: u64,
    /// Entity records encoded (summed over all groups, via
    /// [`RoomLogic::encoded_records`]), cumulative. Together with the
    /// broadcastable entity count this is the *overlap multiplier*: how
    /// many times the same entity was encoded into group snapshots per
    /// tick (1.0 for one-group rooms, up to the block overlap for cell
    /// AOI, the visibility-table out-degree for PVS).
    pub(crate) snap_records: u64,
    /// Metric samples dropped on a full (bounded) metrics channel,
    /// cumulative.
    pub(crate) metrics_dropped: u64,
    /// Snapshot + private bytes/frames shipped to the room's connections,
    /// cumulative.
    pub(crate) shipped_bytes: u64,
    pub(crate) shipped_frames: u64,
    pub(crate) private_frames: u64,
    /// Joins / leaves processed on the control channel, cumulative.
    pub(crate) joins: u64,
    pub(crate) leaves: u64,
    /// Largest snapshot group this tick (recomputed in the broadcast
    /// phase; carried in the per-step sample as a gauge).
    pub(crate) step_max_group: u32,
}

impl Default for RoomCounters {
    fn default() -> Self {
        Self {
            lagged_events: 0,
            lagged_ticks: 0,
            step_min_us: 0,
            step_max_us: 0,
            step_sum_us: 0,
            step_hist: [0; HIST_BINS],
            step_fine_hist: [0; FINE_HIST_BINS],
            late_min_us: 0,
            late_max_us: 0,
            late_sum_us: 0,
            dropped_frames: 0,
            dropped_actions: 0,
            keepalive_resends: 0,
            snapshots: 0,
            snap_bytes: 0,
            snap_bytes_max: 0,
            snap_overflows: 0,
            snap_records: 0,
            metrics_dropped: 0,
            shipped_bytes: 0,
            shipped_frames: 0,
            private_frames: 0,
            joins: 0,
            leaves: 0,
            step_max_group: 0,
        }
    }
}

/// The room actor. Owns the world, the connection table, and the group
/// table; everything mutable is local, so no synchronization is needed.
///
/// `G` is the game logic's group key ([`RoomLogic::GroupKey`]); the room
/// stores per-group state (last snapshot, this tick's ship, diagnostics)
/// under it.
pub struct RoomActor<W, G> {
    config: RoomConfig,
    world: W,
    logic: Box<dyn RoomLogic<W, GroupKey = G>>,
    tick_rx: broadcast::Receiver<TickInfo>,
    control_rx: Inbox<RoomControl>,
    conns: HashMap<ConnectionId, RoomConn<G>>,
    groups: HashMap<G, GroupState>,
    /// Number of global ticks between steps (1 = room rate == global rate).
    run_every: u64,
    /// Wall-clock instant of the last step (for dt).
    last_at: Option<Instant>,
    /// Room's own step counter (keep-alive cadence is in *room* steps, so a
    /// slower room keeps the same keep-alive rate in real time).
    steps: u64,
    /// Every k-th step, unchanged groups re-send their last snapshot
    /// (`None` = keep-alive disabled).
    keepalive_every: Option<u64>,
    /// Every k-th step, the room emits a metrics sample (ties the send
    /// cadence to the collector's report cadence — A2; 1 = every step).
    metrics_every: u64,
    /// The room's tick budget in µs (one period): the histogram's overflow
    /// boundary (A1). Precomputed once (it is constant over the room's life).
    budget_us: u64,
    /// Local metric counters (see [`RoomCounters`]).
    m: RoomCounters,
    /// Outbound metrics path: a *bounded* channel mailbox (see
    /// [`crate::metrics`]); the room sends with the synchronous `try_send`,
    /// so it adds no await (drops are counted and harmless).
    metrics: mpsc::Sender<MetricsEvent>,
}

impl<W, G> RoomActor<W, G>
where
    G: Eq + Hash + Clone + Debug,
{
    pub fn new(
        config: RoomConfig,
        world: W,
        logic: Box<dyn RoomLogic<W, GroupKey = G>>,
        tick_rx: broadcast::Receiver<TickInfo>,
        control_rx: Inbox<RoomControl>,
        run_every: u64,
        // Outbound metrics path (see `crate::metrics`): a bounded channel;
        // the room sends with the synchronous `try_send` (no await). A
        // dropped receiver makes the send fail (ignored) — the room does not
        // observe it.
        metrics: mpsc::Sender<MetricsEvent>,
    ) -> Self {
        let keepalive_every = if config.keepalive_hz > 0.0 {
            if config.keepalive_hz > config.tick_hz {
                // The registry rejects this relationship at room creation;
                // this warn covers direct construction (library use) that
                // bypasses it, so the misconfiguration can never be silent:
                // the cadence clamps to every step, and the "silence when
                // unchanged" gain for this room is gone.
                warn!(
                    room = %config.id,
                    keepalive_hz = config.keepalive_hz,
                    tick_hz = config.tick_hz,
                    "keepalive_hz exceeds tick_hz: keep-alive clamps to every \
                     step (unchanged groups re-send on every step and the \
                     silence gain is lost); set keepalive_hz <= tick_hz"
                );
            }
            Some(((config.tick_hz / config.keepalive_hz).round() as u64).max(1))
        } else {
            None
        };
        // A2: sample at most every `metrics_every` steps, so the send cadence
        // tracks the collector's report cadence. `> tick_hz` (or `<= 0`)
        // clamps to every step.
        let metrics_every = if config.metrics_cadence_hz > 0.0 {
            ((config.tick_hz / config.metrics_cadence_hz).round() as u64).max(1)
        } else {
            1
        };
        let budget_us = config.period().as_micros() as u64;
        Self {
            config,
            world,
            logic,
            tick_rx,
            control_rx,
            conns: HashMap::new(),
            groups: HashMap::new(),
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

    /// Run until the ticker channel closes or a control `Shutdown` is
    /// processed on a tick.
    pub async fn run(mut self) {
        debug!(
            room = %self.config.id,
            hz = self.config.tick_hz,
            run_every = self.run_every,
            "room actor started"
        );
        loop {
            let t = match self.tick_rx.recv().await {
                Ok(t) => t,
                // We fell behind by more than the broadcast buffer: skip
                // these; the wall-clock dt of the next step covers the gap
                // (bounded by the catch-up cap). Counted for metrics:
                // `lagged_*` is the room's "missed ticks" signal.
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    self.m.lagged_events += 1;
                    self.m.lagged_ticks += missed;
                    warn!(
                        room = %self.config.id,
                        missed,
                        "lagged behind global ticker; next step catches up via dt"
                    );
                    continue;
                }
                // Ticker aborted: global stop signal.
                Err(broadcast::error::RecvError::Closed) => break,
            };
            if t.tick % self.run_every != 0 {
                continue; // this room runs slower: every k-th global tick
            }
            if !self.step(&t) {
                break;
            }
        }
        self.logic.on_shutdown();
        debug!(
            room = %self.config.id,
            dropped_frames = self.m.dropped_frames,
            "room actor stopped"
        );
    }

    /// One full step: control → read → convert → systems → broadcast,
    /// measured (tick latency + step body duration) and flushed to the
    /// metrics channel once at the end. Synchronous: the send is an
    /// unbounded (non-parking) mailbox send, so the room's only await
    /// stays `tick_rx.recv()`. Returns `false` when the actor should stop.
    fn step(&mut self, t: &TickInfo) -> bool {
        self.steps += 1;

        // -- tick latency: how late this room processes the tick (step
        //    start minus the ticker's timestamp; covers broadcast delivery
        //    + the room's queue behind the ticker).
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
        self.m.step_hist[crate::metrics::hist_index(self.budget_us, step_us)] += 1;
        // The fine histogram runs ALONGSIDE the log2 one (sub-budget
        // resolution; the overflow semantics of `step_hist` are untouched).
        // One saturating increment, integer only (no float on the hot
        // path); steps at/above the cap are simply absent from it.
        if let Some(fi) = crate::metrics::fine_hist_index(step_us) {
            self.m.step_fine_hist[fi] = self.m.step_fine_hist[fi].saturating_add(1);
        }

        // A2: emit a sample at most every `metrics_every` steps (the send
        // cadence tracks the collector's report cadence; the counters are
        // cumulative, so skipping in between loses nothing). A3: bounded
        // channel + synchronous `try_send` — a full channel drops this
        // sample (harmless: the next sample carries everything) and counts
        // it; a closed channel just fails silently. No await either way, so
        // the tick body stays synchronous.
        if self.steps.is_multiple_of(self.metrics_every)
            && let Err(mpsc::error::TrySendError::Full(_)) =
                self.metrics.try_send(MetricsEvent::Room(self.sample()))
        {
            self.m.metrics_dropped += 1;
        }
        keep
    }

    /// The five phases (moved out of [`Self::step`] so the whole body is
    /// measurable). Synchronous. Returns `false` when the actor should
    /// stop.
    fn step_phases(&mut self, t: &TickInfo) -> bool {
        // -- time: wall-clock since the last step; covers missed ticks
        //    (frame-rate independent), capped for pathological stalls.
        let dt = {
            let last = self.last_at.replace(t.at);
            match last {
                Some(last) => {
                    let elapsed = t.at.saturating_duration_since(last);
                    let cap = self.config.period() * self.config.max_catchup;
                    if elapsed > cap {
                        warn!(
                            room = %self.config.id,
                            ?elapsed,
                            ?cap,
                            "long stall: catch-up dt clamped (sim temporarily slower than real time)"
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

        // -- Phase 0 — CONTROL (before actions: a fresh join's action
        //    channel is only registered once its Join has been processed).
        while let Ok(c) = self.control_rx.try_recv() {
            if !self.handle_control(c) {
                return false;
            }
        }

        // -- Phase 1 — READ: pull each connection's actions (non-blocking;
        //    per-connection isolation — one flooder only fills its own
        //    channel), bounded twice:
        //
        //    (a) per-connection per-tick budget
        //    (`max_actions_per_conn_per_tick`): a single connection cannot
        //    consume more than this of the tick's pull budget. This is the
        //    *fairness* cut — before it, the merged list's overflow dropped
        //    the oldest entries regardless of owner, so one flooding
        //    connection could push other connections' earlier input out of
        //    a tick. Now a flooder's excess stays in its own bounded
        //    channel: it is pulled on later ticks (deferred), and if the
        //    flooder outpaces the budget sustainedly its own `try_send`
        //    hits the full channel and drops *its own* newest input,
        //    counted by the connection actor and attributed to it.
        //
        //    (b) room-level pull budget (`max_pending_actions`): the total
        //    number of actions pulled this tick. It bounds the tick's
        //    ingest cost — the measured 10k-connection wall is the room's
        //    serial step path, and a mass flood must not be able to
        //    exceed it. It is a *pull* bound, not a drop bound: when it is
        //    exhausted the room simply pulls no more this tick; the
        //    remainder waits in the senders' bounded channels.
        //
        //    Consequence: the room never drops an action (`dropped_actions`
        //    stays 0). The architecture's only input-loss point is a
        //    connection's own full action channel — self-inflicted and
        //    attributed (see `conn::ConnectionActor`).
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
                    Err(_) => break, // channel drained
                }
            }
            if budget == 0 {
                break;
            }
        }

        // -- Phase 2 — CONVERT: actions → component writes (game logic).
        self.logic.ingest(&mut self.world, &ctx, &mut actions);

        // -- Phase 3 — SYSTEMS: run the ordered game systems.
        self.logic.update(&mut self.world, &ctx);

        // -- Phase 4 — BROADCAST: one snapshot per group, frozen once and
        //    shared by reference; per-connection fan-out of
        //    [group snapshot] + [private?].
        //    (the step counter was bumped at the top of `step`)
        self.broadcast_phase(&ctx);
        true
    }

    /// Build this room's metrics sample (cumulative counters + current
    /// gauges) for the per-step flush (see [`crate::metrics`]).
    fn sample(&self) -> RoomSample {
        RoomSample {
            room: self.config.id,
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
            groups: self.groups.len() as u32,
            members: self.conns.len() as u32,
            max_group: self.m.step_max_group,
            metrics_dropped: self.m.metrics_dropped,
        }
    }

    /// Phase 4, in four passes. Field-level borrows keep the logic, the
    /// world, the connection table, and the group table independently
    /// accessible — disjoint fields, no synchronization.
    fn broadcast_phase(&mut self, ctx: &TickCtx) {
        let snap_op = self.logic.snapshot_op();
        let priv_op = self.logic.private_op();

        // 4a. Recompute each connection's group (a group may depend on the
        //     world, e.g. zones).
        for (conn, rc) in self.conns.iter_mut() {
            rc.group = self.logic.group_of(&self.world, *conn);
        }

        // 4b. Rebuild the group table. Membership churn (join/leave)
        //     shows up here as a different member set — the game logic's
        //     "no change" test in `snapshot` must account for it.
        let mut members: HashMap<G, Vec<ConnectionId>> = HashMap::new();
        for (conn, rc) in &self.conns {
            members.entry(rc.group.clone()).or_default().push(*conn);
        }        // Gauge for the per-step sample: the largest group this tick.
        self.m.step_max_group =
            members.values().map(Vec::len).max().unwrap_or(0) as u32;
        // Drop groups whose members all left (frees the cached snapshot).
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
                    // The group existed on the previous tick too. If it
                    // has members but has still never emitted, its first
                    // tick's snapshot returned `false` although a fresh
                    // group's first tick is a membership change — a
                    // contract violation the room can detect precisely
                    // (a merely *quiet* group will not trigger this: it
                    // has emitted at least once).
                    if st.last.is_none() && !st.never_emitted_warned {
                        st.never_emitted_warned = true;
                        // Cold path (once per such group). `st`'s last use
                        // ended above, so the key can be borrowed (not
                        // cloned): tracing formats the field within the
                        // statement.
                        let group_key = e.key();
                        warn!(
                            room = %self.config.id,
                            ?group_key,
                            members = m.len(),
                            "snapshot group has members but has never emitted: \
                             RoomLogic::snapshot returned `false` on the \
                             group's first tick although a fresh group's \
                             first tick is a membership change and must \
                             emit; its members receive nothing except \
                             keep-alive re-sends of a cache that was never \
                             set. Check the logic's per-group bookkeeping \
                             (see RoomLogic::snapshot)."
                        );
                    }
                }
            }
        }

        // 4c. One snapshot per group: encode ONCE, share the result by
        //     reference (the scratch buffer is reused across groups and
        //     ticks; `split_to` hands the room a zero-copy `Bytes` view of
        //     its growing heap allocation — no per-group allocation churn).
        //
        //     Keep-alive (on the cadence tick, whether the group emitted
        //     this tick or not): full-snapshot logics keep the default
        //     behaviour (an unchanged group re-sends its cached snapshot —
        //     the unchanged group's cache is the very snapshot that was
        //     just encoded, and an active group's tick payload already is
        //     a fresh full, so re-sending `last` is bit-identical);
        //     delta-mode logics return `true` with a freshly encoded FULL
        //     that REPLACES this tick's payload — a client that lost the
        //     delta (or several) is healed within one keep-alive period
        //     whether its group is active or silent (see
        //     `RoomLogic::keepalive`).
        let keep_due = self
            .keepalive_every
            .map(|every| self.steps.is_multiple_of(every))
            .unwrap_or(false);
        let mut buf = bytes::BytesMut::new();
        for (group, st) in self.groups.iter_mut() {
            buf.clear();
            let emitted = self.logic.snapshot(&mut self.world, ctx, group, &mut buf);
            if emitted {
                if buf.len() > self.config.max_snapshot_bytes && !st.size_warned {
                    st.size_warned = true;
                    warn!(
                        room = %self.config.id,
                        ?group,
                        bytes = buf.len(),
                        max = self.config.max_snapshot_bytes,
                        "snapshot exceeds max_snapshot_bytes (rUDP MTU readiness)"
                    );
                }
                // Metrics: one encoded snapshot and its payload size.
                self.m.snapshots += 1;
                let n = buf.len() as u64;
                self.m.snap_bytes = self.m.snap_bytes.saturating_add(n);
                if n > self.m.snap_bytes_max as u64 {
                    self.m.snap_bytes_max = n as u32;
                }
                // Count every oversized emit (the warn above fires once per
                // group; this counts all of them — the MTU/AOI signal).
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
            // (re-send `last` — bit-identical for unchanged groups).
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
                    // A freshly encoded payload (a delta-mode full): count
                    // it like any other encoded snapshot.
                    if buf.len() > self.config.max_snapshot_bytes && !st.size_warned {
                        st.size_warned = true;
                        warn!(
                            room = %self.config.id,
                            ?group,
                            bytes = buf.len(),
                            max = self.config.max_snapshot_bytes,
                            "snapshot exceeds max_snapshot_bytes (rUDP MTU readiness)"
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

        // 4c'. Overlap metric (see `RoomCounters::snap_records`): the
        //      payload is opaque to the core, so the number of encoded
        //      records can only come from the logic — polled exactly once
        //      per step, right after the phase that produced them.
        self.m.snap_records =
            self.m.snap_records.saturating_add(self.logic.encoded_records());

        // 4d. Per-connection fan-out: one batch per connection — the
        //     group's shared snapshot (Bytes refcount, never copied) plus
        //     the connection's private frame, when the logic has one.
        let mut dropped: u64 = 0;
        // The private-frame scratch is reused across connections (the
        // payload is split off, the capacity retained — no per-connection
        // per-tick allocation).
        let mut pbuf = bytes::BytesMut::new();
        for (conn, rc) in self.conns.iter_mut() {
            // The batch buffer is reused across ticks (floor breakdown: the
            // per-tick `Vec::with_capacity(2)` was a measured slice). It is
            // handed to the channel with `mem::take` — zero allocation,
            // the retained capacity is what makes the reuse free — and, if
            // the outbound channel is full, put back for the next tick.
            rc.batch.clear();
            if let Some(payload) = self
                .groups
                .get(&rc.group)
                .and_then(|st| st.sent.clone())
            {
                // Metrics: one shipped frame and its wire payload size.
                self.m.shipped_frames += 1;
                self.m.shipped_bytes = self.m.shipped_bytes.saturating_add(payload.len() as u64);
                rc.batch.push(gsb_protocol::FrameBody::new(snap_op, payload));
            }
            pbuf.clear();
            if self.logic.private(&mut self.world, *conn, &rc.group, &mut pbuf) {
                // Metrics: one shipped private frame and its payload size.
                self.m.private_frames += 1;
                self.m.shipped_frames += 1;
                self.m.shipped_bytes = self.m.shipped_bytes.saturating_add(pbuf.len() as u64);
                rc.batch.push(gsb_protocol::FrameBody::new(priv_op, pbuf.split_to(pbuf.len()).freeze()));
            }
            if !rc.batch.is_empty() {
                let batch = std::mem::take(&mut rc.batch);
                if let Err(e) = rc.out.try_send(batch) {
                    // Outbound channel full: the batch is dropped. Snapshots are
                    // self-contained, so this costs the client at most one
                    // snapshot of staleness (keep-alive bounds it). The buffer
                    // goes back for the next tick.
                    dropped += 1;
                    rc.batch = e.into_inner();
                }
            }
        }
        self.m.dropped_frames += dropped;
    }

    fn handle_control(&mut self, c: RoomControl) -> bool {
        match c {
            RoomControl::Join { conn, out, reply } => {
                // A join supersedes any stale state this connection had
                // (e.g. a leave queued behind it in the control channel).
                if self.conns.remove(&conn).is_some() {
                    self.logic.on_leave(&mut self.world, conn);
                }
                // Capacity: the room knows its own membership — this is the
                // only place a join can structurally fail. A fresh join to a
                // full room is rejected (no entity, no channel, no state);
                // a re-join of an existing member (removed above) never
                // hits the cap because it supersedes itself.
                if let Some(cap) = self.config.max_players
                    && self.conns.len() >= cap
                {
                    warn!(
                        room = %self.config.id,
                        %conn,
                        capacity = cap,
                        "room full; join rejected (CoreError::RoomFull)"
                    );
                    let _ = reply.send(Err(CoreError::RoomFull(self.config.id.0)));
                    return true;
                }
                let entity = self.logic.on_join(&mut self.world, conn);
                self.m.joins += 1;
                let (act_tx, act_rx) = mpsc::channel(self.config.action_capacity);
                self.conns.insert(
                    conn,
                    RoomConn {
                        out,
                        actions: act_rx,
                        entity,
                        // Authoritative value is recomputed every broadcast
                        // phase (a group may depend on the world); this is
                        // the join-time value.
                        group: self.logic.group_of(&self.world, conn),
                        batch: Vec::new(),
                    },
                );
                let _ = reply.send(Ok((entity, act_tx)));
                debug!(room = %self.config.id, %conn, entity, "player joined");
                true
            }
            RoomControl::Leave { conn, entity } => {
                // Stale-leave guard: only the entity this connection
                // currently owns.
                if self.conns.get(&conn).map(|c| c.entity) == Some(entity) {
                    self.conns.remove(&conn);
                    self.logic.on_leave(&mut self.world, conn);
                    self.m.leaves += 1;
                    debug!(room = %self.config.id, %conn, entity, "player left");
                }
                true
            }
            RoomControl::Shutdown => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::channel;
    use gsb_protocol::FrameBody;
    use std::time::Duration;

    /// A metrics sender whose receiver is dropped immediately: the room's
    /// per-step send fails and is ignored (the metric path is covered by
    /// the dedicated metrics-flow test and by gsb-server's tests).
    fn null_metrics_tx() -> mpsc::Sender<MetricsEvent> {
        let (tx, _rx) = mpsc::channel(1);
        tx
    }

    /// Test logic recording the dt of every step over a channel (no locks:
    /// this crate's lint forbids them even in tests).
    struct RecLogic {
        dts: mpsc::Sender<Duration>,
        ops: mpsc::Sender<u16>,
    }

    impl RoomLogic<()> for RecLogic {
        type GroupKey = ();

        fn snapshot_op(&self) -> u16 {
            0x7000
        }
        fn private_op(&self) -> u16 {
            0x7001
        }

        fn group_of(&self, _w: &(), _c: ConnectionId) -> Self::GroupKey {
            Default::default()
        }

        fn snapshot(
            &mut self,
            _w: &mut (),
            _c: &TickCtx,
            _g: &Self::GroupKey,
            _o: &mut bytes::BytesMut,
        ) -> bool {
            false
        }

        fn on_join(&mut self, _w: &mut (), _c: ConnectionId) -> EntityId {
            1
        }
        fn on_leave(&mut self, _w: &mut (), _c: ConnectionId) {}
        fn ingest(&mut self, _w: &mut (), _c: &TickCtx, actions: &mut Vec<Action>) {
            for a in actions.drain(..) {
                let _ = self.ops.try_send(a.op);
            }
        }
        fn update(&mut self, _w: &mut (), ctx: &TickCtx) {
            let _ = self.dts.try_send(ctx.dt);
        }
    }

    struct Harness {
        tick_tx: broadcast::Sender<TickInfo>,
        control: Mailbox<RoomControl>,
        handle: tokio::task::JoinHandle<()>,
        t0: Instant,
        next_tick: u64,
        run_every: u64,
    }

    impl Harness {
        fn new(run_every: u64, config: RoomConfig, logic: RecLogic) -> Self {
            let (tick_tx, _first) = broadcast::channel(64);
            let tick_rx = tick_tx.subscribe();
            let (control, control_rx) = channel(config.control_capacity);
            let actor = RoomActor::new(
                config,
                (),
                Box::new(logic),
                tick_rx,
                control_rx,
                run_every,
                null_metrics_tx(),
            );
            Self {
                tick_tx,
                control,
                handle: tokio::spawn(actor.run()),
                t0: Instant::now(),
                next_tick: 0,
                run_every: run_every.max(1),
            }
        }

        /// Send the next global tick with an exact synthetic timestamp:
        /// `at = t0 + n * period`, so dts are deterministic.
        fn tick(&mut self, period: Duration) {
            self.next_tick += 1;
            let at =
                self.t0 + Duration::from_secs_f64(self.next_tick as f64 * period.as_secs_f64());
            self.tick_tx
                .send(TickInfo {
                    tick: self.next_tick,
                    at,
                })
                .expect("room subscriber alive");
        }

        async fn join(
            &mut self,
            conn: ConnectionId,
            ticks_needed: u64,
        ) -> (EntityId, Mailbox<Action>) {
            let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(8);
            let (reply_tx, reply_rx) =
                oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
            self.control
                .send(RoomControl::Join {
                    conn,
                    out: out_tx,
                    reply: reply_tx,
                })
                .await
                .expect("control alive");
            // Control is processed on the room's next *step*; feed enough
            // ticks to guarantee one (run_every + slack).
            for _ in 0..ticks_needed {
                self.tick(Duration::from_secs_f64(1.0 / 30.0));
            }
            let (entity, actions) = tokio::time::timeout(Duration::from_secs(2), reply_rx)
                .await
                .expect("timed out waiting for join reply")
                .expect("join reply dropped")
                .expect("join accepted (room not full)");
            (entity, actions)
        }

        async fn shutdown(mut self) {
            self.control
                .send(RoomControl::Shutdown)
                .await
                .expect("control alive");
            // Control is processed on the room's next step: feed enough
            // ticks to guarantee one.
            for _ in 0..self.run_every {
                self.tick(Duration::from_secs_f64(1.0 / 30.0));
            }
            tokio::time::timeout(Duration::from_secs(2), &mut self.handle)
                .await
                .expect("room did not shut down")
                .expect("room task panicked");
        }
    }

    #[tokio::test]
    async fn room_steps_on_ticks_and_pulls_actions() {
        let (dt_tx, mut dts) = mpsc::channel(16);
        let (op_tx, mut ops) = mpsc::channel(16);
        let mut h = Harness::new(
            1,
            RoomConfig {
                id: RoomId(1),
                ..Default::default()
            },
            RecLogic {
                dts: dt_tx,
                ops: op_tx,
            },
        );
        let period = Duration::from_secs_f64(1.0 / 30.0);

        let (entity, actions) = h.join(ConnectionId(7), 1).await;
        assert_eq!(entity, 1);

        // Actions flow over the per-connection channel and are pulled at
        // the next step.
        actions
            .send(Action {
                conn: ConnectionId(7),
                op: 0x1001,
                payload: bytes::Bytes::new(),
            })
            .await
            .unwrap();
        h.tick(period);
        h.tick(period);

        // 3 steps so far (one from the join helper, two here): dts are the
        // exact nominal period (synthetic timestamps).
        for _ in 0..3 {
            let dt = tokio::time::timeout(Duration::from_secs(2), dts.recv())
                .await
                .expect("timed out")
                .expect("dts closed");
            assert!(
                dt.abs_diff(period) < Duration::from_micros(1),
                "dt {dt:?} != period {period:?}"
            );
        }
        let op = tokio::time::timeout(Duration::from_secs(2), ops.recv())
            .await
            .expect("timed out")
            .expect("ops closed");
        assert_eq!(op, 0x1001);

        h.shutdown().await;
    }

    #[tokio::test]
    async fn catchup_clamps_dt_after_long_gap() {
        let (dt_tx, mut dts) = mpsc::channel(16);
        let (op_tx, _ops) = mpsc::channel(16);
        let mut h = Harness::new(
            1,
            RoomConfig {
                id: RoomId(1),
                ..Default::default()
            }, // max_catchup = 4
            RecLogic {
                dts: dt_tx,
                ops: op_tx,
            },
        );
        let period = Duration::from_secs_f64(1.0 / 30.0);

        // Two normal steps, then a 2 s gap: the step's dt must be clamped
        // to 4 periods (frame-rate independence in steady state; bounded
        // slow-motion across the stall).
        h.tick(period);
        h.tick(period);
        h.next_tick += 1; // consume index 3 as "missed"
        let at = h.t0 + period * 4 + Duration::from_secs(2);
        h.tick_tx
            .send(TickInfo { tick: 4, at })
            .expect("subscriber alive");

        let first = dts.recv().await.expect("dts");
        let second = dts.recv().await.expect("dts");
        let third = dts.recv().await.expect("dts");
        assert!(first.abs_diff(period) < Duration::from_micros(1));
        assert!(second.abs_diff(period) < Duration::from_micros(1));
        assert!(
            third.abs_diff(period * 4) < Duration::from_micros(1),
            "clamped dt {third:?} != 4 * period {period:?}"
        );

        h.shutdown().await;
    }

    #[tokio::test]
    async fn slower_room_steps_on_every_kth_global_tick() {
        let (dt_tx, mut dts) = mpsc::channel(16);
        let (op_tx, _ops) = mpsc::channel(16);
        // Room at 15 Hz under a 60 Hz global ticker: run_every = 4.
        let mut h = Harness::new(
            4,
            RoomConfig {
                id: RoomId(1),
                tick_hz: 15.0,
                ..Default::default()
            },
            RecLogic {
                dts: dt_tx,
                ops: op_tx,
            },
        );
        let global_period = Duration::from_secs_f64(1.0 / 60.0);

        for _ in 0..8 {
            h.tick(global_period);
        }

        // Steps happened on ticks 4 and 8 only: 2 dts of 4 global periods.
        let room_period = Duration::from_secs_f64(1.0 / 15.0);
        for _ in 0..2 {
            let dt = tokio::time::timeout(Duration::from_secs(2), dts.recv())
                .await
                .expect("timed out")
                .expect("dts closed");
            assert!(
                dt.abs_diff(room_period) < Duration::from_micros(1),
                "dt {dt:?} != room period {room_period:?}"
            );
        }

        h.shutdown().await;
    }

    #[tokio::test]
    async fn lagged_receiver_catches_up_and_keeps_stepping() {
        let (dt_tx, mut dts) = mpsc::channel(16);
        let (op_tx, _ops) = mpsc::channel(16);
        // Buffer of 2: flooding it makes the receiver lag deterministically
        // *before* the room starts consuming.
        let (tick_tx, lagged_rx) = broadcast::channel(2);
        let (_control, control_rx) = channel(16);
        let t0 = Instant::now();
        let period = Duration::from_secs_f64(1.0 / 30.0);
        for i in 1..=10u64 {
            tick_tx
                .send(TickInfo {
                    tick: i,
                    at: t0 + Duration::from_secs_f64(i as f64 * period.as_secs_f64()),
                })
                .expect("channel open");
        }
        let actor = RoomActor::new(
            RoomConfig {
                id: RoomId(1),
                ..Default::default()
            },
            (),
            Box::new(RecLogic {
                dts: dt_tx,
                ops: op_tx,
            }),
            lagged_rx,
            control_rx,
            1,
            null_metrics_tx(),
        );
        let handle = tokio::spawn(actor.run());

        // The room skips the lagged ticks (Lagged → continue) and steps on
        // the two still-buffered ticks (9 and 10), then the sender is
        // dropped → Closed → clean exit.
        drop(tick_tx);
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("room did not exit on closed ticker")
            .expect("room task panicked");
        let mut count = 0;
        while let Ok(dt) = dts.try_recv() {
            count += 1;
            assert!(dt <= period * 2);
        }
        assert_eq!(count, 2, "expected exactly the two buffered ticks");
    }

    #[tokio::test]
    async fn room_exits_when_ticker_closes() {
        let (dt_tx, _dts) = mpsc::channel(16);
        let (op_tx, _ops) = mpsc::channel(16);
        let (tick_tx, tick_rx) = broadcast::channel(4);
        let (_control, control_rx) = channel(16);
        let actor = RoomActor::new(
            RoomConfig {
                id: RoomId(1),
                ..Default::default()
            },
            (),
            Box::new(RecLogic {
                dts: dt_tx,
                ops: op_tx,
            }),
            tick_rx,
            control_rx,
            1,
            null_metrics_tx(),
        );
        let handle = tokio::spawn(actor.run());
        drop(tick_tx); // ticker aborted → broadcast closes
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("room did not exit on closed ticker")
            .expect("room task panicked");
    }

    #[test]
    fn config_period() {
        let c = RoomConfig {
            tick_hz: 30.0,
            ..Default::default()
        };
        assert!((c.period().as_secs_f64() - 1.0 / 30.0).abs() < 1e-9);
    }

    // -----------------------------------------------------------------
    // Group machinery: per-connection groups, private frames, silence on
    // no-change, keep-alive re-send.
    // -----------------------------------------------------------------

    /// Test logic that exercises the group machinery end to end:
    /// - `GroupKey = ConnectionId`: every connection is its own group, so a
    ///   group's snapshot must never reach another connection;
    /// - emission is gated on a per-group dirty set that membership
    ///   changes (join/leave) set — per the room contract, a join/leave
    ///   **is** a change (for its own group); a clean group reports "no
    ///   change" and nothing is sent (except the room's keep-alive re-send);
    /// - `private` emits a frame for exactly one designated connection.
    struct GroupLogic {
        conn_entity: HashMap<ConnectionId, u64>,
        next: u64,
        dirty: std::collections::HashSet<ConnectionId>,
        step_no: u64,
        steps: mpsc::Sender<u64>,
    }

    impl RoomLogic<()> for GroupLogic {
        type GroupKey = ConnectionId;

        fn snapshot_op(&self) -> u16 {
            0x7010
        }
        fn private_op(&self) -> u16 {
            0x7011
        }

        fn group_of(&self, _w: &(), conn: ConnectionId) -> Self::GroupKey {
            conn
        }

        fn snapshot(
            &mut self,
            _w: &mut (),
            _c: &TickCtx,
            group: &Self::GroupKey,
            out: &mut bytes::BytesMut,
        ) -> bool {
            if !self.dirty.remove(group) {
                return false; // unchanged since the last emission
            }
            let entity = self.conn_entity.get(group).copied().unwrap_or(0);
            out.extend_from_slice(&entity.to_le_bytes());
            true
        }

        fn private(
            &mut self,
            _w: &mut (),
            conn: ConnectionId,
            _group: &ConnectionId,
            out: &mut bytes::BytesMut,
        ) -> bool {
            if conn == ConnectionId(0x70) {
                out.extend_from_slice(&u32::MAX.to_le_bytes());
                true
            } else {
                false
            }
        }

        fn on_join(&mut self, _w: &mut (), conn: ConnectionId) -> EntityId {
            self.next += 1;
            self.conn_entity.insert(conn, self.next);
            self.dirty.insert(conn); // membership changed (this group)
            self.next
        }
        fn on_leave(&mut self, _w: &mut (), conn: ConnectionId) {
            self.conn_entity.remove(&conn);
            // The leaver's group is gone (and clean); the remaining groups
            // are unchanged for a per-connection grouping.
            self.dirty.remove(&conn);
        }
        fn ingest(&mut self, _w: &mut (), _c: &TickCtx, actions: &mut Vec<Action>) {
            actions.clear();
        }
        fn update(&mut self, _w: &mut (), _c: &TickCtx) {
            self.step_no += 1;
            let _ = self.steps.try_send(self.step_no);
        }
    }

    /// Manual-ticker harness for `GroupLogic` rooms.
    struct GLRoom {
        tick_tx: broadcast::Sender<TickInfo>,
        control: Mailbox<RoomControl>,
        handle: tokio::task::JoinHandle<()>,
        t0: Instant,
        next_tick: u64,
    }

    impl GLRoom {
        fn new(config: RoomConfig, logic: GroupLogic) -> Self {
            let (tick_tx, tick_rx) = broadcast::channel(64);
            let (control, control_rx) = channel(config.control_capacity);
            let actor = RoomActor::new(
                config,
                (),
                Box::new(logic),
                tick_rx,
                control_rx,
                1,
                null_metrics_tx(),
            );
            Self {
                tick_tx,
                control,
                handle: tokio::spawn(actor.run()),
                t0: Instant::now(),
                next_tick: 0,
            }
        }

        fn tick(&mut self) {
            self.next_tick += 1;
            let at = self.t0 + Duration::from_secs_f64(self.next_tick as f64 / 30.0);
            self.tick_tx
                .send(TickInfo {
                    tick: self.next_tick,
                    at,
                })
                .expect("room subscriber alive");
        }

        async fn join(
            &mut self,
            conn: ConnectionId,
        ) -> (EntityId, mpsc::Receiver<FrameBatch>) {
            let (out_tx, out_rx) = mpsc::channel::<FrameBatch>(64);
            let (reply_tx, reply_rx) =
                oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
            self.control
                .send(RoomControl::Join {
                    conn,
                    out: out_tx,
                    reply: reply_tx,
                })
                .await
                .expect("control alive");
            self.tick();
            let (entity, _actions) = tokio::time::timeout(Duration::from_secs(2), reply_rx)
                .await
                .expect("timed out waiting for join reply")
                .expect("join reply dropped")
                .expect("join accepted (room not full)");
            (entity, out_rx)
        }

        async fn leave(&mut self, conn: ConnectionId, entity: EntityId) {
            self.control
                .send(RoomControl::Leave { conn, entity })
                .await
                .expect("control alive");
            self.tick();
        }

        /// Feed one tick and wait until the room has stepped it (step
        /// counter from the logic).
        async fn step(&mut self) {
            self.tick();
        }

        async fn shutdown(mut self) {
            self.control
                .send(RoomControl::Shutdown)
                .await
                .expect("control alive");
            self.tick();
            tokio::time::timeout(Duration::from_secs(2), &mut self.handle)
                .await
                .expect("room did not shut down")
                .expect("room task panicked");
        }
    }

    /// Wait until the logic reports `step_no` steps, then give the room a
    /// moment to finish the in-flight step's fan-out (the step counter is
    /// emitted in phase 3, fan-out is phase 4; the sleep is generous —
    /// fan-out is microsecond-scale).
    async fn wait_steps(steps: &mut mpsc::Receiver<u64>, n: u64) {
        while let Some(s) = tokio::time::timeout(Duration::from_secs(2), steps.recv())
            .await
            .expect("steps closed")
        {
            if s == n {
                tokio::time::sleep(Duration::from_millis(50)).await;
                return;
            }
        }
        panic!("steps channel closed before step {n}");
    }

    fn batch_frames(batch: &[FrameBody]) -> Vec<(u16, Vec<u8>)> {
        batch
            .iter()
            .map(|f| (f.op, f.payload.to_vec()))
            .collect()
    }

    #[tokio::test]
    async fn per_connection_groups_isolate_snapshots_and_private() {
        let (step_tx, mut steps) = mpsc::channel(64);
        let mut room = GLRoom::new(
            RoomConfig {
                id: RoomId(1),
                ..Default::default()
            }, // keep-alive 1 Hz at 30 Hz: never due in this test's window
            GroupLogic {
                conn_entity: HashMap::new(),
                next: 0,
                dirty: std::collections::HashSet::new(),
                step_no: 0,
                steps: step_tx,
            },
        );

        let (a_ent, mut a_rx) = room.join(ConnectionId(1)).await;
        let (b_ent, mut b_rx) = room.join(ConnectionId(2)).await;
        let (c_ent, mut c_rx) = room.join(ConnectionId(0x70)).await; // private target

        // Each join dirties exactly its own group (a per-connection
        // grouping: B joining changes nothing for A). So by step 3 each
        // connection's queue holds exactly its own single snapshot — and
        // never another connection's group content.
        wait_steps(&mut steps, 3).await;
        let a_all = drain_all(&mut a_rx).await;
        let b_all = drain_all(&mut b_rx).await;
        let c_all = drain_all(&mut c_rx).await;
        assert_eq!(a_all.len(), 1, "A emitted once (its own join)");
        assert_eq!(b_all.len(), 1, "B emitted once (its own join)");
        assert_eq!(c_all.len(), 1, "C emitted once (its own join)");
        assert_eq!(
            batch_frames(&a_all[0]),
            vec![(0x7010, a_ent.to_le_bytes().to_vec())],
            "A must see exactly its own group's snapshot"
        );
        assert_eq!(
            batch_frames(&b_all[0]),
            vec![(0x7010, b_ent.to_le_bytes().to_vec())],
            "B must see exactly its own group's snapshot"
        );
        assert_eq!(
            batch_frames(&c_all[0]),
            vec![
                (0x7010, c_ent.to_le_bytes().to_vec()),
                (0x7011, u32::MAX.to_le_bytes().to_vec())
            ],
            "the private frame goes only to the designated connection"
        );

        // C leaves: for a per-connection grouping the remaining groups are
        // unchanged, so nothing is re-emitted.
        room.leave(ConnectionId(0x70), c_ent).await;
        wait_steps(&mut steps, 4).await;
        assert!(drain_all(&mut a_rx).await.is_empty(), "A unchanged ⇒ no batch");
        assert!(drain_all(&mut b_rx).await.is_empty(), "B unchanged ⇒ no batch");

        // Nothing changed anymore: no snapshot is emitted at all.
        room.step().await;
        room.step().await;
        wait_steps(&mut steps, 6).await;
        assert!(
            drain_all(&mut a_rx).await.is_empty(),
            "no change (and no keep-alive due) ⇒ no batch"
        );
        assert!(
            drain_all(&mut b_rx).await.is_empty(),
            "no change ⇒ no batch"
        );

        room.shutdown().await;
    }

    /// Test logic for the "the world changes on every tick" scenario,
    /// with contract-conforming bookkeeping: each group remembers the
    /// world step *it* last emitted at, keyed by the group (the
    /// `RoomLogic::snapshot` contract's per-group requirement). A group
    /// whose content is the whole world must emit on every tick.
    struct FairLogic {
        last_world: u64,
        last_emitted: HashMap<ConnectionId, u64>,
        step_no: u64,
        steps: mpsc::Sender<u64>,
    }

    impl RoomLogic<()> for FairLogic {
        type GroupKey = ConnectionId;

        fn snapshot_op(&self) -> u16 {
            0x7020
        }
        fn private_op(&self) -> u16 {
            0x7021
        }

        fn group_of(&self, _w: &(), conn: ConnectionId) -> Self::GroupKey {
            conn
        }

        fn snapshot(
            &mut self,
            _w: &mut (),
            _c: &TickCtx,
            group: &Self::GroupKey,
            out: &mut bytes::BytesMut,
        ) -> bool {
            // "Unchanged" = the world step this group emitted at is the
            // current one. The map is keyed by `group` — per-group
            // bookkeeping, so one group's emission cannot make another
            // group's answer change in the same tick.
            if self.last_emitted.get(group).copied() == Some(self.last_world) {
                return false;
            }
            self.last_emitted.insert(*group, self.last_world);
            out.extend_from_slice(&self.last_world.to_le_bytes());
            true
        }

        fn on_join(&mut self, _w: &mut (), _conn: ConnectionId) -> EntityId {
            1
        }
        fn on_leave(&mut self, _w: &mut (), _conn: ConnectionId) {}
        fn ingest(&mut self, _w: &mut (), _c: &TickCtx, actions: &mut Vec<Action>) {
            actions.clear();
        }
        fn update(&mut self, _w: &mut (), _c: &TickCtx) {
            // The world changes on every tick (e.g. one entity moving).
            self.last_world += 1;
            self.step_no += 1;
            let _ = self.steps.try_send(self.step_no);
        }
    }

    #[tokio::test]
    async fn all_dirty_groups_emit_on_the_same_tick() {
        // The external-measurement scenario with contract-conforming
        // (per-group) bookkeeping: two per-connection groups whose
        // content is the whole world, and the world changes on every
        // tick. Every group must emit on every tick — the group visited
        // first by the room must not make the later groups see "no
        // change" (that is exactly what a ledger shared across groups
        // does; the `RoomLogic::snapshot` contract forbids it).
        let (step_tx, mut steps) = mpsc::channel(64);
        let (tick_tx, tick_rx) = broadcast::channel(64);
        let (control, control_rx) = channel(128);
        let actor = RoomActor::new(
            RoomConfig {
                id: RoomId(3),
                ..Default::default()
            }, // keep-alive 1 Hz at 30 Hz: never due in this test's window
            (),
            Box::new(FairLogic {
                last_world: 0,
                last_emitted: HashMap::new(),
                step_no: 0,
                steps: step_tx,
            }),
            tick_rx,
            control_rx,
            1,
            null_metrics_tx(),
        );
        let handle = tokio::spawn(actor.run());
        let t0 = Instant::now();
        let mut next_tick = 0;
        let mut tick = || {
            next_tick += 1;
            let at = t0 + Duration::from_secs_f64(next_tick as f64 / 30.0);
            tick_tx
                .send(TickInfo {
                    tick: next_tick,
                    at,
                })
                .expect("room subscriber alive");
        };

        // conn 1 joins (tick 1); the control is processed on the room's
        // next step, so the tick goes out before the reply is awaited.
        let (out1_tx, mut a_rx) = mpsc::channel::<FrameBatch>(64);
        let (reply1_tx, reply1_rx) =
            oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
        control
            .send(RoomControl::Join {
                conn: ConnectionId(1),
                out: out1_tx,
                reply: reply1_tx,
            })
            .await
            .expect("control alive");
        tick();
        tokio::time::timeout(Duration::from_secs(2), reply1_rx)
            .await
            .expect("join reply timeout")
            .expect("join reply dropped")
            .expect("join accepted (room not full)");

        // conn 2 joins (tick 2).
        let (out2_tx, mut b_rx) = mpsc::channel::<FrameBatch>(64);
        let (reply2_tx, reply2_rx) =
            oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
        control
            .send(RoomControl::Join {
                conn: ConnectionId(2),
                out: out2_tx,
                reply: reply2_tx,
            })
            .await
            .expect("control alive");
        tick();
        tokio::time::timeout(Duration::from_secs(2), reply2_rx)
            .await
            .expect("join reply timeout")
            .expect("join reply dropped")
            .expect("join accepted (room not full)");

        // 25-tick window: the world changes on every tick, so BOTH
        // groups are dirty on every tick.
        for _ in 0..25 {
            tick();
        }
        wait_steps(&mut steps, 27).await;

        let a_all = drain_all(&mut a_rx).await;
        let b_all = drain_all(&mut b_rx).await;
        // A: its join tick (world 1) + B's join tick (world 2) + all 25
        // window ticks. B: its join tick + all 25 window ticks.
        let seq = |batches: &Vec<Vec<FrameBody>>| {
            batches
                .iter()
                .map(|b| {
                    u64::from_le_bytes(
                        b[0]
                            .payload
                            .get(0..8)
                            .expect("8-byte payload")
                            .try_into()
                            .expect("8-byte payload"),
                    )
                })
                .collect::<Vec<u64>>()
        };
        let a_seq = seq(&a_all);
        let b_seq = seq(&b_all);
        assert_eq!(
            a_seq,
            (1..=27).collect::<Vec<_>>(),
            "A must emit on every tick the world changed"
        );
        assert_eq!(
            b_seq,
            (2..=27).collect::<Vec<_>>(),
            "B must emit on every tick the world changed (no starvation)"
        );

        control
            .send(RoomControl::Shutdown)
            .await
            .expect("control alive");
        tick();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("room did not shut down")
            .expect("room task panicked");
    }

    /// Batch-buffer reuse (the floor turn): the fan-out hands each tick's
    /// batch to the outbound channel with `mem::take` and, on a full
    /// channel, restores it through `TrySendError::into_inner`. This
    /// locks the recovery path: after a stretch in which the outbound
    /// channel stayed full (emissions dropped), the channel must hold
    /// exactly its capacity of intact batches — and, once space appears,
    /// the NEXT emission must arrive (no wedged connection, no lost
    /// buffer, nothing past capacity).
    #[tokio::test]
    async fn full_outbound_channel_drops_then_recovers() {
        // Wider steps pipe than the test's tick count: the room's
        // `try_send` on it is best-effort (a full pipe would silently
        // lose the late step numbers and starve `wait_steps`).
        let (step_tx, mut steps) = mpsc::channel(256);
        let (tick_tx, tick_rx) = broadcast::channel(64);
        let (control, control_rx) = channel(128);
        let actor = RoomActor::new(
            RoomConfig {
                id: RoomId(4),
                ..Default::default()
            },
            (),
            Box::new(FairLogic {
                last_world: 0,
                last_emitted: HashMap::new(),
                step_no: 0,
                steps: step_tx,
            }),
            tick_rx,
            control_rx,
            1,
            null_metrics_tx(),
        );
        let handle = tokio::spawn(actor.run());
        let t0 = Instant::now();
        let mut next_tick = 0;
        let mut tick = || {
            next_tick += 1;
            let at = t0 + Duration::from_secs_f64(next_tick as f64 / 30.0);
            tick_tx
                .send(TickInfo {
                    tick: next_tick,
                    at,
                })
                .expect("room subscriber alive");
        };

        // One connection, an outbound channel of capacity 64.
        let (out_tx, mut rx) = mpsc::channel::<FrameBatch>(64);
        let (reply_tx, reply_rx) =
            oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
        control
            .send(RoomControl::Join {
                conn: ConnectionId(1),
                out: out_tx,
                reply: reply_tx,
            })
            .await
            .expect("control alive");
        tick();
        tokio::time::timeout(Duration::from_secs(2), reply_rx)
            .await
            .expect("join reply timeout")
            .expect("join reply dropped")
            .expect("join accepted (room not full)");

        // The world changes on every tick ⇒ one emission per tick:
        // 69 more ticks ⇒ 70 emissions total against 64 slots. The
        // channel keeps the OLDEST 64 (new ones fail `try_send` and are
        // dropped — the documented one-snapshot-of-staleness cost).
        // Yield between sends: on a current-thread runtime the room task
        // cannot consume the broadcast while the test runs, and a full
        // broadcast buffer would overwrite the oldest ticks.
        for _ in 0..69 {
            tick();
            tokio::task::yield_now().await;
        }
        wait_steps(&mut steps, 70).await;

        let all = drain_all(&mut rx).await;
        assert_eq!(all.len(), 64, "the channel holds exactly its capacity");
        let seq: Vec<u64> = all
            .iter()
            .map(|b| {
                assert_eq!(b[0].op, 0x7020, "snapshot opcode");
                u64::from_le_bytes(
                    b[0]
                        .payload
                        .get(0..8)
                        .expect("8-byte payload")
                        .try_into()
                        .expect("8-byte payload"),
                )
            })
            .collect();
        assert_eq!(seq, (1..=64).collect::<Vec<_>>(), "intact, in order");

        // Space reappears: the next emission must arrive (the batch
        // buffer survived the full-channel stretch).
        tick();
        wait_steps(&mut steps, 71).await;
        let recovered = drain_all(&mut rx).await;
        assert_eq!(recovered.len(), 1, "the post-full emission arrives");
        assert_eq!(
            recovered[0][0].payload.as_ref(),
            71u64.to_le_bytes(),
            "and it is tick 71's snapshot"
        );

        control
            .send(RoomControl::Shutdown)
            .await
            .expect("control alive");
        tick();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("room did not shut down")
            .expect("room task panicked");
    }

    #[tokio::test]
    async fn unchanged_group_is_silent_until_keepalive() {
        // Keep-alive every 3 steps (10 Hz under a 30 Hz room).
        let (step_tx, mut steps) = mpsc::channel(64);
        let mut room = GLRoom::new(
            RoomConfig {
                id: RoomId(2),
                keepalive_hz: 10.0,
                ..Default::default()
            },
            GroupLogic {
                conn_entity: HashMap::new(),
                next: 0,
                dirty: std::collections::HashSet::new(),
                step_no: 0,
                steps: step_tx,
            },
        );

        let (ent, mut a_rx) = room.join(ConnectionId(1)).await;
        wait_steps(&mut steps, 1).await;
        // Step 1 (the join tick): the membership change shipped a snapshot.
        let first = next_batch_full(&mut a_rx).await;
        assert_eq!(
            batch_frames(&first),
            vec![(0x7010, ent.to_le_bytes().to_vec())]
        );

        // Steps 2..8: no change. The room stays silent — except on the
        // keep-alive steps (3 and 6), which re-send the cached snapshot.
        for _ in 0..7 {
            room.step().await;
        }
        wait_steps(&mut steps, 8).await;

        let mut got = vec![first];
        while let Ok(batch) = a_rx.try_recv() {
            got.push(batch);
        }
        assert_eq!(
            got.len(),
            3,
            "one emission (step 1) + two keep-alive re-sends (steps 3, 6), \
             nothing else"
        );
        for batch in &got {
            assert_eq!(
                batch_frames(batch),
                vec![(0x7010, ent.to_le_bytes().to_vec())],
                "keep-alive re-sends the cached snapshot bytes"
            );
        }

        room.shutdown().await;
    }

    // -----------------------------------------------------------------
    // F5: keep-alive rate above the room rate. The registry rejects such
    // a config at room creation; direct construction (library use) must
    // still never be silent: the constructor warns once naming both
    // rates, and the cadence clamps to every step (the observable
    // behavior: an unchanged group re-sends on *every* step).
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn keepalive_above_tick_warns_at_construction_and_clamps_to_every_step() {
        // Thread-local subscriber (NOT the process-global default: other
        // tests in this binary may run concurrently on other threads, and
        // the never-emitted test owns the global slot).
        let (warn_tx, mut warns) = mpsc::channel::<String>(64);
        let (tick_tx, tick_rx) = broadcast::channel(64);
        let (control, control_rx) = channel(64);
        let (step_tx, mut steps) = mpsc::channel(64);

        // Direct construction with keepalive_hz = 60 under a 30 Hz room —
        // the misconfiguration the registry would have rejected.
        let actor = tracing::subscriber::with_default(WarnCapture { tx: warn_tx.clone() }, || {
            RoomActor::new(
                RoomConfig {
                    id: RoomId(25),
                    keepalive_hz: 60.0,
                    ..Default::default()
                },
                (),
                Box::new(GroupLogic {
                    conn_entity: HashMap::new(),
                    next: 0,
                    dirty: std::collections::HashSet::new(),
                    step_no: 0,
                    steps: step_tx,
                }),
                tick_rx,
                control_rx,
                1,
                null_metrics_tx(),
            )
        });
        let handle = tokio::spawn(actor.run());
        let t0 = Instant::now();

        // The construction-time warn fired exactly once and names both
        // rates (synchronous: the warn! runs inside the constructor).
        let w = warns.try_recv().expect("misconfigured construction must warn");
        assert!(
            w.contains("keepalive_hz=60") && w.contains("tick_hz=30"),
            "warn must name both rates: {w}"
        );
        assert!(
            warns.try_recv().is_err(),
            "the construction warn must fire exactly once: {w}"
        );

        // Clamped behavior: the join's own emission, then EVERY step is a
        // keep-alive step (interval 1) re-sending the cached snapshot.
        let (out_tx, mut a_rx) = mpsc::channel::<FrameBatch>(64);
        let (reply_tx, reply_rx) =
            oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
        control
            .send(RoomControl::Join {
                conn: ConnectionId(26),
                out: out_tx,
                reply: reply_tx,
            })
            .await
            .expect("control alive");
        for n in 1..=6u64 {
            tick_tx
                .send(TickInfo {
                    tick: n,
                    at: t0 + Duration::from_secs_f64(n as f64 / 30.0),
                })
                .expect("room subscriber alive");
        }
        let _ = reply_rx
            .await
            .expect("join reply dropped")
            .expect("join accepted (room not full)");
        wait_steps(&mut steps, 6).await;

        let got = drain_all(&mut a_rx).await;
        assert_eq!(
            got.len(),
            6,
            "join emission + 5 keep-alive re-sends (one per step, no \
             silence at all): {got:?}"
        );
        for (i, batch) in got.iter().enumerate() {
            assert_eq!(
                batch_frames(batch),
                vec![(0x7010, 1u64.to_le_bytes().to_vec())],
                "step {i} must carry the cached snapshot (entity 1)"
            );
        }

        drop(tick_tx); // ticker closed → the room exits cleanly
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("room did not exit on closed ticker")
            .expect("room task panicked");

        // No false positives: legitimate ratios must not warn. keep-alive
        // == tick is exactly "one per step" (as configured); 1 Hz is the
        // default setup.
        for keepalive in [30.0, 1.0] {
            let (_tick2, tick_rx2) = broadcast::channel(8);
            let (_control2, control_rx2) = channel(8);
            let (step2_tx, _step2_rx) = mpsc::channel::<u64>(8);
            tracing::subscriber::with_default(WarnCapture { tx: warn_tx.clone() }, || {
                let _actor2 = RoomActor::new(
                    RoomConfig {
                        id: RoomId(27),
                        keepalive_hz: keepalive,
                        ..Default::default()
                    },
                    (),
                    Box::new(GroupLogic {
                        conn_entity: HashMap::new(),
                        next: 0,
                        dirty: std::collections::HashSet::new(),
                        step_no: 0,
                        steps: step2_tx,
                    }),
                    tick_rx2,
                    control_rx2,
                    1,
                    null_metrics_tx(),
                );
            });
        }
        assert!(
            warns.try_recv().is_err(),
            "keepalive_hz <= tick_hz must not warn"
        );
    }

    // -----------------------------------------------------------------
    // F4 diagnostic: a group that has members but has never emitted
    // (snapshot → false on its first tick, although a fresh group's first
    // tick is a membership change and must emit) must be warned about —
    // exactly once, naming the group. The diagnostic previously had no
    // test; a violating logic (e.g. the shared-ledger misuse the room
    // cannot distinguish from legitimate silence) stays invisible without
    // one.
    // -----------------------------------------------------------------

    /// Contract-violating logic: `snapshot` returns `false` on *every*
    /// tick, including a fresh group's first tick.
    struct SilentLogic;

    impl RoomLogic<()> for SilentLogic {
        type GroupKey = ConnectionId;

        fn snapshot_op(&self) -> u16 {
            0x7030
        }
        fn private_op(&self) -> u16 {
            0x7031
        }

        fn group_of(&self, _w: &(), conn: ConnectionId) -> Self::GroupKey {
            conn
        }

        fn snapshot(
            &mut self,
            _w: &mut (),
            _c: &TickCtx,
            _g: &Self::GroupKey,
            _o: &mut bytes::BytesMut,
        ) -> bool {
            false // even the first tick: a contract violation
        }

        fn on_join(&mut self, _w: &mut (), _c: ConnectionId) -> EntityId {
            1
        }
        fn on_leave(&mut self, _w: &mut (), _c: ConnectionId) {}
        fn ingest(&mut self, _w: &mut (), _c: &TickCtx, actions: &mut Vec<Action>) {
            actions.clear();
        }
        fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
    }

    /// Lock-free WARN-capturing subscriber: events are pushed over an mpsc
    /// channel (never blocking); no shared state to protect. The `warn!`
    /// macro carries its text in a `message` field, so the field list is
    /// the log line.
    struct WarnCapture {
        tx: mpsc::Sender<String>,
    }

    impl tracing::Subscriber for WarnCapture {
        fn enabled(&self, meta: &tracing::Metadata<'_>) -> bool {
            *meta.level() == tracing::Level::WARN
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            // The room code creates no spans; placeholder never used.
            tracing::span::Id::from_non_zero_u64(std::num::NonZeroU64::MIN)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut fields: Vec<(String, String)> = Vec::new();
            event.record(&mut WarnFieldSink {
                fields: &mut fields,
            });
            let line = fields
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ");
            let _ = self.tx.try_send(line);
        }
    }

    struct WarnFieldSink<'a> {
        fields: &'a mut Vec<(String, String)>,
    }

    impl tracing::field::Visit for WarnFieldSink<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.fields
                .push((field.name().to_string(), format!("{value:?}")));
        }
    }

    #[tokio::test]
    async fn never_emitted_group_warns_once_naming_the_group() {
        // Only this test sets the process-global default; the other tests
        // in this binary neither set it nor assert on logging.
        let (warn_tx, mut warns) = mpsc::channel::<String>(64);
        tracing::subscriber::set_global_default(WarnCapture { tx: warn_tx })
            .expect("only this test sets the global default");

        let (tick_tx, tick_rx) = broadcast::channel(64);
        let (control, control_rx) = channel(16);
        let actor = RoomActor::new(
            RoomConfig {
                id: RoomId(9),
                ..Default::default()
            },
            (),
            Box::new(SilentLogic),
            tick_rx,
            control_rx,
            1,
            null_metrics_tx(),
        );
        let handle = tokio::spawn(actor.run());
        let t0 = Instant::now();

        // Tick 1: the join is processed, the group is created (Vacant —
        // no check yet) and its first `snapshot()` returns `false`.
        let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(8);
        let (reply_tx, reply_rx) =
            oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
        control
            .send(RoomControl::Join {
                conn: ConnectionId(42),
                out: out_tx,
                reply: reply_tx,
            })
            .await
            .expect("control alive");
        tick_tx
            .send(TickInfo {
                tick: 1,
                at: t0 + Duration::from_secs_f64(1.0 / 30.0),
            })
            .expect("room subscriber alive");
        let _ = reply_rx
            .await
            .expect("join reply dropped")
            .expect("join accepted (room not full)");
        tokio::time::sleep(Duration::from_millis(20)).await;
        // No diagnostic for THIS group on its own first tick (the check
        // only sees a group that existed on the previous tick). Other
        // tests in this binary share the global default and may emit
        // their own warns (e.g. RecLogic, which never emits) — only lines
        // naming our group are ours.
        while let Ok(line) = warns.try_recv() {
            assert!(
                !line.contains("ConnectionId(42)"),
                "no diagnostic on the group's own first tick: {line}"
            );
        }

        // Ticks 2..=4: the group is Occupied with `last = None` — the
        // diagnostic must fire on tick 2 and (flag) not repeat.
        for n in 2u64..=4 {
            tick_tx
                .send(TickInfo {
                    tick: n,
                    at: t0 + Duration::from_secs_f64(n as f64 / 30.0),
                })
                .expect("room subscriber alive");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        drop(tick_tx); // ticker closed → the room exits cleanly
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("room did not exit on closed ticker")
            .expect("room task panicked");

        let mut mine = Vec::new();
        while let Ok(line) = warns.try_recv() {
            if line.contains("ConnectionId(42)") {
                mine.push(line);
            }
        }
        assert_eq!(mine.len(), 1, "warn fires exactly once: {mine:?}");
        assert!(
            mine[0].contains("group_key=ConnectionId(42)"),
            "warn must name the group: {}",
            mine[0]
        );
        assert!(
            mine[0].contains("members=1"),
            "warn must report the member count: {}",
            mine[0]
        );
    }

    /// Receive one batch with a timeout (the positive-side barrier: the
    /// room flushed this connection, so its step's fan-out has reached it).
    async fn next_batch_full(rx: &mut mpsc::Receiver<FrameBatch>) -> Vec<FrameBody> {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for a batch")
            .expect("out channel closed")
    }

    /// Take everything currently queued (after `wait_steps`, the fan-out of
    /// every reported step has completed).
    async fn drain_all(rx: &mut mpsc::Receiver<FrameBatch>) -> Vec<Vec<FrameBody>> {
        let mut out = Vec::new();
        while let Ok(batch) = rx.try_recv() {
            out.push(batch);
        }
        out
    }

    // ── capacity + fairness guardrails (behaviour lock) ──────────────

    /// Capacity guardrail: at `max_players` the next join is rejected with
    /// `CoreError::RoomFull` — no entity, no action channel, no room
    /// state — while the room (and its members) keeps working.
    #[tokio::test]
    async fn join_rejected_when_room_is_full() {
        let (dt_tx, _dts) = mpsc::channel(16);
        let (op_tx, mut ops) = mpsc::channel(16);
        let mut h = Harness::new(
            1,
            RoomConfig {
                id: RoomId(1),
                max_players: Some(2),
                ..Default::default()
            },
            RecLogic {
                dts: dt_tx,
                ops: op_tx,
            },
        );
        let period = Duration::from_secs_f64(1.0 / 30.0);

        // Two joins fill the room.
        let (_e1, _a1) = h.join(ConnectionId(1), 1).await;
        let (_e2, _a2) = h.join(ConnectionId(2), 1).await;

        // The third join is structurally rejected (the reply carries the
        // error; nothing is recorded in the room).
        let (out3_tx, _out3_rx) = mpsc::channel::<FrameBatch>(8);
        let (reply3_tx, reply3_rx) =
            oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
        h.control
            .send(RoomControl::Join {
                conn: ConnectionId(3),
                out: out3_tx,
                reply: reply3_tx,
            })
            .await
            .expect("control alive");
        for _ in 0..2 {
            h.tick(period);
        }
        match tokio::time::timeout(Duration::from_secs(2), reply3_rx)
            .await
            .expect("timed out waiting for the rejection")
            .expect("reply dropped")
        {
            Err(CoreError::RoomFull(id)) => {
                assert_eq!(id, 1, "the error names the rejecting room")
            }
            other => panic!("expected RoomFull, got {other:?}"),
        }

        // Existing members are unaffected: the surviving member's action
        // still reaches the ingest on the next step.
        _a1
            .send(Action {
                conn: ConnectionId(1),
                op: 0x1500,
                payload: bytes::Bytes::new(),
            })
            .await
            .expect("member's action channel alive");
        h.tick(period);
        let op = tokio::time::timeout(Duration::from_secs(2), ops.recv())
            .await
            .expect("timed out waiting for the member op")
            .expect("ops closed");
        assert_eq!(op, 0x1500, "the surviving member's action is still ingested");
        h.shutdown().await;
    }

    /// Fairness guardrail: a connection that floods its own action
    /// channel can no longer evict ANYONE ELSE's actions. The READ phase
    /// is a bounded pull (per-connection budget + room pull budget), not
    /// a merged list with an oldest-drop: the victim's one action per tick
    /// is ingested on every tick, and the flooder's excess stays in its
    /// OWN channel (deferred; the room drops nothing).
    ///
    /// Under the old semantics (merged list, `drain(..over)` = oldest) the
    /// merged list is built in `conns` iteration order and the overflow
    /// drops its HEAD: with one flooded connection and one quiet one, the
    /// quiet connection's actions sit in a small contiguous block of the
    /// list, and the flooder's backlog determines which block overflows —
    /// i.e. a single flooder could evict the other connection's actions
    /// (which block was dropped depended on the hash order, so even the
    /// victim was arbitrary). The per-connection pull budget removes the
    /// interaction entirely: every connection's ingest is bounded by its
    /// own budget, whoever it is.
    #[tokio::test]
    async fn flooder_cannot_evict_other_connections_actions() {
        let (dt_tx, _dts) = mpsc::channel(64);
        // Wide: the room ingests 188 ops (20 victim + 168 flood) and the
        // logic forwards each via try_send — the observation channel must
        // not be the thing that overflows in this test.
        let (op_tx, mut ops) = mpsc::channel(512);
        let mut h = Harness::new(
            1,
            RoomConfig {
                id: RoomId(1),
                // The fairness probe: a tight pull budget and a tight
                // per-connection budget (the old code had neither; the
                // merged list grew with the flooder's backlog).
                max_pending_actions: 16,
                max_actions_per_conn_per_tick: 8,
                ..Default::default()
            },
            RecLogic {
                dts: dt_tx,
                ops: op_tx,
            },
        );
        let period = Duration::from_secs_f64(1.0 / 30.0);

        // The quiet connection ("victim") and the flooded connection
        // ("flooder"); which block of the old merged list overflowed
        // depended on the HashMap iteration order, so neither role is
        // special — the new per-connection budgets make it a non-issue.
        let (_ev, victim) = h.join(ConnectionId(1), 1).await;
        let (_ef, flood) = h.join(ConnectionId(2), 1).await;

        // The flooder fills its own action channel to capacity (256):
        // the flood backlog the room would have merged (and overflowed)
        // under the old READ.
        let mut stuffed = 0usize;
        while let Ok(()) = flood.try_send(Action {
            conn: ConnectionId(2),
            op: 0x3000,
            payload: bytes::Bytes::new(),
        }) {
            stuffed += 1;
        }
        assert_eq!(stuffed, 256, "the flood backlog is the channel capacity");

        // 20 ticks: the victim sends exactly one action per tick.
        for t in 0..20u16 {
            victim
                .try_send(Action {
                    conn: ConnectionId(1),
                    op: 0x2000 + t,
                    payload: bytes::Bytes::new(),
                })
                .expect("victim channel never full (one op per tick)");
            h.tick(period);
        }
        // One more tick so the last queued op is pulled and ingested.
        h.tick(period);

        // Collect everything ingested (the ticks are fire-and-forget, so
        // wait until the full expected volume has landed: 20 victim ops +
        // 8 floods/tick × 21 ticks = 168). Ingest ORDER between the two
        // connections is HashMap-driven and not asserted; OWNERSHIP is
        // what the guardrail guarantees.
        let mut victim_ops = 0u32;
        let mut flood_ops = 0u32;
        let mut total = 0u32;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while total < 188 && std::time::Instant::now() < deadline {
            match ops.try_recv() {
                Ok(op) => {
                    total += 1;
                    if (0x2000..0x2014).contains(&op) {
                        victim_ops += 1;
                    } else if op == 0x3000 {
                        flood_ops += 1;
                    }
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
        assert_eq!(
            total,
            188,
            "the room ingested the full expected volume (20 + 168)"
        );
        assert_eq!(
            victim_ops,
            20,
            "EVERY victim op was ingested — the flood evicted none of them"
        );
        assert_eq!(
            flood_ops,
            168,
            "the room pulled exactly 8 flood ops per tick (its per-conn budget)"
        );
        // The flooder's excess was DEFERRED in its own channel: the room
        // pulled 8/tick × 21 ticks = 168 (the ingested volume above), so
        // 88 of the original 256 are still queued — the room dropped
        // nothing. Observe it by filling the free slots: exactly 168
        // sends fit (= the amount pulled), the 169th hits Full.
        let mut free = 0usize;
        // (Full ends the loop: the backlog is exactly 256 − free.)
        while let Ok(()) = flood.try_send(Action {
            conn: ConnectionId(2),
            op: 0x3001,
            payload: bytes::Bytes::new(),
        }) {
            free += 1;
        }
        assert_eq!(
            free,
            8 * 21,
            "exactly the per-tick pull (8/tick × 21 ticks) freed slots; \
             the rest is still deferred in the flooder's own channel"
        );
        h.shutdown().await;
    }
}
