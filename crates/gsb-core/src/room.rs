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
//!  Phase 2  │  CONVERT:   actions → component writes (GameLogic)
//!  Phase 3  │  SYSTEMS:   run the ordered game systems   (GameLogic)
//!  Phase 4  │  BROADCAST: one snapshot per group (GameLogic::snapshot)
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
//! ([`GameLogic::group_of`]): `()` means "one group per room" (the demo),
//! `ConnectionId` means "one snapshot per connection". Each tick the room
//! encodes each group's **entire** snapshot **once**, `freeze()`s it, and
//! fans the resulting `Bytes` out to the group's members by reference
//! (Arc refcount — the payload is never copied). Membership (join/leave)
//! is expressed by presence in the snapshot: there are no spawn/remove
//! events. The game logic decides "nothing changed for this group"
//! (`GameLogic::snapshot` returning `false`, including membership
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
//! the group key an opaque `G`; all game behaviour is delegated to the
//! logic traits — [`GameLogic`] (the shared supertrait, one source for
//! what used to be a ~17-method duplicate on both actors) narrowed here
//! by [`RoomLogic`] with the room-exclusive request/result seams; the
//! sharded sibling is [`ShardLogic`](crate::shard::ShardLogic) — see
//! `docs/TRAIT-ARCHITECTURE.md`.
//!
//! **Metrics:** the room's counters live in the room's own local state
//! ([`RoomCounters`]) and are flushed once per step over the *bounded*
//! metrics channel with a synchronous `try_send` (a full channel drops
//! the sample and counts it — harmless, the counters are cumulative);
//! the room's only `await` stays `tick_rx.recv()` and the tick body
//! stays fully synchronous (see [`crate::metrics`] for the design and
//! the constraint rationale).

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;
use std::time::{Duration, Instant};

use prost::Message;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, warn};

use crate::channel::{FrameBatch, Inbox, Mailbox};
use crate::error::CoreError;
use crate::id::{ConnectionId, EntityId, PlayerId, RoomId};
use crate::metrics::{MetricsEvent, RoomSample, FINE_HIST_BINS, HIST_BINS};
use crate::ticker::TickInfo;

/// A client action forwarded by the connection actor. The payload is still
/// encoded; the game crate decodes it against its own message types.
///
/// Two routing keys, two jobs (Faz 2, `docs/TRAIT-ARCHITECTURE.md` §5):
/// `conn` is the transport-session key (the wire contract — unchanged);
/// `player` is the stable player identity the ROOM resolves for ingest.
/// A connection actor cannot know `player` (the identity is minted inside
/// the game logic), so it fills the placeholder [`PlayerId(0)`]; the room
/// stamps the authoritative value from its binding table at READ→CONVERT
/// (see the phase comment). An action whose `conn` is not bound drops
/// there — an old session's stray frame after a resume has no binding
/// row left and can never reach the world.
#[derive(Debug)]
pub struct Action {
    pub conn: ConnectionId,
    /// The room-resolved player this action belongs to (placeholder `0`
    /// at construction; stamped by the room/shard before ingest).
    pub player: PlayerId,
    pub op: u16,
    pub payload: bytes::Bytes,
}

/// What a successful join hands back to the room actor (Faz 2): the
/// player identity the LOGIC minted plus the entity (`EntityId`) it
/// created/restored — the same value `on_join` always returned and the
/// reply carries. The core needs both: `player` keys every internal
/// table, `entity` stays the stale-leave/detach guard and wire id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Admission {
    pub player: PlayerId,
    pub entity: EntityId,
}

/// Static configuration for a room.
///
/// `PartialEq` (not just `Debug + Clone`): the control plane's
/// idempotent create compares the requested config with the existing
/// room's config (identical request = no-op success, different request
/// = conflict — see `RegistryMsg::CreateRoom`).
#[derive(Debug, Clone, PartialEq)]
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
    /// Per-connection cap on PENDING external (delegated) requests — the
    /// request/RPC pattern (see `crate::rpc`). A request arriving when
    /// the connection already has this many in flight is answered with a
    /// normal rejection in the same tick. Room-local requests (answered
    /// in the same tick) never occupy a pending slot. The arrival *rate*
    /// is separately bounded by the READ phase's per-connection pull
    /// budget (a request is an action), so the cap bounds state, not
    /// rate.
    ///
    /// Default 4: a well-behaved client has at most 1–2 requests in
    /// flight (one per logical action; a two-action burst in one frame
    /// is the realistic maximum) plus 1–2 of retry headroom (a client
    /// that suspects a lost answer re-requests under a FRESH id, which
    /// occupies a new slot while the first is still pending). 4 = 2 + 2:
    /// 2 would reject a burst+retry; larger values only let one
    /// misbehaving connection hoard more of the room's budget — the
    /// fairness knob is the exhaustion threshold (room cap / this cap),
    /// see `max_pending_requests` and §6.1 of
    /// `docs/RPC-CONTROL-PLANE.md`.
    pub max_pending_requests_per_conn: usize,
    /// Room-wide cap on pending external requests (all connections). The
    /// per-connection cap alone would still let N connections × the cap
    /// of pending work accumulate in the room's pending set and its
    /// worker tasks; the room cap makes the room's delegation budget
    /// explicit and bounded independently of its population.
    ///
    /// Sizing rule: the in-flight count is driven by **request rate ×
    /// backend latency** (Little's law: L = λ·T) — NOT by the room's
    /// population. Example: 10 000 players × 1 request/min × 1 s of
    /// backend latency ≈ 167 in flight; ×~12 headroom → the default
    /// 2000. Full-cost at saturation: 2000 worker tasks ≈ 1–4 MB,
    /// 2000 timers, worst-case wake spread ≈ 2–4 ms against the 33 ms
    /// step budget.
    ///
    /// Exhaustion threshold (derived): room cap / per-connection cap =
    /// 2000 / 4 = **500 connections** — the room cap can bind only if
    /// ≥500 connections are simultaneously at their full per-connection
    /// quota (5 % of a 10 000-member room); below it, no subset of
    /// connections can starve the rest of the room of pending budget.
    /// The fairness property lives in that number (see §6.1 of
    /// `docs/RPC-CONTROL-PLANE.md`).
    ///
    /// Boundary: this caps the request COUNT (the room's own state
    /// budget), NOT backend concurrency — 2000 in flight means up to
    /// 2000 concurrent backend calls. Capacity-limited services need
    /// call-site throttling (your own queue/pool): the base cannot know
    /// the service's capacity.
    pub max_pending_requests: usize,
    /// The request timeout (see `crate::rpc`): a pending external request
    /// whose deadline passes is swept on the next tick and answered with
    /// a timeout reply (the client never hangs on an accepted request).
    /// The worker task carries the same deadline internally as a resource
    /// guard (a never-resolving future cannot outlive it).
    ///
    /// Default 5 s: comfortably above the latency of a healthy in-process
    /// or networked service (the hook's job is a service round trip, not
    /// game logic), short enough that a stuck dependency degrades a
    /// client's request within one heartbeat cycle.
    pub request_timeout: Duration,
    /// Whether the registry should REBUILD the room from the same factory +
    /// config when its actor task dies unexpectedly (a panic in the game
    /// logic kills the room's task; without the death watch the registry's
    /// table kept answering `Running` forever and joins disappeared into a
    /// dead mailbox — see `RegistryMsg::RoomDied`).
    ///
    /// Contract of a rebirth: the room comes back **EMPTY**. The old
    /// members were notified (`ConnIn::RoomGone`) and may rejoin; no world
    /// state survives — the world lived inside the dead task, and no
    /// cross-task state is shared by design. Default `false`: a death is
    /// final and the operator decides what to do.
    ///
    /// `PartialEq` participation (the idempotent-create comparison): the
    /// flag is an ordinary field, so it compares like every other field —
    /// a retry carries it identically by construction (a retry IS the same
    /// request), and flipping it between retries is a different spec =
    /// `RoomConflict`, exactly as for any other field change.
    pub restart_on_panic: bool,
    /// The room's CLASS (§8 of `docs/RECONNECT.md`), not another restart
    /// knob: `true` = a persistent world piece (an MMO map) that must not
    /// die with a panic; `false` = an ephemeral match room whose end is
    /// final.
    ///
    /// What the class buys, enforced by the REGISTRY:
    /// - supervision rebuilds a dead persistent room EVEN WHEN
    ///   [`RoomConfig::restart_on_panic`] is `false` ("a continuous room
    ///   does not stay dead from a panic" is a class guarantee, not a
    ///   tunable preference). The rebuild comes back EMPTY — without a
    ///   persistence layer an MMO map opens fresh, which is the documented
    ///   §8 limit of this seam, not its goal;
    /// - `DestroyRoom` RETIRES the id in either class (later joins/resumes
    ///   answer ERROR 12 and no create may resurrect the id in-process);
    ///   for an ephemeral room the same retirement means "the match ended,
    ///   never retry" — the flag changes supervision, not the destroy
    ///   path.
    ///
    /// Default `false`; ordinary `PartialEq` participant like every other
    /// field (a retry carries its class identically).
    pub persistent: bool,
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
            max_pending_requests_per_conn: 4,
            max_pending_requests: 2000,
            request_timeout: Duration::from_secs(5),
            restart_on_panic: false,
            persistent: false,
        }
    }
}

/// The fallback tick period [`RoomConfig::period`] reports for a config
/// whose `tick_hz` has no usable period (`<= 0`, NaN/±inf, or so high the
/// reciprocal truncates to zero). One second: slow enough that every
/// derived quantity stays finite and panic-free (the µs budget for the
/// step histogram, `period * max_catchup` as the dt cap — even ×u32::MAX
/// fits `Duration`), fast enough that a misconfigured room still steps
/// (and its metrics show a 1 Hz rate instead of a frozen counter).
const FALLBACK_TICK_PERIOD: Duration = Duration::from_secs(1);

impl RoomConfig {
    /// The tick period (`1 / tick_hz`).
    ///
    /// Total by construction: it never panics, mirroring the totality
    /// guard of [`crate::ticker::Ticker::spawn`] (finite, `> 0`,
    /// representable non-zero duration — otherwise the fallback above).
    ///
    /// Why the guard exists even though the registry path rejects bad
    /// rates before any actor exists (the CreateRoom handler refuses a
    /// `tick_hz` whose step divisor rounds below 1 or misses the global
    /// rate): `RoomConfig` is public API and direct hand-built configs
    /// bypass that validation entirely — tests, embedders, factories.
    /// This method previously reached `Duration::from_secs_f64`, which
    /// panics on exactly those inputs, killing an actor task at
    /// construction (and, under `restart_on_panic`, respawning straight
    /// into the same panic forever). A degraded-but-alive room beats a
    /// panic loop: the fallback keeps every derived quantity well-defined.
    pub fn period(&self) -> Duration {
        let hz = self.tick_hz;
        if hz.is_finite() && hz > 0.0 {
            Duration::try_from_secs_f64(1.0 / hz)
                .ok()
                .filter(|period| !period.is_zero())
                .unwrap_or(FALLBACK_TICK_PERIOD)
        } else {
            FALLBACK_TICK_PERIOD
        }
    }
}

/// What a room's logic wants to happen to a disconnected player's entity
/// (the detach policy — `docs/RECONNECT.md` §3: *transport death is a
/// fact; what happens to the entity is a game rule*).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detach {
    /// The old behavior: despawn the entity right away (lobby, chat).
    /// This is the trait method's default, so every pre-reconnect logic
    /// keeps byte-for-byte its old semantics without recompiling anything.
    Despawn,
    /// The entity lives on, parked. `grace = None` → only
    /// [`GameLogic::may_release`] ends the hold (combat-held);
    /// `Some(d)` → the hold ends at the latest `d` after the disconnect
    /// (the ceiling that makes a harassed lock impossible to extend
    /// forever). What happens at the end: the player returns first
    /// (resume) or `to` runs.
    Hold { grace: Option<Duration>, to: ExpireTo },
}

/// Where a parked entity goes when its hold ends without a resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpireTo {
    /// Despawn (the slot is released; the identity may fresh-join later).
    Despawn,
    /// The same entity keeps playing, driven by the game's own input
    /// synthesis (`ExpireTo::AiHandover` — §9). The wire id is unchanged:
    /// handover is a behavior change, not an identity change. Tur A marks
    /// the connection `bot_fed` and keeps everything alive; the demo bot
    /// that synthesizes the input is Tur B's seam.
    AiHandover,
}

/// Outcome of the park-ledger lookup behind a resume attempt
/// (`docs/RECONNECT.md` §5/§7). The ledger lives in the logic (§4); this
/// is the one question the core asks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeFound {
    /// The identity is parked: `PlayerId` is the stable identity of the
    /// parked session's row (Faz 2 — the ledger rides the player state,
    /// §14.2, so it answers with the id that keys the core's own tables;
    /// the parked row is found by ONE lookup instead of a scan).
    Held(PlayerId),
    /// The identity WAS parked but the hold has ended (expired, consumed
    /// by an earlier resume, superseded). The resume mechanism rejects
    /// (counted as `resume_rejected_stale`) and — per the transparent
    /// fallback of §5, which produces no client-visible error — the join
    /// proceeds as an ordinary fresh join.
    Ended,
    /// Never parked (the default): transparent fresh-join fallback,
    /// indistinguishable from an ordinary join. This is what makes the
    /// grace TOCTOU self-resolving (§11): whichever of expire/resume
    /// lands first, both branches are valid.
    Never,
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
    /// The connection's transport died: hand the entity's fate to the
    /// room policy (`GameLogic::on_disconnect` — `docs/RECONNECT.md` §3).
    /// This is what `ConnClosed` routes instead of the despawn-causing
    /// `Leave` it used to send: the registry never decides the policy,
    /// it only reports the fact.
    ///
    /// `identity` is the resume key (`ValidatedTicket.player`, or
    /// `Auth.name` on the local-auth path — demo-only there): the logic
    /// records it in its park ledger when it answers
    /// [`Detach::Hold`](crate::room::Detach::Hold).
    Detach {
        conn: ConnectionId,
        entity: EntityId,
        identity: String,
    },
    /// An identified join whose ledger may hold this identity: the
    /// implicit resume attempt (§14.3 — there is NO new wire opcode; a
    /// ticket-pinned connection's ordinary `JOIN_ROOM_REQ` IS the resume
    /// attempt). If the ledger holds the identity, the room swaps the
    /// channel halves onto the parked entity and replies with the SAME
    /// entity id; otherwise it processes an ordinary fresh join and
    /// replies identically (transparent fallback, §5) — the client
    /// cannot tell the two apart from the reply alone.
    Resume {
        conn: ConnectionId,
        /// The dispatcher-minted join epoch of the NEW session. Guard
        /// (§7): a resume whose epoch is not newer than the parked
        /// session's stamp is a delayed duplicate/replay — rejected with
        /// [`CoreError::ResumeStale`] so it can never double-bind or
        /// fresh-join a second entity for one identity. `0` (= "no
        /// epoch", hand-built calls) disables the guard; the ledger
        /// consumption stays exactly-once regardless, because the actor
        /// is single-threaded.
        epoch: u64,
        identity: String,
        out: mpsc::Sender<FrameBatch>,
        reply: oneshot::Sender<Result<(EntityId, Mailbox<Action>), CoreError>>,
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

/// Game-side behaviour shared by every actor shape — the single source of
/// the contract that used to be duplicated between the room and the shard
/// (~17 near-identical methods; see `docs/TRAIT-ARCHITECTURE.md` §3): the
/// tick seam ([`Self::ingest`] / [`Self::update`] / [`Self::snapshot`]),
/// the snapshot-group partitioning, the membership hooks, and the
/// reconnect surface (detach/resume). Two thin subtraits add only their
/// actor's exclusive hooks: [`RoomLogic`] the room's request/result seams,
/// [`ShardLogic`](crate::shard::ShardLogic) the migration/topology hooks.
///
/// Implemented by the game crate; the core never inspects the world `W`
/// or the group key `GroupKey`.
pub trait GameLogic<W>: Send {
    /// Opaque key partitioning the room's connections into snapshot groups.
    /// `()` = one per room (everyone sees the whole world);
    /// `ConnectionId` = one snapshot per connection; anything else (e.g. a
    /// zone id) is a legitimate future grouping. `Debug` so the room's
    /// group diagnostics can name a misbehaving group.
    type GroupKey: Eq + Hash + Clone + Debug;

    /// What an entity carries across a SHARD boundary (the
    /// visibility-strip payload inside [`shard::BorderRecord`]). Lives on
    /// THIS supertrait because [`Self::snapshot`] is the single encode
    /// seam both actors share — the borrowed set reaches the encoder
    /// typed, so the payload type must be visible here too.
    ///
    /// Ownership follows the architecture rule (`docs/TRAIT-ARCHITECTURE.md`):
    /// the wire identity (`wire`) is core-managed, but WHAT travels beside
    /// it — position only, or velocity/facing/hp for combat/prediction
    /// games — is the game's decision, exactly like the migration
    /// [`shard::ShardLogic::State`]. Any logic that never runs sharded
    /// picks `()` and never sees a record. Serialization at
    /// process-boundary links stays future work owned by the logic
    /// (`docs/DISTRIBUTED.md` §4b). `PartialEq` is load-bearing on the
    /// sharded path: it IS the delta upsert test.
    type Strip: Debug + Clone + PartialEq + Send + 'static;

    /// Opcode under which the room ships group snapshots.
    fn snapshot_op(&self) -> u16;
    /// Opcode under which the room ships the per-connection private frame
    /// produced by [`Self::private`].
    fn private_op(&self) -> u16;

    /// Which snapshot group the player belongs to. Re-evaluated every
    /// tick: a group may depend on the world (e.g. the zone an entity is
    /// in). Keyed by the stable [`PlayerId`] (Faz 2) — a resumed session
    /// keeps its group without any re-keying.
    fn group_of(&self, world: &W, player: PlayerId) -> Self::GroupKey;

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
    ///
    /// `borrowed` carries the neighboring shards' boundary records on the
    /// SHARDED execution path (the shard actor folds its latest border
    /// exchange in — see `crate::shard`); a single-room actor passes an
    /// empty slice. The parameter lives here — on the shared supertrait,
    /// not on the shard subtrait — so ONE method serves both actors and
    /// the fan-out machinery stays textually identical: a plain room's
    /// "no change" test simply never sees borrowed content. Each record's
    /// payload is the game's own [`Self::Strip`] type, so encoding it is
    /// fully in the logic's hands.
    fn snapshot(
        &mut self,
        world: &mut W,
        ctx: &TickCtx,
        group: &Self::GroupKey,
        borrowed: &[crate::shard::BorderRecord<Self::Strip>],
        out: &mut bytes::BytesMut,
    ) -> bool;

    /// Encode a per-connection private frame (delivered only to this
    /// player's current session, alongside the group snapshot).
    ///
    /// The player's group is passed in: the room re-evaluates it every
    /// tick (see [`Self::group_of`]) and hands the current value over, so a
    /// logic that needs "which group is this player in" must not
    /// re-derive it — each re-derivation is extra table lookups per
    /// player per tick (measured: part of the idle floor).
    ///
    /// `responses` is this tick's list of RPC answers owed to `conn`
    /// (empty = none; see [`RoomLogic::handle_request`] and
    /// `crate::rpc`):
    /// same-tick local answers and, on later ticks, the deferred answers
    /// of external requests that completed (or timed out) since the
    /// request was processed. The logic encodes them into the private
    /// frame (`Private.responses` in the demo protocol) — the frame is
    /// emitted whenever there is ANY content (ack, one-shot full, or
    /// responses).
    ///
    /// Default: none.
    fn private(
        &mut self,
        _world: &mut W,
        _player: PlayerId,
        _group: &Self::GroupKey,
        _responses: &[crate::rpc::RpcReply],
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

    /// A player entered the room: mint (or restore) its stable
    /// [`PlayerId`], create its entity, and hand BOTH back — `player`
    /// keys every internal table from here on, `entity` is the wire id
    /// the join reply carries. The identity policy is the game's: core
    /// never invents player ids. A FRESH join mints a fresh id; the park
    /// ledger makes an id stable across resume for the same identity
    /// (the record rides the player state, §14.2).
    fn on_join(&mut self, world: &mut W, conn: ConnectionId) -> Admission;

    /// A player left the room: remove its entity (and any per-player
    /// bookkeeping). Keyed by the stable [`PlayerId`] (Faz 2): a leave of
    /// ANY session of this player lands here under the same key.
    fn on_leave(&mut self, world: &mut W, player: PlayerId);

    /// The connection's transport died: decide the parked entity's fate
    /// (`docs/RECONNECT.md` §3.1). Called once per disconnect, from the
    /// tick's CONTROL phase, INSTEAD of the `on_leave` a transport death
    /// used to trigger — a [`Detach::Hold`] answer keeps the entity, its
    /// world state, its group membership, AND the room-cap slot it
    /// occupies (§4: a parked player holds their slot).
    ///
    /// The logic records the park entry here (identity → entity + hold
    /// metadata) in WHATEVER storage it owns; per §14.2 that storage must
    /// be part of the migrating player state for sharded rooms, so a
    /// migration carries the park record along. Storage lives in the
    /// game state; policy and lookup live in this trait.
    ///
    /// `identity` is the resume key. On the ticket-auth path it is
    /// `ValidatedTicket.player`; on the local-auth path it is
    /// `Auth.name`, which makes resume demo/testing-only there (no
    /// cryptographic identity behind the name — noted at the ledger
    /// site by design).
    ///
    /// Default: [`Detach::Despawn`] — every pre-reconnect logic keeps
    /// today's behavior exactly, unchanged.
    fn on_disconnect(
        &mut self,
        _world: &mut W,
        _player: PlayerId,
        _identity: &str,
    ) -> Detach {
        Detach::Despawn
    }

    /// May the hold end NOW? Asked every CONTROL phase while a detached
    /// player is held WITHOUT a grace deadline (combat-held): the
    /// logic answers "no" while the hold must persist (an enemy nearby),
    /// "yes" to release. A veto extends the hold; the ceiling against an
    /// endless veto is the policy choosing `Hold { grace: Some(_) }`
    /// (§14.4: with a grace, the core's own timer ends the hold and this
    /// method is not consulted for timed holds). Cost note: only the
    /// (rarely populated) detached set is asked, per §14.4.
    ///
    /// Default: `true` (no logic veto — the hold ends at once, which for
    /// a `grace = None` hold means "expire immediately": logics that
    /// never want a hold simply return `Detach::Despawn` instead).
    fn may_release(&mut self, _world: &mut W, _player: PlayerId) -> bool {
        true
    }

    /// The hold ended without a resume (grace expired, or `may_release`
    /// cleared a combat-held player): the entity's end, as chosen by the
    /// policy's [`ExpireTo`]. After this call the core runs the ordinary
    /// despawn path for [`ExpireTo::Despawn`] (`on_leave` remains THE
    /// single despawn funnel — snapshot/membership contracts hang off
    /// it), or keeps everything alive with a `bot_fed` marker for
    /// [`ExpireTo::AiHandover`] (Tur B consumes the marker).
    ///
    /// Default: empty.
    fn on_detach_expired(&mut self, _world: &mut W, _player: PlayerId, _to: ExpireTo) {}

    /// Park-ledger query behind a resume attempt (§5/§7): does the ledger
    /// hold `identity`? See [`ResumeFound`] for the three answers and
    /// their exact client-visible consequences. The `Held` answer is the
    /// parked session's stable [`PlayerId`] — the key the core's own
    /// tables already use.
    ///
    /// Default: [`ResumeFound::Never`] — a logic without parks turns
    /// every identified join into an ordinary fresh join.
    fn resume_lookup(&self, _world: &W, _identity: &str) -> ResumeFound {
        ResumeFound::Never
    }

    /// A resume was accepted: the session moved onto `entity`/`player`
    /// through the fresh transport `conn`. With every table keyed by the
    /// stable [`PlayerId`] (Faz 2), the logic has almost NOTHING to
    /// re-key here any more — its own player-keyed tables kept their keys
    /// across the disconnect. What remains is exactly what is genuinely
    /// session-scoped: consume/update the ledger entry, reset per-session
    /// numbered-input state (DESIGN §14.2: the resumed session numbers
    /// from 1; a logic keeping per-conn sequence state resets it HERE,
    /// under the stable player key), and mark a delta-mode fresh member
    /// so the one-shot full flows to the NEW connection. Full-snapshot
    /// logics need nothing here (every snapshot is full).
    ///
    /// Default: no-op.
    fn on_resume(
        &mut self,
        _world: &mut W,
        _identity: &str,
        _conn: ConnectionId,
        _player: PlayerId,
        _entity: EntityId,
    ) {
    }

    /// Phase 2 — convert buffered actions into component writes.
    fn ingest(&mut self, world: &mut W, ctx: &TickCtx, actions: &mut Vec<Action>);

    /// Phase 3 — run the game systems for this tick.
    fn update(&mut self, world: &mut W, ctx: &TickCtx);

    /// Called when the room shuts down (world is dropped right after).
    fn on_shutdown(&mut self) {}

    /// Handle one correlated request (the RPC pattern; see `crate::rpc`
    /// for the contract: id space, ordering, caps, timeouts).
    ///
    /// The request is guaranteed to belong to a connection that is in
    /// the room (the actor only pulls actions of registered connections)
    /// and to carry a decodable base envelope with `id != 0` (the core
    /// rejects the malformed/uncorrelable cases before this call). The
    /// logic decides the request's fate:
    ///
    /// - [`RequestDecision::Reply`] — answered in this tick;
    /// - [`RequestDecision::Reject`] — a normal rejection, this tick;
    /// - [`RequestDecision::External`] — delegated; the core registers
    ///   the request as pending (subject to the pending caps — an
    ///   over-cap request is answered with a normal rejection even if
    ///   the logic said `External`) and hands the future to a worker;
    /// - `None` — the opcode is not a request this logic handles: the
    ///   core answers with a normal rejection (the client learns "no
    ///   handler" instead of waiting for its own timeout).
    ///
    /// Called once per request, in arrival order, AFTER this tick's
    /// `ingest` (a request sees the world after the tick's fire-and-
    /// forget actions were applied). Synchronous: an `External` decision
    /// must be an OWNING future (`'static`) — the tick body ends long
    /// before the work resolves.
    ///
    /// Default: `None` (a logic without request support gets a
    /// "no handler" answer for every request — no behaviour change for
    /// existing games, whose requests were previously ignored).
    ///
    /// Why this lives on the SHARED supertrait (Faz 3,
    /// `docs/TRAIT-ARCHITECTURE.md` §4): the method only gives a shard
    /// logic the *possibility* of answering requests; making it *work*
    /// needed the actor machinery (pending set, sweep, completion
    /// channel) on [`crate::shard::ShardActor`] — which now runs it with
    /// the same contract as [`RoomActor`]. One method, one decision
    /// vocabulary, two actors.
    fn handle_request(
        &mut self,
        _world: &mut W,
        _ctx: &TickCtx,
        _req: &crate::rpc::RpcRequest,
    ) -> Option<crate::rpc::RequestDecision> {
        None
    }

    /// The match result to report through the actor's result sink when it
    /// shuts down (any shutdown: a control-plane destroy, a server stop).
    /// The actor calls this after [`GameLogic::on_shutdown`], right
    /// before the world is dropped, passing the world (mutably — a
    /// final-state query, e.g. bevy's `Query`, needs it) so the logic can
    /// compute the result from final state without having cached it
    /// (no per-tick cost). `None` = no result (the sink receives
    /// nothing). The payload is game-encoded and opaque to the core.
    ///
    /// Delivery is best-effort (a bounded sink, a synchronous
    /// `try_send`): a full or gone sink drops the result and warns —
    /// a slow result consumer must not stall the teardown.
    ///
    /// Sharded rooms: EVERY shard calls this on its own teardown and
    /// reports through the SAME sink under the same logical room id, so
    /// one logical room yields one payload PER SHARD (the platform's
    /// adapter concatenates/filters; nothing was added to the wire — see
    /// `crate::shard`). Default: no result.
    fn match_result(&mut self, _world: &mut W) -> Option<bytes::Bytes> {
        None
    }

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

/// Game-side behaviour of a SINGLE-ROOM actor. Faz 3 promoted the two
/// request/result seams ([`GameLogic::handle_request`] /
/// [`GameLogic::match_result`]) onto the SHARED supertrait, so this
/// subtrait no longer adds methods — it remains the compile-time marker
/// that a logic is built for the single-room actor (the same
/// deliberate-subtrait discipline the shard side keeps with its topology
/// hooks; `docs/TRAIT-ARCHITECTURE.md` §3). The RPC/result MACHINERY
/// (pending set, sweep, completion channel, sink) is actor-side state,
/// not trait surface: [`RoomActor`] and [`crate::shard::ShardActor`]
/// each run their own copy of it.
pub trait RoomLogic<W>: GameLogic<W> {}

/// Per-player row of the room (or shard) member table. `pub(crate)`
/// because the shard actor reuses the same table shape (a shard's `conns`
/// is the shard's share of the room's members — see `crate::shard`).
///
/// Keyed by the stable [`PlayerId`] (Faz 2): the row — and everything it
/// carries (entity, group, detach flags) — survives resume and migration
/// under its key. Only the transport session feeding it changes.
pub(crate) struct RoomConn<G> {
    /// The transport session CURRENTLY bound to this player (the binding
    /// table's back-reference; updated at join/resume together with the
    /// binding entry). Used exactly where a control message names a
    /// session-scoped thing owned by this row: pruning its RPC request
    /// state and removing its binding row on despawn.
    pub(crate) conn: ConnectionId,
    pub(crate) out: mpsc::Sender<FrameBatch>,
    /// This connection's input, written by its connection actor as frames
    /// arrive; pulled non-blockingly at each step.
    pub(crate) actions: Inbox<Action>,
    pub(crate) entity: EntityId,
    /// Snapshot group this connection belongs to (recomputed every tick via
    /// [`GameLogic::group_of`]).
    pub(crate) group: G,
    /// The fan-out batch buffer, reused across ticks (the measured floor
    /// carried one `Vec::with_capacity(2)` per connection per tick; the
    /// capacity is retained so the 0-2-frame batch never allocates again
    /// after warm-up — see `docs/ROADMAP.md`, the floor breakdown).
    pub(crate) batch: FrameBatch,
    // -- Detach/resume state (§14.4: the CLOCK is core-owned; §7: the
    //    broadcast/READ skip flags are flag-guarded so a dead outbound
    //    half never pollutes the drop counter). --------------------------
    /// The transport died but the entity is parked (a [`Detach::Hold`]
    /// policy answer): READ pulls nothing for this row and BROADCAST ships
    /// it nothing (the outbound half is dead; a `try_send` against it
    /// would count a drop nobody caused). Everything else is unchanged:
    /// the entity stays in `ingest`, `update`, snapshots, group
    /// membership, and the member/slot accounting (§4).
    pub(crate) detached: bool,
    /// When the hold ends at the latest (`Detach::Hold.grace` mapped to an
    /// absolute instant by the CORE — §14.4 deadline ownership);
    /// `None` = combat-held, only [`GameLogic::may_release`] ends it.
    pub(crate) detach_deadline: Option<Instant>,
    /// The policy's chosen end ([`ExpireTo`]) when the hold expires.
    pub(crate) expire_to: ExpireTo,
    /// The hold expired toward [`ExpireTo::AiHandover`]: the entity keeps
    /// playing on synthesized input (the game logic synthesizes; Tur B's
    /// demo bot consumes this marker). Channels stay alive; the row stays
    /// skipped for READ/broadcast exactly like a detached one (there is no
    /// human socket behind it), and the sweep never re-fires (deadline
    /// cleared).
    pub(crate) bot_fed: bool,
    /// The resume-accept epoch stamp (§7's guard): the newest session that
    /// (re)bound this row. A later resume with an older-or-equal epoch is
    /// a delayed duplicate and is rejected. `0` = unset (guard off).
    pub(crate) session_epoch: u64,
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
/// (bounded) metrics channel with a synchronous `try_send` — no await.
/// `pub(crate)`
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
    /// [`GameLogic::encoded_records`]), cumulative. Together with the
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
    /// Resumes accepted (a parked session rebound onto a fresh socket),
    /// cumulative (§10).
    pub(crate) resumes: u64,
    /// Resume attempts rejected as stale — the ledger answered
    /// [`ResumeFound::Ended`] or the epoch guard tripped (§7/§10). The
    /// client-visible outcome of an `Ended` rejection is still the
    /// transparent fresh join; this counter is the mechanism-level signal.
    pub(crate) resume_rejected_stale: u64,
    /// Holds that expired toward [`ExpireTo::Despawn`] (slot released),
    /// cumulative.
    pub(crate) detach_expired_despawn: u64,
    /// Holds that expired toward [`ExpireTo::AiHandover`] (entity kept,
    /// bot-fed marker set — Tur B's seam), cumulative.
    pub(crate) detach_expired_ai: u64,
    /// RPC requests answered room-local in the same tick, cumulative.
    pub(crate) requests_local: u64,
    /// RPC requests delegated to a worker (registered pending), cumulative.
    pub(crate) requests_external: u64,
    /// RPC rejections, split by cause (one bucket per terminal reject
    /// decision in the tick body — see the `2a`/`2c` phases): the
    /// buckets answer distinct operational questions (client protocol
    /// bug vs. duplicate storm vs. one hoarding connection vs. room
    /// budget vs. the game logic's own business rejections), which is
    /// what the cap-sizing measurement needs (§6.1 of
    /// `docs/RPC-CONTROL-PLANE.md`). Cumulative.
    /// Malformed envelope or correlation id = 0 (cannot correlate).
    pub(crate) requests_rejected_malformed: u64,
    /// In-flight duplicate id (every decision kind; rejected without
    /// re-processing).
    pub(crate) requests_rejected_dup: u64,
    /// The room's logic handles no request for the op.
    pub(crate) requests_rejected_no_handler: u64,
    /// The logic's own `Reject` decision (a business answer — normal
    /// flow, not an anomaly).
    pub(crate) requests_rejected_logic: u64,
    /// The per-connection pending cap bound the request.
    pub(crate) requests_rejected_conn_cap: u64,
    /// The room-wide pending cap bound the request.
    pub(crate) requests_rejected_room_cap: u64,
    /// Pending external requests swept as timed out (the client-visible
    /// timeout; see `crate::rpc`), cumulative.
    pub(crate) requests_timed_out: u64,
    /// Worker reports that arrived for an id no longer pending (already
    /// answered, timed out, or the connection left) and were dropped,
    /// cumulative (the exactly-one-answer reconciliation in action).
    pub(crate) requests_late: u64,
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
            resumes: 0,
            resume_rejected_stale: 0,
            detach_expired_despawn: 0,
            detach_expired_ai: 0,
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
            step_max_group: 0,
        }
    }
}

/// The room actor. Owns the world, the player table (+ its session
/// binding table), and the group table; everything mutable is local, so
/// no synchronization is needed.
///
/// `G` is the game logic's group key ([`GameLogic::GroupKey`]) and `Sp`
/// its strip payload ([`GameLogic::Strip`], opaque here — the room never
/// exchanges borders); the room
/// stores per-group state (last snapshot, this tick's ship, diagnostics)
/// under the key.
pub struct RoomActor<W, G, Sp> {
    config: RoomConfig,
    world: W,
    logic: Box<dyn RoomLogic<W, GroupKey = G, Strip = Sp>>,
    tick_rx: broadcast::Receiver<TickInfo>,
    control_rx: Inbox<RoomControl>,
    /// The room's members, keyed by their STABLE player identity (Faz 2,
    /// `docs/TRAIT-ARCHITECTURE.md` §5). The key survives resume and (for
    /// the shard sibling) migration, so a resume no longer re-keys this
    /// table at all — only the channel halves inside the row swap.
    conns: HashMap<PlayerId, RoomConn<G>>,
    /// THE session binding table (Faz 2): transport session → player.
    /// ONE map owns the conn ↔ PlayerId context; established at
    /// join/resume, torn down at leave / detach-expire-despawn. Resume
    /// updates THIS table (plus the channel halves inside the `conns`
    /// row) instead of re-keying N tables — the §14.1 RebindKey pass
    /// shrank to exactly this move (see `rebind_session`).
    ///
    /// Also the ingest-side authority: every pulled action's `conn` is
    /// translated to a `PlayerId` through it before CONVERT; an unbound
    /// conn (an old session's stray frame after a resume) drops there.
    binding: HashMap<ConnectionId, PlayerId>,
    /// READ-phase scan order: the member players in join order, kept
    /// exactly in sync with `conns` by the control path only
    /// ([`Self::roster_add`] on join, [`Self::roster_remove`] on leave /
    /// superseded rejoin). A `Vec` because the READ phase needs a
    /// deterministic order it can rotate: a `HashMap` iteration is
    /// arbitrary but fixed within a run, and under a sustained overload
    /// that fixed prefix consumes the whole room pull budget on every
    /// tick while the tail connections are never reached (their actions
    /// deferred forever — see the READ phase's rotation note).
    ///
    /// Keyed by `PlayerId` (Faz 2): stable across resume — a rebind does
    /// not touch the roster AT ALL, so the rotation cursor's meaning is
    /// trivially unaffected (the pre-Faz-2 in-place rename is gone).
    roster: Vec<PlayerId>,
    /// Each player's index in `roster` — the bookkeeping that makes a
    /// membership removal a swap-remove plus one index fix instead of an
    /// O(n) scan. Control-path state only: never touched between ticks.
    roster_pos: HashMap<PlayerId, usize>,
    /// Round-robin cursor over `roster`: each READ starts at
    /// `read_cursor % roster.len()` and advances past every connection
    /// examined. Kept as an absolute count (not a raw index) so
    /// membership changes between reads degrade to a shifted start
    /// offset, never an out-of-range index.
    read_cursor: usize,
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
    // -- RPC (deferred completion; see `crate::rpc`) ----------------------
    /// In-flight external requests, per TRANSPORT SESSION (Faz 2 decision,
    /// documented where the contract left latitude): `pending`/`queued`
    /// stay conn-keyed BY DESIGN — they are session-scoped RPC state that
    /// dies with the session. RECONNECT §11 fixes today's leave semantics
    /// ("pending drops at detach; late reports are silently discarded")
    /// and detach already clears both tables, so a resumed player never
    /// inherits the dead session's in-flight work; re-keying them to
    /// PlayerId would CHANGE that policy (a resume would carry pending
    /// requests across sessions) for zero gain. Listed in
    /// `rebind_session`'s enumeration as CHECKED, not missed.
    /// (FIFO: deadlines are non-decreasing per connection, so only the
    /// head can expire.) Owner of the pending state: the room is the
    /// single authority on "is this request still pending" (module docs
    /// of `crate::rpc`).
    pending: HashMap<ConnectionId, std::collections::VecDeque<crate::rpc::PendingRequest>>,
    /// Total in-flight external requests (the room-wide cap).
    pending_total: usize,
    /// This tick's queued RPC answers per transport session; drained in
    /// the broadcast phase (handed to the logic's `private`) and emptied.
    /// Cleared with the SESSION on leave/rejoin/detach (a stale answer to
    /// a gone session must not be delivered); conn-keyed for the same
    /// session-scope reason as `pending` above.
    queued: HashMap<ConnectionId, Vec<crate::rpc::RpcReply>>,
    /// Per-tick scratch for the rare path of the broadcast's RPC-answer
    /// hand-off (a connection's removed `queued` entry, borrowed for the
    /// logic's `private` call). A field so its capacity survives across
    /// ticks; the quiet room never writes it.
    replies_buf: Vec<crate::rpc::RpcReply>,
    /// Worker reports: the room's inbox for delegated-request outcomes
    /// (drained non-blockingly in the CONTROL phase — no new await).
    completions: Inbox<crate::rpc::Completion>,
    /// The room's sender half of the completion channel, cloned to each
    /// spawned worker (bounded: a burst of completions cannot exceed the
    /// room-wide pending cap, which is the channel's capacity).
    completions_tx: Mailbox<crate::rpc::Completion>,
    /// The room's match-result sink (the control plane's result seam; see
    /// [`RoomLogic::match_result`]): a bounded mailbox, sent to with the
    /// synchronous `try_send` on shutdown (no await, best effort).
    result_sink: Option<Mailbox<crate::registry::MatchResult>>,
    /// The registry's mailbox, used for exactly one report: a park that
    /// expired toward despawn ([`crate::registry::RegistryMsg::ParkExpired`]).
    /// `None` for a standalone room (the direct-drive test harnesses) — it
    /// then simply has no registry to tell.
    registry: Option<Mailbox<crate::registry::RegistryMsg>>,
    /// Park expiries not yet accepted by the registry's mailbox.
    ///
    /// The report is a `try_send` (the tick body stays synchronous — the
    /// room's only await is `tick_rx.recv()`), so a momentarily full
    /// registry mailbox would otherwise DROP it — and a dropped report is
    /// the very leak this message exists to close. Un-sent ids wait here
    /// and are retried on later ticks instead. Bounded in practice by the
    /// parks that expire while the registry is saturated, and it drains as
    /// soon as the registry catches up.
    park_reports: Vec<ConnectionId>,
}

impl<W, G, Sp> RoomActor<W, G, Sp>
where
    G: Eq + Hash + Clone + Debug,
    // The strip payload's trait bounds (`GameLogic::Strip`) — restated so
    // calls through the logic object type-check; the room itself never
    // touches the payloads.
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    // The actor's wiring (the base's actor constructors take every
    // mailbox the actor owns — the `new` is the composition point).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: RoomConfig,
        world: W,
        logic: Box<dyn RoomLogic<W, GroupKey = G, Strip = Sp>>,
        tick_rx: broadcast::Receiver<TickInfo>,
        control_rx: Inbox<RoomControl>,
        run_every: u64,
        // Outbound metrics path (see `crate::metrics`): a bounded channel;
        // the room sends with the synchronous `try_send` (no await). A
        // dropped receiver makes the send fail (ignored) — the room does not
        // observe it.
        metrics: mpsc::Sender<MetricsEvent>,
        // The room's match-result sink (the control plane's result seam —
        // see `RoomLogic::match_result`): a bounded mailbox; `None` = the
        // room reports no result.
        result_sink: Option<Mailbox<crate::registry::MatchResult>>,
    ) -> Self {
        // The completion channel: bounded at the room-wide pending cap (a
        // completion burst cannot exceed the number of in-flight workers,
        // which is the cap) — the usual backpressure rule; the room drains
        // it every tick's CONTROL phase, so a full channel only parks a
        // worker until the next tick, never the room.
        let (completions_tx, completions) =
            mpsc::channel(config.max_pending_requests.max(1));
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
            binding: HashMap::new(),
            roster: Vec::new(),
            roster_pos: HashMap::new(),
            read_cursor: 0,
            groups: HashMap::new(),
            run_every: run_every.max(1),
            last_at: None,
            steps: 0,
            keepalive_every,
            metrics_every,
            budget_us,
            m: RoomCounters::default(),
            metrics,
            pending: HashMap::new(),
            pending_total: 0,
            queued: HashMap::new(),
            replies_buf: Vec::new(),
            completions,
            completions_tx,
            result_sink,
            registry: None,
            park_reports: Vec::new(),
        }
    }

    /// Give the room the registry mailbox it reports park expiries on
    /// (see [`crate::registry::RegistryMsg::ParkExpired`]). A builder
    /// instead of another `new` parameter: every direct-drive harness
    /// constructs rooms without a registry, and `new` is already at the
    /// argument limit.
    pub fn with_registry(mut self, registry: Mailbox<crate::registry::RegistryMsg>) -> Self {
        self.registry = Some(registry);
        self
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
        // The match-result seam (control plane): the logic computes the
        // final result from the world (still alive — it is dropped only
        // when `self` drops, below) and the room reports it to the sink
        // with the synchronous `try_send` (no await: the room's only
        // await stayed `tick_rx.recv()`). Best effort — a full or gone
        // sink drops the result and warns (a slow result consumer must
        // not stall the room's teardown).
        if let Some(result) = self.logic.match_result(&mut self.world)
            && let Some(sink) = &self.result_sink
        {
            match sink.try_send(crate::registry::MatchResult {
                room: self.config.id,
                payload: result,
            }) {
                Ok(()) => debug!(room = %self.config.id, "match result reported"),
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(room = %self.config.id, "match result dropped: sink full");
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    debug!(room = %self.config.id, "match result dropped: sink gone");
                }
            }
        }
        debug!(
            room = %self.config.id,
            dropped_frames = self.m.dropped_frames,
            "room actor stopped"
        );
    }

    /// One full step: control → read → convert → systems → broadcast,
    /// measured (tick latency + step body duration) and flushed to the
    /// metrics channel once at the end. Synchronous: the send is a
    /// bounded-channel `try_send` (drops counted, never parks), so the
    /// room's only await stays `tick_rx.recv()`. Returns `false` when the
    /// actor should stop.
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

        // -- Phase 0b — deferred completions (the RPC pattern, see
        //    `crate::rpc`): drain the worker reports (non-blocking — the
        //    room never awaits a worker; this is the same try_recv
        //    discipline as the control channel above) and reconcile each
        //    report against the pending set. Exactly one answer per
        //    request is structural: a report for an id that is no longer
        //    pending (already answered, timed out below, or its
        //    connection left) is dropped and counted.
        while let Ok(rep) = self.completions.try_recv() {
            let Some(deq) = self.pending.get_mut(&rep.conn) else {
                // The connection left (its pending set was cleared on
                // leave/rejoin): the report is stale by construction.
                self.m.requests_late += 1;
                continue;
            };
            match deq.iter().position(|p| p.id == rep.id) {
                Some(idx) => {
                    // The inner op comes from the pending entry (the
                    // worker's report is outcome-only — the room is the
                    // authority on the request's shape).
                    let op = deq[idx].op;
                    deq.remove(idx);
                    self.pending_total -= 1;
                    if deq.is_empty() {
                        self.pending.remove(&rep.conn);
                    }
                    self.queued
                        .entry(rep.conn)
                        .or_default()
                        .push(crate::rpc::RpcReply {
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
        // Sweep expired pending requests (the client-visible timeout —
        // see `crate::rpc`): per connection the deadlines are
        // non-decreasing (same timeout, FIFO arrivals), so only the head
        // of each deque can be due. The room's tick body stays
        // synchronous: this is a wall-clock comparison, no await.
        // Cost when quiet: one `is_empty` probe (the common case — a
        // room with no pending requests pays nothing below it).
        if !self.pending.is_empty() {
            let now = Instant::now();
            // Pop the due heads (conn + request together — the reply is
            // owed to the request's owner), then queue the timeout
            // replies outside the borrow of `self.pending`.
            let mut due: Vec<(ConnectionId, crate::rpc::PendingRequest)> = Vec::new();
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
                self.queued.entry(conn).or_default().push(crate::rpc::RpcReply {
                    id: p.id,
                    ok: false,
                    op: p.op,
                    reason: crate::rpc::TIMEOUT_REASON.to_string(),
                    payload: bytes::Bytes::new(),
                });
            }
        }

        // -- Phase 0c — detach-hold sweep (§14.4: the deadline clock is
        //    CORE-owned; the logic owns the policy). Two arms, exactly as
        //    resolved in §14.4:
        //
        //    - `grace = Some(d)`: the core fires its OWN deadline —
        //      `may_release` is not consulted for timed holds (the grace
        //      IS the ceiling that makes an endless veto impossible);
        //    - `grace = None` (combat-held): the core asks `may_release`
        //      every tick — the detached set is tiny (parks are rare),
        //      so the per-tick cost is a filter pass over the table that
        //      short-circuits on the `detached` flag.
        //
        //    The ended hold is handed to `on_detach_expired(to)` and then:
        //    Despawn → the ordinary despawn path (`on_leave` stays THE one
        //    despawn funnel; slot released); AiHandover → everything stays
        //    alive under a `bot_fed` marker (Tur B synthesizes the input;
        //    this is the documented seam).
        if self.conns.values().any(|rc| rc.detached && !rc.bot_fed) {
            let now = Instant::now();
            // Timed holds past their deadline + combat-helds the logic is
            // ready to release. Collected first so each logic callback runs
            // against an unborrowed `self`.
            let mut due: Vec<(PlayerId, ExpireTo)> = Vec::new();
            let mut ask: Vec<PlayerId> = Vec::new();
            for (&pid, rc) in &self.conns {
                if !rc.detached || rc.bot_fed {
                    continue;
                }
                match rc.detach_deadline {
                    Some(dl) if now >= dl => due.push((pid, rc.expire_to)),
                    Some(_) => {}
                    None => ask.push(pid),
                }
            }
            for pid in ask {
                if self.logic.may_release(&mut self.world, pid) {
                    // The veto cleared: the hold ends NOW, toward the same
                    // `ExpireTo` the policy chose at detach time.
                    let to = self
                        .conns
                        .get(&pid)
                        .map(|rc| rc.expire_to)
                        .unwrap_or(ExpireTo::Despawn);
                    due.push((pid, to));
                }
            }
            for (pid, to) in due {
                self.logic.on_detach_expired(&mut self.world, pid, to);
                match to {
                    ExpireTo::Despawn => {
                        self.m.detach_expired_despawn += 1;
                        // The registry is holding a detached row (and a
                        // cap slot) for this session; the despawn below
                        // is the event that ends it, and this room is the
                        // only actor that sees it happen. Queued only when
                        // there IS a registry — a standalone room has no
                        // reader, so the queue must not accumulate.
                        if self.registry.is_some()
                            && let Some(conn) = self.conns.get(&pid).map(|rc| rc.conn)
                        {
                            self.park_reports.push(conn);
                        }
                        self.despawn_conn(pid, false);
                        debug!(room = %self.config.id, %pid, "detach hold expired: despawn");
                    }
                    ExpireTo::AiHandover => {
                        self.m.detach_expired_ai += 1;
                        if let Some(rc) = self.conns.get_mut(&pid) {
                            rc.bot_fed = true;
                            // The deadline must never re-fire (the row stays
                            // held by the bot); clearing it also takes the
                            // row out of the `may_release` polling set.
                            rc.detach_deadline = None;
                        }
                        debug!(
                            room = %self.config.id,
                            %pid,
                            "detach hold expired: AI handover (bot_fed; Tur B seam)"
                        );
                    }
                }
            }
        }

        // -- Park-expiry reports: hand the registry back the rows (and cap
        //    slots) whose holds ended this tick, plus anything an earlier
        //    tick could not place. Synchronous `try_send` — the tick body
        //    stays await-free — and whatever the mailbox refuses stays
        //    queued for the next tick rather than being dropped (a dropped
        //    report IS the leak this closes).
        //    A CLOSED mailbox (the registry is gone — the process is
        //    coming down) drops the report instead of retrying forever:
        //    there is no table left to leak into.
        if !self.park_reports.is_empty()
            && let Some(registry) = &self.registry
        {
            let room = self.config.id;
            self.park_reports.retain(|&conn| {
                matches!(
                    registry.try_send(crate::registry::RegistryMsg::ParkExpired { conn, room }),
                    Err(mpsc::error::TrySendError::Full(_))
                )
            });
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
        //    (c) deterministic rotation: the scan does NOT walk the
        //    `conns` HashMap — its iteration order is arbitrary but fixed
        //    within a run, so under a sustained overload that fixed
        //    hash-order prefix would consume the entire pull budget on
        //    every tick while the tail connections were never REACHED
        //    (deferred ≠ ever delivered). Instead the scan follows
        //    `roster` (join order) from a rotating cursor advanced past
        //    every connection examined, so every connection is reached
        //    within one full rotation (`roster.len()` READ phases) no
        //    matter how much input the connections ahead of it hold.
        //    This is the reach-side sibling of (a): (a) bounds how much
        //    ONE connection can take from a tick; (c) guarantees what is
        //    left is shared around, not taken by the same fixed prefix
        //    every tick.
        //
        //    Consequence: the room never drops an action (`dropped_actions`
        //    stays 0). The architecture's only input-loss point is a
        //    connection's own full action channel — self-inflicted and
        //    attributed (see `conn::ConnectionActor`).
        let per_conn = self.config.max_actions_per_conn_per_tick;
        let mut budget = self.config.max_pending_actions;
        let mut actions: Vec<Action> = Vec::new();
        // The rotating scan (see (c) above): start at the cursor over the
        // join-order roster, visit connections until either a full
        // rotation is done or the pull budget ran out, then advance the
        // cursor past every connection EXAMINED — so the next READ
        // resumes exactly where this one stopped. Same shape as the
        // hash-order walk it replaces (O(visited), no allocation, no
        // sort); only the start position moves.
        let n = self.roster.len();
        let mut visited = 0usize;
        let mut idx = if n == 0 { 0 } else { self.read_cursor % n };
        while visited < n && budget > 0 {
            // Roster and table are kept in sync by the control path, so
            // the entry is always present; a plain lookup (no unwrap)
            // keeps hypothetical drift a skip, not a panic. A DETACHED
            // row is skipped (§3.2): its input source is dead — nothing
            // pulls from it — but the visit still counts toward the
            // rotation so the cursor's fairness contract is untouched.
            if let Some(rc) = self.conns.get_mut(&self.roster[idx])
                && !rc.detached
            {
                for _ in 0..per_conn {
                    if budget == 0 {
                        break;
                    }
                    match rc.actions.try_recv() {
                        Ok(a) => {
                            budget -= 1;
                            actions.push(a);
                        }
                        Err(_) => break, // channel drained
                    }
                }
            }
            visited += 1;
            idx += 1;
            if idx == n {
                idx = 0;
            }
        }
        self.read_cursor = self.read_cursor.wrapping_add(visited);

        // -- Phase 1.5 — BINDING TRANSLATION (Faz 2): the wire protocol is
        //    unchanged — every action still names its transport session
        //    (`Action.conn`) — but the world is keyed by stable player
        //    identity. The binding table is the ONE authority for the
        //    conn ↔ PlayerId context: each action's session is translated
        //    to its player here, before any logic sees the action.
        //
        //    An unbound conn DROPS here: today's stale-action path (the
        //    game ingest silently skipping actions of conns not in the
        //    table) mirrored — the drop just moved to where the binding
        //    actually lives. This is what makes a resumed player safe: the
        //    old session's binding row was removed at the rebind, so a
        //    stray frame sent under the OLD conn after resume can never
        //    reach the world (locked by
        //    `actions_from_the_old_session_are_dropped_after_resume`).
        //    Structurally such strays are already rare — the old channel's
        //    receiver died with the rebind — this is the belt under that
        //    suspenders.
        actions.retain_mut(|a| match self.binding.get(&a.conn) {
            Some(&player) => {
                a.player = player;
                true
            }
            None => {
                debug!(
                    room = %self.config.id,
                    conn = %a.conn,
                    op = a.op,
                    "action dropped: connection not bound (stale/old session)"
                );
                false
            }
        });

        // -- Phase 2a — split the requests out of the pulled actions (the
        //    RPC pattern, see `crate::rpc`). A request is an action
        //    carrying the base-band envelope opcode; the core decodes the
        //    envelope (a base message) and hands the rest to the logic.
        //    The split is in-order: both the actions and the requests
        //    keep their arrival order (the processing order contract:
        //    all actions, then all requests — module docs of `rpc`).
        //
        //    Cost when quiet: one `u16` compare per pulled action — the
        //    common case (no requests in the tick) touches nothing else.
        //
        //    A malformed envelope is a *normal rejection* (a client bug —
        //    the same class as an undecodable game payload, which the
        //    logic ignores today), not a protocol violation: the room
        //    answers with a reject reply and counts it. `id = 0` is
        //    rejected the same way (it cannot correlate).
        let mut requests: Vec<crate::rpc::RpcRequest> = Vec::new();
        actions.retain_mut(|a| {
            if a.op != crate::rpc::RPC_REQ_OP {
                return true;
            }
            match gsb_protocol::base::RpcRequest::decode(&a.payload[..]) {
                Ok(env) if env.id != 0 => {
                    requests.push(crate::rpc::RpcRequest {
                        conn: a.conn,
                        // Already translated (phase 1.5): the logic sees
                        // the stable player key, while pending/replies
                        // stay session-scoped under `conn`.
                        player: a.player,
                        id: env.id,
                        // The wire type is `u32` (proto3 has no 16-bit
                        // integers); the op space is `u16` by protocol
                        // contract — the same range the op registry
                        // applies to every opcode (a value above the
                        // space decodes to the `u16` truncation, and the
                        // logic simply sees an op it does not handle).
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

        // -- Phase 2b — CONVERT: actions → component writes (game logic).
        //    All fire-and-forget actions are processed before the
        //    requests (the ordering contract; a request sees the world
        //    after this tick's actions were applied).
        self.logic.ingest(&mut self.world, &ctx, &mut actions);

        // -- Phase 2c — REQUESTS: the correlated requests (see `rpc`).
        //    Synchronous: an `External` decision returns an owning
        //    future that a spawned worker resolves off the tick; the
        //    tick body only registers the request and (for the deferred
        //    ones) spawns the worker.
        for req in &requests {
            // Duplicate id that is still in flight: reject WITHOUT
            // re-processing — for every decision kind, not just external.
            // An in-flight id is already correlated with a pending
            // request: a second request under the same id (even a
            // room-local one, which would be answered in this very tick)
            // would produce a second reply for one id, and a retrying
            // client must not be able to buy a double-applied side
            // effect (see the `rpc` module docs). Normal rejection, same
            // tick; the id becomes reusable once the first request is
            // answered (no unbounded history).
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
                    // Not a request this logic handles: answer with a
                    // normal rejection (the client learns "no handler"
                    // instead of waiting for its own timeout).
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
                    // Caps (the room's authority on pending state): a
                    // request past a cap is a normal rejection, same
                    // tick — the logic's `External` decision does not
                    // commit the room to registering it. (The duplicate
                    // check is above the decision: it applies to every
                    // kind.)
                    let per_conn = self
                        .pending
                        .get(&req.conn)
                        .map(std::collections::VecDeque::len)
                        .unwrap_or(0);
                    let over_conn_cap = per_conn >= self.config.max_pending_requests_per_conn;
                    let over_room_cap = self.pending_total >= self.config.max_pending_requests;
                    if over_conn_cap || over_room_cap {
                        // A request past both caps counts against the
                        // per-connection one (the reply names it first —
                        // the client's own quota is the actionable one).
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
                    // Register it pending, then delegate.
                    let due = Instant::now() + self.config.request_timeout;
                    self.pending
                        .entry(req.conn)
                        .or_default()
                        .push_back(crate::rpc::PendingRequest {
                            id: req.id,
                            op: req.op,
                            due,
                        });
                    self.pending_total += 1;
                    self.m.requests_external += 1;
                    // The worker: resolves the future (or gives up at the
                    // same deadline the room's sweep enforces — the
                    // worker's timeout is a resource guard, the room's
                    // sweep is the client-visible authority; on expiry
                    // the worker reports nothing). The report rides the
                    // completion channel; the room reconciles it on a
                    // later tick's CONTROL phase.
                    let conn = req.conn;
                    let id = req.id;
                    let timeout = self.config.request_timeout;
                    let report_tx = self.completions_tx.clone();
                    tokio::spawn(async move {
                        match tokio::time::timeout(timeout, fut).await {
                            Ok(Ok(payload)) => {
                                let _ = report_tx
                                    .send(crate::rpc::Completion::reply(conn, id, payload))
                                    .await;
                            }
                            Ok(Err(reason)) => {
                                let _ = report_tx
                                    .send(crate::rpc::Completion::error(conn, id, reason))
                                    .await;
                            }
                            Err(_elapsed) => {
                                // Timed out: no report — the room's sweep
                                // owns the client-visible timeout (it may
                                // have answered it already, on this or a
                                // previous tick). The worker exits.
                            }
                        }
                        // The report send fails (the channel is closed)
                        // when the room shut down: the worker exits
                        // either way — its lifetime is bounded by the
                        // timeout in every case (no task outlives a
                        // request by more than the timeout).
                    });
                }
            }
        }

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
            // Gauge, computed at sample time from the table it describes
            // (§10: "anlık park sayısı" — the instant park count).
            detached: self.conns.values().filter(|rc| rc.detached).count() as u32,
            resumes: self.m.resumes,
            resume_rejected_stale: self.m.resume_rejected_stale,
            detach_expired_despawn: self.m.detach_expired_despawn,
            detach_expired_ai: self.m.detach_expired_ai,
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

    /// Phase 4, in four passes. Field-level borrows keep the logic, the
    /// world, the connection table, and the group table independently
    /// accessible — disjoint fields, no synchronization.
    fn broadcast_phase(&mut self, ctx: &TickCtx) {
        let snap_op = self.logic.snapshot_op();
        let priv_op = self.logic.private_op();

        // 4a. Recompute each player's group (a group may depend on the
        //     world, e.g. zones).
        for (player, rc) in self.conns.iter_mut() {
            rc.group = self.logic.group_of(&self.world, *player);
        }

        // 4b. Rebuild the group table. Membership churn (join/leave)
        //     shows up here as a different member set — the game logic's
        //     "no change" test in `snapshot` must account for it.
        let mut members: HashMap<G, Vec<PlayerId>> = HashMap::new();
        for (&player, rc) in &self.conns {
            members.entry(rc.group.clone()).or_default().push(player);
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
        //     `GameLogic::keepalive`).
        let keep_due = self
            .keepalive_every
            .map(|every| self.steps.is_multiple_of(every))
            .unwrap_or(false);
        let mut buf = bytes::BytesMut::new();
        for (group, st) in self.groups.iter_mut() {
            buf.clear();
            // No boundary records on the single-room path: the borrowed
            // slice is a sharded-execution-only input (see
            // `GameLogic::snapshot`).
            let emitted = self.logic.snapshot(&mut self.world, ctx, group, &[], &mut buf);
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
        // RPC answers are the rare case: in a quiet room (no in-flight
        // requests — the loadgen steady state) this is ONE `is_empty`
        // probe for the whole fan-out. The per-connection map probe runs
        // only on the ticks that actually owe an answer (measured: the
        // 500-probe-per-tick form added tens of microseconds to the tick
        // floor; the quiet path must stay O(1), like the 0b sweep above).
        let has_replies = !self.queued.is_empty();
        for (&player, rc) in self.conns.iter_mut() {
            // A detached (or bot-fed) row ships nothing: its outbound half
            // is dead (or has no human behind it). Skipping here — instead
            // of letting the `try_send` fail — is what keeps the room's
            // drop counter meaning "slow CLIENT" and nothing else (§7).
            // Its group snapshot is still encoded (the parked entity is in
            // the world and the other members must see it); only THIS
            // row's fan-out is skipped.
            if rc.detached {
                continue;
            }
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
            // This connection's queued RPC answers for the tick (empty for
            // the common case — the `has_replies` guard above keeps the
            // quiet fan-out free of per-connection map probes). The logic
            // encodes them into the private frame alongside any ack /
            // one-shot full.
            let replies: &[crate::rpc::RpcReply] = if has_replies {
                self.replies_buf = self.queued.remove(&rc.conn).unwrap_or_default();
                &self.replies_buf
            } else {
                &[]
            };
            pbuf.clear();
            if self.logic.private(&mut self.world, player, &rc.group, replies, &mut pbuf) {
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
        // Every connection was visited above (each owns at most one
        // queued entry this tick), so anything left here belongs to a
        // connection that was removed from the table this same tick and
        // never got its frame: drop it (the request is answered exactly
        // once — it was never delivered).
        if !self.queued.is_empty() {
            self.queued.clear();
        }
        self.m.dropped_frames += dropped;
    }

    /// Queue one RPC answer for a connection's next (or this tick's, if
    /// broadcast has not run yet) private frame. All request paths —
    /// same-tick reply/reject, cap/duplicate rejects, the worker-report
    /// reconciliation, the timeout sweep — funnel through here, so the
    /// per-tick delivery point is exactly one.
    fn queue_reply(
        &mut self,
        conn: ConnectionId,
        id: u64,
        op: u16,
        ok: bool,
        reason: String,
        payload: bytes::Bytes,
    ) {
        self.queued.entry(conn).or_default().push(crate::rpc::RpcReply {
            id,
            ok,
            op,
            reason,
            payload,
        });
    }

    /// Drop a connection's request state (pending set + queued answers).
    /// Called on leave and on join (a join supersedes the connection's
    /// prior state, including a queued leave). Late worker reports for
    /// the dropped requests find no pending entry and are dropped (the
    /// exactly-one-answer reconciliation); the workers themselves exit
    /// on their own (report send against a dropped entry, or their
    /// timeout).
    fn drop_conn_request_state(&mut self, conn: ConnectionId) {
        if let Some(deq) = self.pending.remove(&conn) {
            self.pending_total = self.pending_total.saturating_sub(deq.len());
        }
        self.queued.remove(&conn);
    }

    /// Register a freshly joined player at the tail of the READ
    /// roster (why the roster exists: see the field docs and the READ
    /// phase's rotation note).
    fn roster_add(&mut self, player: PlayerId) {
        self.roster_pos.insert(player, self.roster.len());
        self.roster.push(player);
    }

    /// Remove a player from the READ roster: swap-remove plus one
    /// index fix for the element moved into the freed slot (O(1)
    /// amortized; no scan). Called exactly where `conns` loses an entry
    /// (a leave, or a join superseding a stale session), so the roster
    /// cannot drift from the table.
    ///
    /// The fix targets the element that RELOCATED — `swap_remove`
    /// returns the REMOVED element (`player` itself), and the former last
    /// element lands at `idx`. Missing that distinction silently left
    /// the relocated element's position stale (the mass-leave panic the
    /// supervision round surfaced).
    fn roster_remove(&mut self, player: &PlayerId) {
        if let Some(idx) = self.roster_pos.remove(player) {
            let relocated = *self.roster.last().expect("pos entry implies non-empty roster");
            self.roster.swap_remove(idx);
            if relocated != *player {
                // `insert`, not `get_mut`: the relocated element's entry
                // exists while the invariant holds, and rewriting it
                // keeps this function correct even under partial drift.
                self.roster_pos.insert(relocated, idx);
            }
        }
    }

    // NOTE (Faz 2): the old `roster_rekey` is GONE — with the roster keyed
    // by stable PlayerId a resume does not touch it AT ALL (no rename, no
    // reorder; the rotation cursor keeps its meaning trivially). The
    // re-key surface of a resume is exactly the binding move — see the
    // enumeration in `rebind_session`.

    fn handle_control(&mut self, c: RoomControl) -> bool {
        match c {
            RoomControl::Join { conn, out, reply } => {
                self.admit_fresh(conn, out, reply)
            }
            RoomControl::Leave { conn, entity } => {
                // Stale-leave guard: resolve the session through the
                // binding, then only the entity this player currently
                // owns. An unbound conn (a shard/room that never knew it,
                // or an already-torn-down session) is a no-op.
                if let Some(&player) = self.binding.get(&conn)
                    && self.conns.get(&player).map(|c| c.entity) == Some(entity)
                {
                    self.despawn_conn(player, true);
                    debug!(room = %self.config.id, %conn, %player, entity, "player left");
                }
                true
            }
            RoomControl::Detach { conn, entity, identity } => {
                // Transport death (registry `ConnClosed` route): the POLICY
                // is the logic's (§3 — the registry only reports the fact).
                // Same stale guard as `Leave`: binding first, then the
                // entity this player currently owns.
                if let Some(&player) = self.binding.get(&conn)
                    && self.conns.get(&player).map(|c| c.entity) == Some(entity)
                {
                    let decision =
                        self.logic.on_disconnect(&mut self.world, player, &identity);
                    match decision {
                        Detach::Despawn => {
                            // Byte-for-byte the old close semantics.
                            self.despawn_conn(player, false);
                        }
                        Detach::Hold { grace, to } => {
                            // Park it: keep the row (under its STABLE
                            // player key — nothing is re-keyed), the
                            // entity, the world state, the group membership
                            // AND the cap slot (§4 — members accounting
                            // does not drop). The binding row stays too:
                            // the parked row still belongs to that (dead)
                            // session until a resume re-points it. The
                            // clock is CORE-owned (§14.4): the grace is
                            // written here as an absolute deadline and the
                            // CONTROL sweep below fires it.
                            let rc = self.conns.get_mut(&player).expect("guarded above");
                            rc.detached = true;
                            rc.expire_to = to;
                            rc.detach_deadline = grace.map(|g| Instant::now() + g);
                            // Today's leave semantics for in-flight work
                            // (§11 "RPC pending detach anında"): pending
                            // requests drop, late reports are silently
                            // discarded (structural already), queued answers
                            // for the dead session go.
                            self.drop_conn_request_state(conn);
                            debug!(
                                room = %self.config.id,
                                %conn,
                                %player,
                                entity,
                                ?grace,
                                ?to,
                                "player detached (entity parked)"
                            );
                        }
                    }
                }
                true
            }
            RoomControl::Resume {
                conn,
                epoch,
                identity,
                out,
                reply,
            } => {
                // The implicit resume attempt (§14.3): ledger first, fresh
                // join as the transparent fallback (§5). An empty identity
                // never resumes (nothing to look up; the local-auth demo
                // may still send names, an anonymous client cannot).
                if identity.is_empty() {
                    return self.admit_fresh(conn, out, reply);
                }
                match self.logic.resume_lookup(&self.world, &identity) {
                    ResumeFound::Held(player) => {
                        // The parked row, by its STABLE key: one lookup
                        // (the ledger rides the player state, §14.2, so it
                        // answers with the id the table is keyed by — the
                        // pre-Faz-2 linear scan over the detached subset
                        // is gone).
                        let Some(rc) = self.conns.get(&player) else {
                            // Ledger says held but the table lost the row
                            // (a logic bug, or the hold expired in this
                            // very tick's sweep above): treat as ended.
                            self.m.resume_rejected_stale += 1;
                            debug!(
                                room = %self.config.id,
                                %identity,
                                %player,
                                "resume rejected: ledger holds a row the table lost"
                            );
                            return self.admit_fresh(conn, out, reply);
                        };
                        if !rc.detached {
                            // The player's row is LIVE (a double session of
                            // an identity the ledger somehow still holds):
                            // same divergence posture as above.
                            self.m.resume_rejected_stale += 1;
                            debug!(
                                room = %self.config.id,
                                %identity,
                                %player,
                                "resume rejected: ledger holds a live row"
                            );
                            return self.admit_fresh(conn, out, reply);
                        }
                        // Epoch guard (§7): one integer comparison rejects a
                        // delayed duplicate/replay AFTER a newer session
                        // already took the park over. Without it the loser of
                        // two racing resumes could fresh-join a SECOND entity
                        // for one identity. `0` disables the guard (hand-built
                        // calls); the ledger consumption itself stays
                        // exactly-once regardless — this actor is
                        // single-threaded.
                        if epoch != 0
                            && rc.session_epoch != 0
                            && epoch <= rc.session_epoch
                        {
                            self.m.resume_rejected_stale += 1;
                            warn!(
                                room = %self.config.id,
                                %conn,
                                %player,
                                %identity,
                                resume_epoch = epoch,
                                "resume rejected: stale epoch (a newer session \
                                 already rebound this park)"
                            );
                            let _ = reply.send(Err(CoreError::ResumeStale));
                            return true;
                        }
                        self.rebind_session(player, conn, epoch, identity, out, reply);
                        true
                    }
                    ResumeFound::Ended => {
                        // The hold already ended (expired/consumed/
                        // superseded): the RESUME mechanism rejects (counted),
                        // while the client-visible outcome stays the
                        // transparent fresh join of §5 — no waiting endpoint,
                        // no error frame; the TOCTOU rule ("whichever branch
                        // lands first wins, both are valid") covers exactly
                        // this race.
                        self.m.resume_rejected_stale += 1;
                        debug!(
                            room = %self.config.id,
                            %identity,
                            "resume rejected stale (hold ended); falling back \
                             to a fresh join"
                        );
                        self.admit_fresh(conn, out, reply)
                    }
                    ResumeFound::Never => self.admit_fresh(conn, out, reply),
                }
            }
            RoomControl::Shutdown => false,
        }
    }

    /// Admit a connection as a FRESH member: the exact body of the
    /// pre-reconnect `Join` arm (supersede own stale state, cap check,
    /// `on_join`, register, roster, reply) — now shared by the plain
    /// `Join` arm and the resume fallback paths, so the fallback can
    /// never drift from an ordinary join.
    fn admit_fresh(
        &mut self,
        conn: ConnectionId,
        out: mpsc::Sender<FrameBatch>,
        reply: oneshot::Sender<Result<(EntityId, Mailbox<Action>), CoreError>>,
    ) -> bool {
        // A join supersedes any stale state this connection had
        // (e.g. a leave queued behind it in the control channel) —
        // including its request state (a rejoin is a new session:
        // in-flight requests and queued answers of the old one
        // are dropped, and their late reports are discarded).
        if let Some(&stale) = self.binding.get(&conn) {
            let rc = self.conns.remove(&stale).expect("binding implies row");
            self.roster_remove(&stale);
            self.drop_conn_request_state(conn);
            self.logic.on_leave(&mut self.world, stale);
            drop(rc);
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
        // The LOGIC mints the stable player identity here (Faz 2) — core
        // never invents player ids.
        let admission = self.logic.on_join(&mut self.world, conn);
        self.m.joins += 1;
        let (act_tx, act_rx) = mpsc::channel(self.config.action_capacity);
        self.binding.insert(conn, admission.player);
        self.conns.insert(
            admission.player,
            RoomConn {
                conn,
                out,
                actions: act_rx,
                entity: admission.entity,
                // Authoritative value is recomputed every broadcast
                // phase (a group may depend on the world); this is
                // the join-time value.
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
        self.roster_add(admission.player);
        debug!(
            room = %self.config.id,
            %conn,
            player = %admission.player,
            entity = admission.entity,
            "player joined"
        );
        true
    }

    /// Bind a resumed session onto its parked row: swap the channel
    /// halves (§7), stamp the guard epoch, move THE binding row, and hand
    /// the logic its `on_resume` hook (ledger consumption + seq/ack
    /// reset + fresh-member mark). The wire id does not move: the reply
    /// carries the SAME entity id the original join returned (§5).
    ///
    /// **The RebindKey shrink (Faz 2).** Pre-Faz-2 this function ran a
    /// single-pass rename over every conn-keyed table (`conns`, `roster`,
    /// `roster_pos`) — §14.1's one-point discipline against the
    /// "new-table-added-later-and-forgotten" bug. With every table keyed
    /// by the stable [`PlayerId`] the rename class is GONE: a resume now
    /// updates exactly ONE row of ONE table — the binding — plus the
    /// channel halves inside the (unmoved) `conns` row. The signpost
    /// enumeration survives as the proof that nothing else was missed;
    /// today it is a list of tables that must NOT be touched:
    ///
    ///   1. `binding` — MOVED below (`old conn → new conn`, same player):
    ///      the entire re-key surface.
    ///   2. `conns` — NOT re-keyed: the row's key IS the stable player
    ///      id; only `RoomConn.conn` (its bound session back-reference)
    ///      and the channel halves are rewritten.
    ///   3. `roster` / `roster_pos` / `read_cursor` — NOT touched:
    ///      player-keyed; the parked row kept its slot in the READ
    ///      rotation the whole time (it simply had nothing to pull).
    ///   4. `pending` / `queued` — NOT re-keyed AND not moved: they stay
    ///      conn-keyed BY DESIGN (session-scoped RPC state, §11) and were
    ///      already cleared at DETACH time; listed here as CHECKED.
    ///   5. `groups` — NOT touched: `G` is opaque (it MAY embed a
    ///      per-player key) and cannot be derived generically. Safe
    ///      because the broadcast phase rebuilds the whole group table
    ///      from `conns` every tick (phase 4b): a changed group key
    ///      self-heals within one tick at the cost of one extra emission
    ///      for that group (its cached snapshot ledger is unreachable
    ///      under the old key and is dropped with it).
    ///
    /// Anything the LOGIC keys per-session goes through
    /// [`GameLogic::on_resume`] — which under Faz 2 is nearly empty for
    /// a player-keyed logic (ledger + seq reset only).
    fn rebind_session(
        &mut self,
        player: PlayerId,
        conn: ConnectionId,
        epoch: u64,
        identity: String,
        out: mpsc::Sender<FrameBatch>,
        reply: oneshot::Sender<Result<(EntityId, Mailbox<Action>), CoreError>>,
    ) {
        // Fresh input channel for the fresh session (the old channel's
        // senders died with the old connection actor); the seq/ack
        // contract (DESIGN §14.2) makes the NEW session start from a
        // clean numbering, so the old channel object is dropped, not
        // reused.
        let (act_tx, act_rx) = mpsc::channel(self.config.action_capacity);
        let (entity, old_conn) = {
            let rc = self.conns.get_mut(&player).expect("parked row checked by caller");
            rc.out = out;
            rc.actions = act_rx;
            rc.detached = false;
            rc.bot_fed = false;
            rc.detach_deadline = None;
            rc.session_epoch = epoch;
            let old = rc.conn;
            rc.conn = conn;
            (rc.entity, old)
        };
        self.m.resumes += 1;
        // THE binding move — the whole remaining re-key surface (see the
        // enumeration above). The old session's row is removed first so a
        // stray frame under the dead conn finds no binding from here on
        // (locked by `actions_from_the_old_session_are_dropped_after_resume`).
        self.binding.remove(&old_conn);
        self.binding.insert(conn, player);
        self.logic
            .on_resume(&mut self.world, &identity, conn, player, entity);
        let _ = reply.send(Ok((entity, act_tx)));
        debug!(
            room = %self.config.id,
            %old_conn,
            %conn,
            %player,
            entity,
            epoch,
            "player resumed onto parked row (binding moved; tables untouched)"
        );
    }

    fn despawn_conn(&mut self, player: PlayerId, count_as_leave: bool) {
        let Some(rc) = self.conns.remove(&player) else {
            return;
        };
        // Tear the session binding down with the row (the leave/detach-
        // expiry/despawn half of the binding lifecycle).
        self.binding.remove(&rc.conn);
        self.roster_remove(&player);
        // The request state goes with the SESSION: in-flight requests
        // are released (their slots free up for other connections) and
        // any queued answer is dropped (a reply to a gone session is not
        // delivered).
        self.drop_conn_request_state(rc.conn);
        self.logic.on_leave(&mut self.world, player);
        if count_as_leave {
            self.m.leaves += 1;
        }
    }
}

#[cfg(test)]
mod tests;
