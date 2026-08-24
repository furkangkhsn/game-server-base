//! The registry actor: the server's control plane.
//!
//! A single channel-driven actor that owns:
//! - the room table: `RoomId → control mailbox`;
//! - the connection table: `ConnectionId → ConnInfo` (kept for the
//!   connection's *whole* lifetime, so the notification path — the inbox —
//!   is never lost mid-session);
//! - the relationship dispatchers: one small task per connection that has
//!   a room relationship in flight, serializing that connection's
//!   join/leave operations (see [`RoomOp`]);
//! - the [`Ticker`] handle (global tick broadcast + rate), which rooms
//!   subscribe to at creation;
//! - **sharded rooms** (see [`crate::shard`]): one logical room can be
//!   backed by N shard actors (disjoint spatial regions, each its own
//!   task). The registry spawns the shards for one [`RoomId`], routes
//!   each join to the home shard (the factory's pure `home_shard`
//!   router), enforces the room's membership cap for sharded rooms
//!   (it is the only actor that sees every join — `ShardGroup`), and
//!   broadcasts leaves to all shards (the entity-id guard in exactly one
//!   of them matches). The registry never awaits a shard: its only
//!   interaction with the room side is channel sends, exactly as with a
//!   single room.
//! - the [`RoomFactory`], which is how the (game-specific) room logic gets
//!   into the core without the core knowing any game types.
//! - **supervision** (the death watch): every spawned room/shard task gets
//!   ONE watcher task that awaits only that task's `JoinHandle` and reports
//!   [`RegistryMsg::RoomDied`] through the registry's own mailbox. A panic
//!   in game logic (inside `logic.update()`) kills a room's task silently;
//!   without the watcher the table kept answering `Running { members }`
//!   forever while joins vanished into a dead control channel. The watcher
//!   adds no multiplexing: it is the same "one task per source, awaiting a
//!   single receive" idiom as the signal handler and the relationship
//!   dispatchers below.
//!
//! No locks: every cross-actor value (mailboxes, one-shot replies) is moved
//! through channels. In particular the registry **never awaits a room**:
//! `SpawnPlayer` hands the room round-trip to the connection's dispatcher
//! and returns immediately, so one slow room can never block the control
//! plane (joins elsewhere, room creation, shutdown).
//!
//! Room lifecycle is channel-driven: creating a room is a `subscribe` on
//! the ticker plus a control channel; destroying one removes the table
//! entry and sends a control `Shutdown` (processed on the room's next
//! tick) — there is no cancellation plumbing anywhere, not even in
//! supervision: the death watch only *observes* task exits, it never causes
//! them.

use std::collections::{HashMap, VecDeque};
use std::fmt::Debug;
use std::hash::Hash;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use crate::channel::{FrameBatch, Inbox, Mailbox, channel};
use crate::conn::ConnIn;
use crate::error::CoreError;
use crate::id::{ConnectionId, EntityId, RoomId};
use crate::metrics::{MetricsEvent, RegistrySample};
use crate::room::{Action, RoomActor, RoomConfig, RoomControl, RoomLogic};
use crate::shard::{ShardActor, ShardLogic, ShardMsg};
use crate::ticker::Ticker;

/// The retirement set's eviction cap (see the `retired` field): 65 536
/// ended-room ids — far beyond any realistic matchmaker churn for one
/// process lifetime, small enough to be a fixed bound rather than a
/// growing table.
const RETIRED_SET_CAP: usize = 65_536;

/// One shard of a sharded room: its `World` + its [`ShardLogic`].
pub type Shard<W, G, St> = (W, Box<dyn ShardLogic<W, GroupKey = G, State = St>>);

/// The outcome of a room factory: one room actor (the pre-sharding shape)
/// or a **sharded room** — N shard actors forming one logical room (see
/// [`crate::shard`]).
///
/// `St` is the sharded room's migration state type
/// ([`crate::shard::ShardLogic::State`]); it is unused by the
/// [`BuiltRoom::Single`] arm (a single-room-only factory can pick any
/// `St`, e.g. `()`).
pub enum BuiltRoom<W, G, St> {
    /// One room actor (today's shape).
    Single {
        world: W,
        logic: Box<dyn RoomLogic<W, GroupKey = G>>,
    },
    /// N shard actors (indices `0..N`, the vec order) forming one logical
    /// room. `home_shard` maps a joining connection to the shard that owns
    /// its spawn point — pure and synchronous (the registry calls it at
    /// join dispatch and never awaits it). A misrouted join self-heals:
    /// the entity's first boundary crossing migrates it to the right
    /// shard (at most one tick of cross-boundary staleness).
    Sharded {
        shards: Vec<Shard<W, G, St>>,
        home_shard: Arc<dyn Fn(ConnectionId) -> usize + Send + Sync>,
    },
}

/// Builds a room's world + logic. Provided by the composition root; the core
/// never names the concrete game types. `G` is the game logic's group key
/// (`RoomLogic::GroupKey` / `ShardLogic::GroupKey`); the room stores
/// per-group state under it. `St` is the sharded room's migration state
/// (see [`BuiltRoom`]).
pub type RoomFactory<W, G, St> =
    Arc<dyn Fn(RoomId, &RoomConfig) -> BuiltRoom<W, G, St> + Send + Sync>;

/// A room's status, as known to the registry's table (the control plane's
/// vocabulary — see [`RegistryMsg::RoomStatus`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomStatus {
    /// The room is running (a `Shutdown` has not been accepted for it).
    /// `members` is the registry-side count of connections affiliated
    /// with the room (the same source the room-cap enforcement reads).
    Running { members: u32 },
    /// The room existed and its shutdown was just accepted by this
    /// message (the room actor stops on its next tick).
    Destroyed,
    /// The room is not known to the registry (never created, or already
    /// destroyed).
    Absent,
}

/// The match result the room reports when it shuts down (the control
/// plane's result seam — see [`crate::room::RoomLogic::match_result`]):
/// the room id plus the game-encoded payload (opaque to the core; the
/// platform's adapter decodes it).
#[derive(Debug)]
pub struct MatchResult {
    pub room: RoomId,
    pub payload: bytes::Bytes,
}

/// Messages addressed to the registry actor.
#[derive(Debug)]
pub enum RegistryMsg {
    /// Create and start a room.
    ///
    /// **Idempotent** (the control plane may resend the same request —
    /// retry-after-timeout is the normal pattern for a control plane):
    ///
    /// - room absent → created (the usual validation: tick rate,
    ///   keep-alive rate) and the reply is `Ok(Running { members: 0 })`;
    /// - room present with the IDENTICAL config → no-op, the reply is
    ///   `Ok(Running { members })` (one room, not two);
    /// - room present with a DIFFERENT config → `Err(RoomConflict)` —
    ///   a different spec is not a retry, it is a contradiction, and
    ///   silently accepting it would start a room the control plane did
    ///   not ask for.
    ///
    /// The comparison is over the WHOLE `RoomConfig` (it is `PartialEq`
    /// for this): a retry carries the same config by construction. The
    /// reply carries the room's status (see [`RoomStatus`]) so a create
    /// round trip is also a status query (one hop, not two).
    CreateRoom {
        config: RoomConfig,
        reply: oneshot::Sender<Result<RoomStatus, CoreError>>,
    },
    /// Shut down a room (its players' entities are dropped; connections are
    /// notified via [`ConnIn::RoomGone`]).
    ///
    /// Idempotent: destroying an absent room is a no-op and the reply is
    /// `Ok(Absent)` (a control plane that retries a destroy never sees an
    /// error for the second attempt). The reply is `Ok(Destroyed)` when
    /// the shutdown was issued — note the room actor processes the
    /// `Shutdown` on its next tick, so between the reply and the actual
    /// stop a status query may already report `Absent` (the table entry
    /// is removed when the destroy is ACCEPTED, which is the
    /// control-plane-relevant moment: no new joins can start).
    DestroyRoom {
        id: RoomId,
        reply: oneshot::Sender<RoomStatus>,
    },
    /// Query a room's status from the registry's table (no room round
    /// trip — the registry never awaits a room; the member count comes
    /// from the connection table, which the registry maintains from the
    /// join/leave reports of every room, sharded or single).
    RoomStatus {
        id: RoomId,
        reply: oneshot::Sender<RoomStatus>,
    },
    /// Spawn a player entity in a room and report the entity + the
    /// per-connection action channel the connection actor writes to.
    ///
    /// Non-blocking with respect to the room: the round-trip is dispatched
    /// to the connection's relationship task and the reply may arrive
    /// later (at the room's next tick boundary). A slow room can therefore
    /// never stall the registry.
    SpawnPlayer {
        conn: ConnectionId,
        room: RoomId,
        /// The connection's outbound channel, handed to the room for fan-out.
        out: mpsc::Sender<FrameBatch>,
        /// The resume key (`ValidatedTicket.player`, or `Auth.name` on the
        /// local-auth path — demo-only there). Empty = anonymous: an
        /// ordinary fresh join, no ledger lookup, no supersedence. A
        /// NON-empty identity makes this join the implicit resume attempt
        /// of §14.3 (the room falls back to a fresh join transparently
        /// when its ledger does not hold the identity).
        identity: String,
        reply: oneshot::Sender<Result<(EntityId, Mailbox<Action>), CoreError>>,
    },
    /// Remove a player from its room (voluntary leave). The connection
    /// stays registered (it may rejoin).
    DespawnPlayer { conn: ConnectionId },
    /// A connection registered itself so the registry can notify it later.
    ConnOpened {
        conn: ConnectionId,
        inbox: Mailbox<ConnIn>,
    },
    /// A connection went away for good; the registry removes its entry and
    /// makes sure its room-side entity is cleaned up.
    ConnClosed { conn: ConnectionId },
    /// Shut everything down: notify connections, destroy all rooms.
    Shutdown,

    // -- internal: reported by relationship dispatcher tasks ----------------
    /// A dispatched join completed: record the affiliation.
    ///
    /// `generation` is the room incarnation the join was dispatched
    /// against (stamped by the registry, echoed by the dispatcher): a
    /// mismatch with the current entry means the room died (or was
    /// replaced) between dispatch and settlement — the affiliation must
    /// NOT be recorded (see the handler; supervision).
    SpawnDone {
        conn: ConnectionId,
        room: RoomId,
        entity: EntityId,
        generation: u64,
    },
    /// A dispatched join was rejected by the room (e.g. the shard's
    /// wire-id range is exhausted): release the capacity reservation.
    /// The reservation is released only when `generation` still matches
    /// (a stale failure must not touch a rebuilt room's counters).
    SpawnFailed {
        conn: ConnectionId,
        room: RoomId,
        generation: u64,
    },
    /// A dispatched leave completed: clear the affiliation.
    LeaveDone { conn: ConnectionId, room: RoomId },
    /// A connection's transport died and its DETACH was delivered to the
    /// room (the dispatcher's `Close` now reports this instead of
    /// `LeaveDone`): the affiliation is KEPT but marked detached — the
    /// parked entity still holds its cap slot (§4), so the registry's
    /// member view must not drop either. The slot is released later: by a
    /// resume that re-affiliates the identity (SpawnDone cleanup), by the
    /// room-side expiry reflected through a fresh SpawnDone for another
    /// member of the room... never automatically — see the accepted v1
    /// imprecision documented on the `ConnClosed` handler.
    DetachDone { conn: ConnectionId, room: RoomId },
    /// A connection's dispatcher task exited; drop its slot.
    OpsClosed { conn: ConnectionId },
    /// Internal: reported by a room/shard death watcher (see
    /// [`Self::spawn_room_watcher`]) when the watched actor task has ended
    /// — by panic or by any normal exit (`DestroyRoom`, server
    /// `Shutdown`). The registry answers it with one table lookup, and the
    /// `generation` is what makes that lookup decisive: an entry of a
    /// DIFFERENT generation (destroyed and even re-created in the
    /// meantime) or no entry at all means the report is stale → silent
    /// no-op. Only a LIVE entry of the SAME incarnation is an *unexpected*
    /// death: reap it like a destroy (members notified, affiliations
    /// cleared) and, per [`RoomConfig::restart_on_panic`], rebuild.
    RoomDied {
        id: RoomId,
        /// The dead task's shard index (sharded rooms only).
        shard: Option<usize>,
        /// The incarnation this watcher was spawned for (the stale-watch
        /// guard; see [`Registry::install_room`]).
        generation: u64,
    },
}

/// How a connection's room-relationship ops reach the room side: the
/// single room's control channel, or a sharded room's shard mailboxes.
/// `Clone` because the registry hands a copy to each dispatcher op.
/// `St` is the sharded room's migration state (see [`BuiltRoom`]).
#[derive(Clone)]
enum RoomHandle<St> {
    /// A single room: one control mailbox.
    Single(Mailbox<RoomControl>),
    /// A sharded room: one mailbox per shard (indices = shard indices).
    /// `Join` goes to the home shard (the registry picked it via
    /// `home_shard`); `Leave`/`Shutdown` go to ALL of them (exactly one
    /// shard owns the connection — the entity-id guard makes the others
    /// no-ops; see `crate::shard`, "Connection ownership").
    Sharded(Vec<Mailbox<ShardMsg<St>>>),
}

/// Operations on a connection's room relationship, processed by that
/// connection's dispatcher task — **in order**, which is what makes
/// leave→rejoin race-free: a `Leave` can never overtake (or be overtaken
/// by) the `Join` it follows.
enum RoomOp<St> {
    /// Join `room`: round-trip the control `Join` (the home shard, when the
    /// room is sharded — `shard` carries the registry's pick), reply to
    /// the connection actor (with the per-connection action channel),
    /// report [`RegistryMsg::SpawnDone`] to the registry.
    ///
    /// A NON-empty `identity` turns this op into the implicit resume
    /// attempt of §14.3: the dispatcher sends `Resume` instead of `Join`
    /// (broadcast to every shard — §6 — for sharded rooms). When nothing
    /// accepts (no ledger holds the identity anywhere), the dispatcher
    /// falls back to the ordinary join below — transparently to the
    /// client (§5).
    Join {
        room: RoomId,
        handle: RoomHandle<St>,
        /// The home shard index (sharded rooms only).
        shard: Option<usize>,
        /// The room incarnation this handle was taken from (stamped by the
        /// registry; echoed back on SpawnDone/SpawnFailed so a settled join
        /// of a dead incarnation can be recognized — supervision, see
        /// `RegistryMsg::RoomDied`).
        generation: u64,
        /// The join's guard epoch, minted GLOBALLY by the registry (one
        /// monotonically increasing counter across ALL connections). A
        /// per-connection counter resets to 1 on every new session, so a
        /// parked row from the previous session (`session_epoch = k`)
        /// rejected the k-th reconnect's first resume as stale and cost a
        /// wasted round trip per reconnect (measured: one rejection for
        /// EVERY resume beyond an identity's first). Global minting keeps
        /// each connection's epochs a strictly increasing subsequence AND
        /// makes every new session strictly newer than every parked row —
        /// the §7 single-comparison guarantee holds on first attempt.
        epoch: u64,
        out: mpsc::Sender<FrameBatch>,
        /// The resume key; empty = anonymous plain join.
        identity: String,
        reply: oneshot::Sender<Result<(EntityId, Mailbox<Action>), CoreError>>,
    },
    /// Leave `room`: send control `Leave` (with the entity this dispatcher
    /// saw the join create), then report [`RegistryMsg::LeaveDone`].
    Leave { room: RoomId },
    /// Drain the queue (processing whatever is left, including a final
    /// leave), report [`RegistryMsg::OpsClosed`], exit.
    Close,
}

/// The folded outcome of one dispatched join/resume round trip (see
/// [`Self::dispatch_plain_join`] / [`Self::dispatch_resume`]).
enum OpOutcome {
    Joined(EntityId, Mailbox<Action>),
    /// A structural rejection from the room side (full room, stale-resume
    /// epoch guard): propagated to the connection actor as-is.
    Rejected(CoreError),
    /// The room side is unreachable or dropped the reply.
    Gone,
}

/// A sharded room's registry-side state (see `crate::shard`): the shard
/// mailboxes, the home-shard router, and the room-level capacity
/// accounting (the registry is the only actor that sees every join, so
/// it enforces the room cap for sharded rooms — a shard cannot count the
/// room without shared state).
#[derive(Clone)]
struct ShardGroup<St> {
    /// One mailbox per shard index (the registry's own senders; the
    /// shards' neighbors hold clones of the same channels).
    mailboxes: Vec<Mailbox<ShardMsg<St>>>,
    /// Maps a joining connection to its home shard (pure; see
    /// [`BuiltRoom::Sharded`]).
    home: Arc<dyn Fn(ConnectionId) -> usize + Send + Sync>,
    /// The room's membership cap (`RoomConfig::max_players`, `None` =
    /// unlimited).
    cap: Option<u64>,
    /// Connections accepted (SpawnDone, deduplicated per connection).
    members: u64,
    /// Joins dispatched but not yet settled (SpawnDone/SpawnFailed):
    /// reserved against the cap so a concurrent join burst cannot race
    /// past it.
    pending: u64,
}

struct RoomEntry<St> {
    /// The single room's control mailbox (single rooms only).
    control: Option<Mailbox<RoomControl>>,
    /// The sharded room's state (sharded rooms only).
    shards: Option<ShardGroup<St>>,
    /// The config the room was created with (the idempotent-create
    /// comparison: a resent create must match it EXACTLY to be a no-op —
    /// see `RegistryMsg::CreateRoom`).
    config: RoomConfig,
    /// Which incarnation of this room id this entry is (0 = first create,
    /// +1 per rebuild — see [`Registry::room_gen`]). Copied into each
    /// death watcher so a late death report can be attributed to its own
    /// incarnation and a stale one rejected (see
    /// `RegistryMsg::RoomDied`).
    generation: u64,
}

#[derive(Default)]
struct ConnInfo {
    room: Option<RoomId>,
    entity: Option<EntityId>,
    /// Set via [`RegistryMsg::ConnOpened`]; kept for the connection's whole
    /// lifetime so `RoomGone`/`Shutdown` can always reach it.
    inbox: Option<Mailbox<ConnIn>>,
    /// The resume key this connection joined with (see
    /// [`RegistryMsg::SpawnPlayer::identity`]); empty = anonymous.
    /// Recorded at dispatch so the resume re-affiliation can find and
    /// release the detached entry it supersedes.
    identity: String,
    /// The transport died but the entity is parked room-side: the
    /// affiliation is kept (slot held, §4) with this mark. A resumed or
    /// fresh session for the same identity releases the entry; a room
    /// destroy/death clears it like any affiliation. Accepted v1
    /// imprecision: a hold that expires WITHOUT the identity ever
    /// returning leaves this entry until one of those events — the
    /// registry-side count errs CONSERVATIVELY (it overcounts members, so
    /// caps close slightly early instead of ever letting a room
    /// overfill), and the room side — which enforces its own cap against
    /// its own table — is exact.
    detached: bool,
}

/// The registry actor. `W`/`G` are the room's world / group-key types;
/// `St` is the sharded room's migration state (unused by single rooms —
/// see [`BuiltRoom`]).
pub struct Registry<W, G, St> {
    factory: RoomFactory<W, G, St>,
    inbox: Inbox<RegistryMsg>,
    /// Sender half of our own mailbox: cloned to dispatcher tasks so they
    /// can report back.
    self_mailbox: Mailbox<RegistryMsg>,
    rooms: HashMap<RoomId, RoomEntry<St>>,
    /// Per-room-id incarnation counter: bumped on every install (first
    /// create AND each panic rebuild). A room id can outlive several
    /// incarnations (destroy → re-create, death → restart); each death
    /// watcher carries its own incarnation's number, so a late report from
    /// a dead-and-replaced room can never reap the wrong entry — no
    /// cancellation plumbing, just one integer comparison at report time.
    room_gen: HashMap<RoomId, u64>,
    conns: HashMap<ConnectionId, ConnInfo>,
    conn_ops: HashMap<ConnectionId, mpsc::Sender<RoomOp<St>>>,
    ticker: Ticker,
    /// Local control-plane counters (flushed as a sample whenever a table
    /// changes — event-driven; no timer, no new await; see
    /// [`crate::metrics`]).
    reg_created: u64,
    reg_destroyed: u64,
    /// Rooms that died UNEXPECTEDLY (a panicked room or shard task; one
    /// dead shard counts once), cumulative — see `RegistryMsg::RoomDied`.
    /// A destroy never increments this; a rebuild after a death does not
    /// increment `reg_created` (the rebirth is not a control-plane create).
    reg_died: u64,
    reg_joins: u64,
    reg_leaves: u64,
    reg_opens: u64,
    reg_closes: u64,
    /// Metric samples dropped on a full (bounded) metrics channel,
    /// cumulative.
    reg_metrics_dropped: u64,
    /// Global monotonic join-epoch counter, minted here at dispatch (see
    /// the `RoomOp::Join::epoch` doc: per-connection counters made every
    /// resume after an identity's first trip the staleness guard once).
    next_join_epoch: u64,
    /// Outbound metrics path (bounded channel; the registry sends with the
    /// synchronous `try_send` — no await).
    metrics: mpsc::Sender<MetricsEvent>,
    /// Server-wide connection cap (the accept loop's guardrail, enforced
    /// where the connection *count* lives — the registry's table, not the
    /// accept loop's local state, because the accept loop cannot observe
    /// disconnects without a second awaited source). `None` = unlimited.
    /// A rejected connection is never recorded (no table entry, no
    /// `reg_opens`) and is told to close itself via
    /// [`ConnIn::ServerClosed`] (an `ERROR` frame, code 9, then EOF).
    max_connections: Option<u64>,
    /// The match-result sink (the control plane's result seam, see
    /// [`crate::room::RoomLogic::match_result`]): a bounded mailbox the
    /// composition root reads from (its reference adapter). Cloned to
    /// each room at creation; `None` = rooms report no result. The
    /// registry never awaits the sink (it only holds a sender clone).
    result_sink: Option<Mailbox<MatchResult>>,
    /// RETIRED room ids (§8): ids whose room ENDED in this process — an
    /// accepted `DestroyRoom` (an ended match, or a persistent room's
    /// decommissioning), or an unexpected death left unrebuilt. Joins and
    /// resumes answer ERROR 12 ([`CoreError::RoomRetired`], "do not retry
    /// — return to the lobby") instead of 4, because the client decision
    /// differs: an ended match must never silently reopen, and an
    /// operator's decommissioning intent must never be crushed by
    /// automatic re-creation.
    ///
    /// Bounded by [`RETIRED_SET_CAP`] with FIFO eviction of the oldest
    /// retirement: the set is operator-scale by nature (rooms end at
    /// human/matchmaker pace), and the cap keeps the discipline that no
    /// table grows without bound; an id evicted after extreme churn
    /// degrades safely to the pre-reconnect answer (4).
    retired: HashMap<RoomId, ()>,
    /// FIFO order for the eviction cap above.
    retired_order: VecDeque<RoomId>,
}

impl<W, G, St> Registry<W, G, St>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
    St: Debug + Send + 'static,
{
    pub fn new(
        inbox: Inbox<RegistryMsg>,
        self_mailbox: Mailbox<RegistryMsg>,
        factory: RoomFactory<W, G, St>,
        ticker: Ticker,
        // Outbound metrics path (see `crate::metrics`): a bounded channel;
        // the registry sends with the synchronous `try_send` (no await).
        metrics: mpsc::Sender<MetricsEvent>,
        // Server-wide connection cap (`None` = unlimited; see the field).
        max_connections: Option<u64>,
        // The match-result sink (see the field): `None` = no result seam.
        result_sink: Option<Mailbox<MatchResult>>,
    ) -> Self {
        Self {
            factory,
            inbox,
            self_mailbox,
            rooms: HashMap::new(),
            room_gen: HashMap::new(),
            conns: HashMap::new(),
            conn_ops: HashMap::new(),
            ticker,
            reg_created: 0,
            reg_destroyed: 0,
            reg_died: 0,
            reg_joins: 0,
            reg_leaves: 0,
            reg_opens: 0,
            reg_closes: 0,
            reg_metrics_dropped: 0,
            next_join_epoch: 0,
            metrics,
            max_connections,
            result_sink,
            retired: HashMap::new(),
            retired_order: VecDeque::new(),
        }
    }

    /// Flush the registry's local counters as a sample. Synchronous
    /// `try_send` on the bounded metrics channel (A3): the registry's await
    /// set is unchanged (its only await stays the mailbox `recv`), and a
    /// full channel drops + counts the sample (harmless — the counters are
    /// cumulative, so the next flush carries everything).
    fn emit_metrics(&mut self) {
        let sample = RegistrySample {
            rooms: self.rooms.len() as u32,
            conns: self.conns.len() as u32,
            rooms_created: self.reg_created,
            rooms_destroyed: self.reg_destroyed,
            rooms_died: self.reg_died,
            joins: self.reg_joins,
            leaves: self.reg_leaves,
            opens: self.reg_opens,
            closes: self.reg_closes,
            metrics_dropped: self.reg_metrics_dropped,
        };
        if let Err(mpsc::error::TrySendError::Full(_)) =
            self.metrics.try_send(MetricsEvent::Registry(sample))
        {
            self.reg_metrics_dropped += 1;
        }
    }

    /// Tell the collector a room's accumulator can go: sent at every
    /// point this actor removes a live table entry — an accepted
    /// [`RegistryMsg::DestroyRoom`], an unexpected-death reap, and the
    /// shutdown-all drain. Without it the collector kept a per-room
    /// accumulator for every room id ever created. A notice lost to a
    /// full channel leaves that one accumulator until process end — the
    /// same bounded-imprecision trade as `emit_metrics` above (the
    /// counters there are cumulative; here the cost is one stale entry,
    /// never a wrong number).
    fn emit_room_gone(&mut self, id: RoomId) {
        if let Err(mpsc::error::TrySendError::Full(_)) =
            self.metrics.try_send(MetricsEvent::RoomGone(id))
        {
            self.reg_metrics_dropped += 1;
        }
    }

    /// Mark a room id retired (§8), FIFO-capped (see the `retired` field).
    fn retire_room(&mut self, id: RoomId) {
        if self.retired.insert(id, ()).is_none() {
            self.retired_order.push_back(id);
            while self.retired_order.len() > RETIRED_SET_CAP {
                let evicted = self.retired_order.pop_front().expect("len checked above");
                self.retired.remove(&evicted);
            }
        }
    }

    /// Lift a retirement (the explicit-create override; see the
    /// `CreateRoom` handler). Order-vec pruning keeps the FIFO honest.
    fn unretire_room(&mut self, id: &RoomId) {
        self.retired.remove(id);
        self.retired_order.retain(|x| x != id);
    }

    /// Count the connections affiliated with `room` (the registry-side
    /// membership view — maintained from the join/leave reports of every
    /// room, sharded or single; the same source the sharded room-cap
    /// enforcement reads). O(connections): this is a control-plane query
    /// (rare, human-paced), not a tick-path operation.
    fn room_members(&self, room: RoomId) -> u32 {
        self.conns
            .values()
            .filter(|info| info.room == Some(room))
            .count() as u32
    }

    /// Install a freshly built room: spawn its actor task(s) plus ONE death
    /// watcher per task, and put this incarnation's table entry. This is
    /// THE single creation path — `CreateRoom` and a panic rebuild (see
    /// [`RegistryMsg::RoomDied`]) both wire their actors through here, so
    /// the two can never drift apart.
    ///
    /// The factory has already run by the time this is called, inside the
    /// registry loop (as on every create): room construction is synchronous
    /// game code, and a panicking FACTORY is out of scope for supervision —
    /// it would kill the registry itself, exactly as it does today. The
    /// watch covers the spawned tick-loop tasks, whose panics are contained
    /// by tokio's task boundary.
    fn install_room(&mut self, config: RoomConfig, built: BuiltRoom<W, G, St>, run_every: u64) {
        let id = config.id;
        // This incarnation's generation (0 = first create for this id,
        // +1 per rebuild/re-create): copied into every death watcher of
        // this install so a late report from an earlier incarnation can be
        // rejected at report time with one integer comparison (see
        // `RegistryMsg::RoomDied`) — no cancellation plumbing anywhere.
        let generation = {
            let slot = self.room_gen.entry(id).or_insert(0);
            let current = *slot;
            *slot = current + 1;
            current
        };
        match built {
            BuiltRoom::Single { world, logic } => {
                let (control_tx, control_rx) = channel(config.control_capacity);
                let handle = tokio::spawn(
                    RoomActor::new(
                        config.clone(),
                        world,
                        logic,
                        self.ticker.subscribe(),
                        control_rx,
                        run_every,
                        self.metrics.clone(),
                        self.result_sink.clone(),
                    )
                    .run(),
                );
                Self::spawn_room_watcher(
                    id,
                    None,
                    generation,
                    handle,
                    self.self_mailbox.clone(),
                );
                self.rooms.insert(
                    id,
                    RoomEntry {
                        control: Some(control_tx),
                        shards: None,
                        config,
                        generation,
                    },
                );
            }
            BuiltRoom::Sharded { shards, home_shard } => {
                let n = shards.len();
                debug_assert!(n >= 1, "a sharded room needs >= 1 shard");
                // One channel per shard: the registry keeps the original
                // sender, and every neighbor of the shard holds a CLONE of
                // it (tokio mpsc: many senders, one receiver). So each
                // shard's mailbox carries both the registry's control
                // messages and its neighbors' protocol messages
                // (Migrate/Border) — the shard actor's single `try_recv`
                // drain handles all of them.
                //
                // Pass 1 — one channel per shard (the registry keeps the
                // original sender; every neighbor holds a clone — tokio
                // mpsc: many senders, one receiver), and each actor's
                // `neighbors` vec (`txs[a][b]` = the sender shard a uses to
                // reach shard b, indexed by the receiver's index — the
                // actor indexes it that way; non-neighbor slots are
                // dummies, closed senders that are never sent to).
                let mut rxs: Vec<Inbox<ShardMsg<St>>> = Vec::with_capacity(n);
                let mut reg_txs = Vec::with_capacity(n);
                let (dummy_tx, _dummy_rx) = channel::<ShardMsg<St>>(1);
                for _ in 0..n {
                    let (tx, rx) = channel(config.control_capacity);
                    reg_txs.push(tx.clone());
                    rxs.push(rx);
                }
                let mut txs: Vec<Vec<Mailbox<ShardMsg<St>>>> = Vec::with_capacity(n);
                for a in 0..n {
                    let mut row = Vec::with_capacity(n);
                    for (b, tx_b) in reg_txs.iter().enumerate() {
                        // neighbors() is game knowledge (the grid
                        // topology); it is read after the factory built
                        // the logics.
                        let is_neighbor = shards
                            .get(a)
                            .map(|(_, l)| l.neighbors().contains(&b))
                            .unwrap_or(false);
                        row.push(if is_neighbor {
                            tx_b.clone()
                        } else {
                            dummy_tx.clone()
                        });
                    }
                    txs.push(row);
                }
                // Pass 2 — spawn the shard actors (indices = the vec order;
                // the sample id / neighbor slots rely on it) and one death
                // watcher per shard: ANY dead shard breaks the whole
                // logical room (its neighbors hold senders into its closed
                // mailbox, so cross-shard migration can never complete
                // again), which is exactly what the watcher will report.
                for (i, (world, logic)) in shards.into_iter().enumerate() {
                    // `rxs` was built in shard order (pass 1), so popping
                    // the front pairs each shard with its own receiver.
                    let rx = rxs.remove(0);
                    let handle = tokio::spawn(
                        ShardActor::new(
                            config.clone(),
                            i,
                            world,
                            logic,
                            self.ticker.subscribe(),
                            rx,
                            txs[i].clone(),
                            run_every,
                            self.metrics.clone(),
                        )
                        .run(),
                    );
                    Self::spawn_room_watcher(
                        id,
                        Some(i),
                        generation,
                        handle,
                        self.self_mailbox.clone(),
                    );
                }
                self.rooms.insert(
                    id,
                    RoomEntry {
                        control: None,
                        shards: Some(ShardGroup {
                            mailboxes: reg_txs,
                            home: home_shard,
                            cap: config.max_players.map(|c| c as u64),
                            members: 0,
                            pending: 0,
                        }),
                        config,
                        generation,
                    },
                );
                debug!(room = %id, shards = n, "sharded room created");
            }
        }
    }

    /// One watcher task per spawned room/shard task. It awaits ONLY that
    /// task's `JoinHandle` — the project's "one watcher per source, the
    /// owner awaits a single receive" idiom (see the signal handling in
    /// `main.rs`, the conn_ops dispatchers) — and reports the exit through
    /// the registry's own mailbox, exactly like the dispatchers report
    /// their completions.
    ///
    /// No cancellation plumbing, deliberately: EVERY exit reports, because
    /// `DestroyRoom` and server `Shutdown` also end room tasks normally.
    /// The discrimination happens on the receiving side instead (the design
    /// trick): a destroy removes the table entry FIRST and synchronously,
    /// so its late report finds no entry — or, if the id was re-created in
    /// between, an entry of a different `generation`. Either way the report
    /// is a silent no-op. Only a live entry with a matching generation is
    /// an *unexpected* death.
    ///
    /// The panic payload itself is not carried here: it is printed by the
    /// default panic hook when the task unwinds; what the base adds is the
    /// operator-facing attribution (which room, which shard, what happened
    /// to the members).
    fn spawn_room_watcher(
        id: RoomId,
        shard: Option<usize>,
        generation: u64,
        handle: tokio::task::JoinHandle<()>,
        registry: Mailbox<RegistryMsg>,
    ) {
        tokio::spawn(async move {
            // The outcome (panic vs clean return) is deliberately not
            // inspected: the registry decides whether this exit means
            // anything, based on its table state at report time.
            let _ = handle.await;
            let _ = registry
                .send(RegistryMsg::RoomDied {
                    id,
                    shard,
                    generation,
                })
                .await;
        });
    }

    /// Run until the mailbox is closed.
    pub async fn run(mut self) {
        debug!("registry actor started");
        while let Some(msg) = self.inbox.recv().await {
            match msg {
                RegistryMsg::CreateRoom { config, reply } => {
                    let id = config.id;
                    // A retired id stays closed to ACCIDENTAL traffic
                    // (joins/resumes answer ERROR 12 while absent), but an
                    // EXPLICIT create is the operator revisiting its ending
                    // decision — un-retire it and proceed (§8 protects
                    // against AUTOMATIC resurrection, e.g. the panic
                    // rebuild, never against a deliberate new create).
                    if self.retired.contains_key(&id) {
                        self.unretire_room(&id);
                        warn!(
                            room = %id,
                            "re-creating a RETIRED room id (explicit override; \
                             joins answer normally from here on)"
                        );
                    }
                    // Idempotency: a room that already exists is a no-op
                    // for an IDENTICAL request (the control plane's retry
                    // pattern) and a conflict for a different one. The
                    // whole-config comparison is the "same request"
                    // definition (see the message docs).
                    if let Some(existing) = self.rooms.get(&id) {
                        if existing.config == config {
                            let members = self.room_members(id);
                            debug!(room = %id, "room create idempotent (already exists)");
                            let _ = reply.send(Ok(RoomStatus::Running { members }));
                        } else {
                            warn!(room = %id, "room create conflict: different config");
                            let _ = reply.send(Err(CoreError::RoomConflict(id.0)));
                        }
                        continue;
                    }
                    // The room rate must divide the global ticker rate: the
                    // room steps on every k-th global tick (k = run_every).
                    let global = self.ticker.hz();
                    let run_every = (global / config.tick_hz).round() as u64;
                    if run_every < 1 || (global - config.tick_hz * run_every as f64).abs() > 1e-3 {
                        let _ = reply.send(Err(CoreError::TickRate {
                            room: config.tick_hz,
                            global,
                        }));
                        continue;
                    }
                    // Keep-alive cannot run faster than the room's own tick:
                    // the cadence would clamp to every step, the "silence
                    // when unchanged" gain would be lost, and clients would
                    // receive fewer keep-alives than configured. Reject the
                    // config rather than start a silently degraded room.
                    if config.keepalive_hz > 0.0 && config.keepalive_hz > config.tick_hz {
                        let _ = reply.send(Err(CoreError::KeepaliveRate {
                            keepalive: config.keepalive_hz,
                            tick: config.tick_hz,
                        }));
                        continue;
                    }
                    let built = (self.factory)(id, &config);
                    // The factory runs inside the registry loop on purpose
                    // (unchanged): room construction is synchronous game
                    // code, and a panicking FACTORY is out of scope for the
                    // death watch — it kills the registry itself, exactly
                    // as it does today. Supervision covers the spawned
                    // actor tasks (the tick loops), not this call.
                    //
                    // `install_room` is THE single creation path: both the
                    // create here and a panic rebuild (see
                    // `RegistryMsg::RoomDied`) wire the actors identically
                    // through it — one implementation, no drift.
                    self.install_room(config.clone(), built, run_every);
                    self.reg_created += 1;
                    self.emit_metrics();
                    debug!(room = %id, "room created");
                    // The reply is the status (a create round trip doubles
                    // as a status query — one hop, not two); a fresh room
                    // starts with zero members.
                    let _ = reply.send(Ok(RoomStatus::Running { members: 0 }));
                }
                RegistryMsg::DestroyRoom { id, reply } => {
                    if let Some(entry) = self.rooms.remove(&id) {
                        // The entry is gone FIRST (synchronously) — this is
                        // also what makes the death watcher's late report
                        // for this room a silent no-op (see
                        // `RegistryMsg::RoomDied`).
                        //
                        // Members learn `RoomGone`, affiliations clear —
                        // exactly the semantics an unexpected death gets
                        // below (same helper on purpose: a member must not
                        // be able to tell how the room ended).
                        self.notify_room_gone(id);
                        // The room processes it on its next tick (the ticker
                        // is still running); aborting the ticker later closes
                        // its broadcast as a backstop.
                        match (entry.control, entry.shards) {
                            (Some(control), _) => {
                                let _ = control.send(RoomControl::Shutdown).await;
                            }
                            (None, Some(group)) => {
                                // Sharded: one Shutdown per shard. Bounded
                                // sends (the same idiom as the single room);
                                // a stalled shard parks the send briefly and
                                // the ticker abort remains the backstop.
                                for tx in &group.mailboxes {
                                    let _ = tx.send(ShardMsg::Shutdown).await;
                                }
                            }
                            (None, None) => {}
                        }
                        self.reg_destroyed += 1;
                        self.emit_room_gone(id);
                        self.emit_metrics();
                        // The id is retired (§8): an ephemeral match ended,
                        // or a persistent room was decommissioned — either
                        // way later joins/resumes answer ERROR 12, and a
                        // create cannot silently reopen it.
                        self.retire_room(id);
                        debug!(room = %id, "room destroyed (id retired)");
                        let _ = reply.send(RoomStatus::Destroyed);
                    } else {
                        // Idempotent destroy: a missing room is a no-op
                        // success (a control plane retry never errors on
                        // the second attempt).
                        debug!(room = %id, "room destroy: absent (idempotent no-op)");
                        let _ = reply.send(RoomStatus::Absent);
                    }
                }
                RegistryMsg::RoomStatus { id, reply } => {
                    // Table-only answer (the registry never awaits a room):
                    // a present entry means running (the member count comes
                    // from the connection table — see the message docs);
                    // an accepted destroy already removed the entry, so
                    // `Absent` is the control-plane-correct answer for the
                    // shutdown window.
                    let status = if self.rooms.contains_key(&id) {
                        RoomStatus::Running {
                            members: self.room_members(id),
                        }
                    } else {
                        RoomStatus::Absent
                    };
                    let _ = reply.send(status);
                }
                RegistryMsg::SpawnPlayer {
                    conn,
                    room,
                    out,
                    identity,
                    reply,
                } => {
                    // Double-session supersedence (§5: "en son kazanan" —
                    // latest wins): a LIVE (still-connected) session with
                    // the same identity in the same room is evicted here,
                    // BEFORE the new join dispatches. The old socket —
                    // still open by definition — is closed with ERROR 9
                    // (`ConnIn::ServerClosed`), its affiliation released
                    // through the ordinary leave path. A DETACHED session
                    // with the identity needs no eviction: its park IS the
                    // resume target (the re-affiliation cleanup in
                    // `SpawnDone` releases it).
                    if !identity.is_empty() {
                        let mut evicted: Vec<(ConnectionId, Option<Mailbox<ConnIn>>)> =
                            Vec::new();
                        for (&other, info) in &self.conns {
                            if other != conn
                                && !info.detached
                                && info.room == Some(room)
                                && info.identity == identity
                            {
                                evicted.push((other, info.inbox.clone()));
                            }
                        }
                        for (old_conn, inbox) in evicted {
                            warn!(
                                %old_conn,
                                %conn,
                                room = %room,
                                %identity,
                                "double session: a newer session supersedes the \
                                 live one (ERROR 9 to the old socket)"
                            );
                            if let Some(inbox) = inbox {
                                let reason =
                                    "a newer session for this player superseded this \
                                     connection"
                                        .to_string();
                                tokio::spawn(async move {
                                    let _ = inbox.send(ConnIn::ServerClosed { reason }).await;
                                });
                            }
                            self.direct_leave(old_conn);
                        }
                    }
                    let Some(entry) = self.rooms.get(&room) else {
                        let code = if self.retired.contains_key(&room) {
                            CoreError::RoomRetired(room.0)
                        } else {
                            CoreError::RoomNotFound(room.0)
                        };
                        let _ = reply.send(Err(code));
                        continue;
                    };
                    // Record the resume key on THIS connection's entry (the
                    // resume re-affiliation matches on it). The entry may
                    // not exist yet (ConnOpened races nothing: the accept
                    // loop sends ConnOpened before any client frame), so
                    // create-on-demand like the cap check does.
                    self.conns.entry(conn).or_default().identity = identity.clone();
                    let sharded_room = entry.shards.is_some();
                    // The incarnation this join is dispatched against: the
                    // settlement reports (SpawnDone/SpawnFailed) echo it so
                    // a late settle of a since-died room cannot touch the
                    // table (supervision).
                    let generation = entry.generation;
                    // Sharded room: the registry is the only actor that
                    // sees every join, so it enforces the room cap here
                    // (a shard cannot count the room without shared state)
                    // and routes the join to the home shard (the factory's
                    // pure `home_shard` router — never awaited).
                    //
                    // All the reads from `entry` happen BEFORE the borrow
                    // ends (the `pending += 1` below re-borrows mutably).
                    let sharded_pick = match &entry.shards {
                        Some(group) => {
                            let rejoin =
                                self.conns.get(&conn).and_then(|i| i.room) == Some(room);
                            let at_cap = match group.cap {
                                Some(cap) => !rejoin && group.members + group.pending >= cap,
                                None => false,
                            };
                            Some((at_cap, (group.home)(conn), group.mailboxes.clone()))
                        }
                        None => None,
                    };
                    let (handle, shard_idx) = match sharded_pick {
                        Some((at_cap, home_idx, mailboxes)) => {
                            if at_cap {
                                // The cap makes the sharded room's degraded
                                // regime structurally unreachable (the same
                                // guardrail as a single room's max_players:
                                // gentle ERROR 8, the connection stays alive).
                                let _ = reply.send(Err(CoreError::RoomFull(room.0)));
                                continue;
                            }
                            // Reserve against the cap until the join settles
                            // (SpawnDone / SpawnFailed release it).
                            if let Some(e) =
                                self.rooms.get_mut(&room).and_then(|e| e.shards.as_mut())
                            {
                                e.pending += 1;
                            }
                            // Clamp a router bug (an out-of-range pick)
                            // instead of panicking at join dispatch; the
                            // entity's first boundary crossing self-heals
                            // the mis-routing (see BuiltRoom::Sharded).
                            let idx = home_idx % mailboxes.len();
                            (RoomHandle::Sharded(mailboxes), Some(idx))
                        }
                        None => (
                            RoomHandle::Single(
                                self.rooms
                                    .get(&room)
                                    .and_then(|e| e.control.clone())
                                    .expect("single room always has control"),
                            ),
                            None,
                        ),
                    };
                    let op_tx = self
                        .conn_ops
                        .entry(conn)
                        .or_insert_with(|| Self::spawn_conn_ops(conn, self.self_mailbox.clone()));
                    // The guard epoch is minted HERE, in the single-threaded
                    // registry, globally across all connections (see the
                    // `RoomOp::Join::epoch` doc for why per-connection
                    // minting broke first-attempt resumes).
                    self.next_join_epoch = self.next_join_epoch.wrapping_add(1);
                    let join_epoch = self.next_join_epoch;
                    if op_tx
                        .try_send(RoomOp::Join {
                            room,
                            handle,
                            shard: shard_idx,
                            generation,
                            epoch: join_epoch,
                            out,
                            identity,
                            reply,
                        })
                        .is_err()
                    {
                        // Op queue full (pathological): the op (and its
                        // reply) is dropped; the connection actor observes
                        // the dropped reply and sends an ERROR frame. The
                        // cap reservation never settles (the dispatcher
                        // never saw the op), so release it here.
                        if sharded_room
                            && let Some(e) =
                                self.rooms.get_mut(&room).and_then(|e| e.shards.as_mut())
                        {
                            e.pending = e.pending.saturating_sub(1);
                        }
                        warn!(%conn, room = %room, "join op queue full; join failed");
                    } else {
                        debug!(%conn, room = %room, "join dispatched");
                    }
                }
                RegistryMsg::DespawnPlayer { conn } => {
                    // Voluntary leave: the connection stays registered (its
                    // inbox must survive), only the affiliation goes.
                    let room = self.conns.get(&conn).and_then(|i| i.room);
                    match room {
                        Some(room) => {
                            let dispatched = self
                                .conn_ops
                                .get(&conn)
                                .and_then(|op_tx| op_tx.try_send(RoomOp::Leave { room }).ok());
                            if dispatched.is_some() {
                                // The dispatcher sends LeaveDone later, in
                                // order with any concurrent join.
                                debug!(%conn, room = %room, "leave dispatched");
                            } else {
                                self.direct_leave(conn);
                            }
                        }
                        None => debug!(%conn, "despawn of unaffiliated connection"),
                    }
                }
                RegistryMsg::ConnOpened { conn, inbox } => {
                    // Connection cap, enforced at birth: the count lives
                    // here (the connection table is the only place that
                    // sees both opens and closes), so the guardrail is
                    // enforced here, not in the accept loop. A rejected
                    // connection is never recorded — no table entry, no
                    // `reg_opens`, no room involvement — and its actor is
                    // told to close itself gently (`ERROR` frame, code 9).
                    if let Some(cap) = self.max_connections
                        && self.conns.len() as u64 >= cap
                    {
                        warn!(
                            %conn,
                            capacity = cap,
                            "server at connection capacity; new connection rejected"
                        );
                        tokio::spawn(async move {
                            let _ = inbox
                                .send(ConnIn::ServerClosed {
                                    reason: "server at connection capacity".into(),
                                })
                                .await;
                        });
                        continue;
                    }
                    let info = self.conns.entry(conn).or_default();
                    info.inbox = Some(inbox);
                    self.reg_opens += 1;
                    self.emit_metrics();
                }
                RegistryMsg::ConnClosed { conn } => {
                    // The connection actor is gone for good. The entity's
                    // fate is no longer decided HERE (the old code removed
                    // the affiliation and sent a despawn-causing leave):
                    // the registry only reports the fact and routes a
                    // DETACH — the ROOM's policy (`on_disconnect`)
                    // decides despawn-vs-hold (§3/§4). Until the policy
                    // answers, this entry stays with `detached` set and
                    // its inbox dropped (nothing can notify a dead
                    // socket): a parked player keeps holding its cap slot
                    // in this table exactly as it does in the room's.
                    //
                    // An UNAFFILIATED close still removes the entry
                    // outright (nothing to park, nothing to hold).
                    let Some(info) = self.conns.get_mut(&conn) else {
                        // The registry never recorded this connection — it
                        // was rejected at connection capacity. Its actor
                        // still reports the close; there is no entry to
                        // remove and nothing to count. A dispatcher slot
                        // can still exist if the client raced a JOIN in
                        // before its `ServerClosed` was processed (the room
                        // may have accepted it for a tick): drain it so the
                        // slot cannot outlive the connection.
                        if let Some(op_tx) = self.conn_ops.remove(&conn) {
                            let _ = op_tx.try_send(RoomOp::Close);
                        }
                        debug!(%conn, "close of unregistered connection");
                        continue;
                    };
                    let affiliated = info.room.is_some();
                    info.detached = affiliated;
                    info.inbox = None;
                    let (room, entity) = (info.room, info.entity);
                    match self.conn_ops.remove(&conn) {
                        Some(op_tx) => {
                            // The dispatcher serializes the detach behind
                            // any in-flight join and reports `DetachDone`.
                            let _ = op_tx.try_send(RoomOp::Close);
                        }
                        None => {
                            if let (Some(room), Some(entity)) = (room, entity) {
                                self.send_detach_direct(conn, room, entity);
                                // Sharded room: NO member decrement — the
                                // parked entity still holds its slot (§4).
                            }
                        }
                    }
                    if !affiliated {
                        // Nothing to park: the entry has no future reader
                        // (no resume can target it), so it goes, exactly
                        // like the old code's unconditional removal.
                        self.conns.remove(&conn);
                    }
                    self.reg_closes += 1;
                    self.emit_metrics();
                    debug!(
                        %conn,
                        ?room,
                        "connection closed (detach routed; affiliation held for \
                         the park)"
                    );
                }
                RegistryMsg::SpawnDone {
                    conn,
                    room,
                    entity,
                    generation,
                } => {
                    // Ordering note (supervision): a settled join races the
                    // death report of the room incarnation it was dispatched
                    // against — both travel to us over our own mailbox, in
                    // no guaranteed order. The dispatcher echoes the
                    // generation its handle was stamped with, so one
                    // comparison decides: an ABSENT room, or a room of a
                    // DIFFERENT incarnation (destroyed and even rebuilt in
                    // the meantime), must not receive this affiliation.
                    // Recording it would resurrect the exact zombie the
                    // death watch exists to kill (status `Running` forever,
                    // a member that never learns the room is gone) or pin a
                    // dead join onto the rebuilt room. Either order of the
                    // two messages now converges to the same end state: the
                    // connection is told `RoomGone` (it holds senders into
                    // a dead task) and stays unaffiliated (it may rejoin).
                    if self.rooms.get(&room).map(|e| e.generation) != Some(generation) {
                        let notify = match self.conns.get_mut(&conn) {
                            Some(info) => {
                                info.room = None;
                                info.entity = None;
                                info.inbox.clone()
                            }
                            None => None,
                        };
                        if let Some(inbox) = notify {
                            tokio::spawn(async move {
                                let _ = inbox.send(ConnIn::RoomGone(room)).await;
                            });
                        }
                        debug!(
                            %conn,
                            room = %room,
                            "join settled after its room died; affiliation dropped"
                        );
                        continue;
                    }
                    // Ordered per-connection (from the dispatcher). If the
                    // connection is unknown it died mid-join; the
                    // dispatcher's Close already cleaned up the room side.
                    // (The `info` borrow is scoped inside the `match` so the
                    // `&mut self` `emit_metrics` call below does not conflict
                    // with it.)
                    let new_affiliation = match self.conns.get_mut(&conn) {
                        Some(info) => {
                            let fresh = info.room != Some(room);
                            info.room = Some(room);
                            info.entity = Some(entity);
                            fresh
                        }
                        None => false,
                    };
                    // Resume re-affiliation cleanup: the NEW session's
                    // ConnectionId replaced the parked one — release the OLD
                    // detached entry for this identity (the room-side park is
                    // consumed by the rebind; holding the registry entry any
                    // longer would leak a slot). Exactly one such entry can
                    // exist (one identity, one park); a scan keeps this
                    // correct without a second index.
                    if new_affiliation {
                        let identity = self
                            .conns
                            .get(&conn)
                            .map(|i| i.identity.clone())
                            .unwrap_or_default();
                        if !identity.is_empty() {
                            let mut released = 0u64;
                            let stale_keys: Vec<ConnectionId> = self
                                .conns
                                .iter()
                                .filter(|(c, i)| {
                                    **c != conn
                                        && i.detached
                                        && i.identity == identity
                                        && i.room == Some(room)
                                })
                                .map(|(c, _)| *c)
                                .collect();
                            for old in stale_keys {
                                self.conns.remove(&old);
                                released += 1;
                                debug!(
                                    %old,
                                    %conn,
                                    room = %room,
                                    %identity,
                                    "resume re-affiliation released the detached \
                                     entry"
                                );
                            }
                            // The sharded member counter nets out here: each
                            // released entry held a count; the resumed session's
                            // `fresh` increment below replaces exactly one of
                            // them. (A fallback FRESH join after an expiry also
                            // lands here with zero releases — its +1 is the
                            // genuine new member.)
                            if released > 0
                                && let Some(e) = self
                                    .rooms
                                    .get_mut(&room)
                                    .and_then(|e| e.shards.as_mut())
                            {
                                e.members = e.members.saturating_sub(released);
                            }
                        }
                    }
                    // Sharded room: settle the capacity reservation (the
                    // join was dispatched against the cap) and, when the
                    // affiliation is new, count the member (a re-join of an
                    // already-affiliated connection is not a new member —
                    // the same semantics as a single room's cap).
                    if let Some(e) = self.rooms.get_mut(&room).and_then(|e| e.shards.as_mut()) {
                        e.pending = e.pending.saturating_sub(1);
                        if new_affiliation {
                            e.members += 1;
                        }
                    }
                    if self.conns.contains_key(&conn) {
                        // (Counted per SpawnDone for a live connection —
                        // the pre-sharding semantics; a re-join re-counts,
                        // as before.)
                        self.reg_joins += 1;
                        self.emit_metrics();
                        debug!(%conn, room = %room, %entity, "player spawned");
                    }
                }
                RegistryMsg::SpawnFailed {
                    conn,
                    room,
                    generation,
                } => {
                    // The room rejected the dispatched join (e.g. the
                    // shard's wire-id range is exhausted): release the
                    // capacity reservation (the join never counted). Only
                    // for the SAME incarnation: a stale failure from a dead
                    // room must not touch a rebuilt room's counters.
                    if let Some(e) = self.rooms.get_mut(&room)
                        && e.generation == generation
                        && let Some(shards) = e.shards.as_mut()
                    {
                        shards.pending = shards.pending.saturating_sub(1);
                    }
                    debug!(%conn, room = %room, "spawn failed; reservation released");
                }
                RegistryMsg::LeaveDone { conn, room } => {
                    let left = match self.conns.get_mut(&conn) {
                        Some(info) => {
                            let matched = info.room == Some(room);
                            if matched {
                                info.room = None;
                                info.entity = None;
                            }
                            matched
                        }
                        None => false,
                    };
                    if left {
                        // Sharded room: the registry's counter loses the
                        // member (see `ShardGroup`).
                        if let Some(e) =
                            self.rooms.get_mut(&room).and_then(|e| e.shards.as_mut())
                        {
                            e.members = e.members.saturating_sub(1);
                        }
                        self.reg_leaves += 1;
                        self.emit_metrics();
                        debug!(%conn, room = %room, "player despawned");
                    }
                }
                RegistryMsg::DetachDone { conn, room } => {
                    // The detach was delivered: the affiliation is KEPT
                    // under the detached mark (the parked entity holds its
                    // slot, §4) — no member decrement, no `reg_leaves`.
                    // Released later by a resume's SpawnDone cleanup or by
                    // the room ending (destroy/death/notify_room_gone).
                    if let Some(info) = self.conns.get_mut(&conn)
                        && info.room == Some(room)
                    {
                        info.detached = true;
                        debug!(%conn, room = %room, "player detached (slot held)");
                    }
                }
                RegistryMsg::OpsClosed { conn } => {
                    self.conn_ops.remove(&conn);
                }
                RegistryMsg::RoomDied {
                    id,
                    shard,
                    generation,
                } => {
                    // The design trick (no cancellation plumbing): a NORMAL
                    // end — `DestroyRoom`, server `Shutdown` — removes the
                    // table entry first, so this report finds either no
                    // entry, or an entry of a DIFFERENT incarnation (the id
                    // was re-created in the meantime), and must stay
                    // silent. Only a live entry of the SAME generation is an
                    // unexpected death.
                    let is_current = self
                        .rooms
                        .get(&id)
                        .map(|e| e.generation == generation)
                        .unwrap_or(false);
                    if !is_current {
                        debug!(
                            room = %id,
                            shard = ?shard,
                            "late death report for a destroyed/replaced room; ignored"
                        );
                        continue;
                    }
                    // Unexpected death. For a sharded room, ANY dead shard
                    // means the whole LOGICAL room is broken — its neighbors
                    // hold senders into the dead shard's closed mailbox, so
                    // cross-shard migration can never complete again. There
                    // is no partial-shard recovery: the whole room goes,
                    // exactly like a single-room death.
                    warn!(
                        room = %id,
                        shard = ?shard,
                        "room task died unexpectedly (panic in game logic?); \
                         reaping the room"
                    );
                    let entry = self.rooms.remove(&id).expect("generation checked above");
                    // Exactly the destroy semantics: members learn
                    // `ConnIn::RoomGone`, affiliations clear, status turns
                    // `Absent`.
                    self.notify_room_gone(id);
                    self.reg_died += 1;
                    self.emit_room_gone(id);
                    self.emit_metrics();
                    // Restart policy: `restart_on_panic` is the operator's
                    // preference, but a PERSISTENT room's class guarantee
                    // overrides it (§8 — "sürekli oda panikle ölü kalmaz"
                    // is a class property, not a tunable): the rebuild is
                    // forced ON. The rebuilt room comes back EMPTY either
                    // way — the members were notified above and may
                    // rejoin; no world state survives (it lived inside the
                    // dead task). A logic that panics persistently yields
                    // a restart-per-death cycle, one warn per round:
                    // immediately visible to the operator, deemed
                    // acceptable for v1 (no backoff machinery).
                    //
                    // In-flight joins dispatched against the dead
                    // incarnation settle later (SpawnDone/SpawnFailed);
                    // their sharded-counter effects land on the NEW
                    // ShardGroup with saturating arithmetic — bounded
                    // imprecision (at most the number of in-flight joins),
                    // never a panic or a permanently stuck cap.
                    if entry.config.restart_on_panic || entry.config.persistent {
                        warn!(
                            room = %id,
                            persistent = entry.config.persistent,
                            "rebuilding the room from its factory + config \
                             (it comes back EMPTY)"
                        );
                        let global = self.ticker.hz();
                        // The config was validated when this room was first
                        // created (tick rate divides the global rate), so
                        // the recomputed step divisor is >= 1 by
                        // construction.
                        let run_every = (global / entry.config.tick_hz).round() as u64;
                        let built = (self.factory)(id, &entry.config);
                        self.install_room(entry.config.clone(), built, run_every);
                        debug!(room = %id, "room rebuilt after unexpected death");
                    } else {
                        // A death left unrebuilt ends the match: retire the
                        // id so later joins answer ERROR 12 ("definitively
                        // over") instead of 4 ("unknown/temporary").
                        self.retire_room(id);
                    }
                }
                RegistryMsg::Shutdown => {
                    warn!("registry shutting down");
                    // 1. Ask every dispatcher to drain (final leaves for
                    //    in-flight joins), then drop the senders so they
                    //    exit after draining.
                    for op_tx in self.conn_ops.values() {
                        let _ = op_tx.try_send(RoomOp::Close);
                    }
                    self.conn_ops.clear();
                    // 2. Notify every registered connection.
                    let doomed: Vec<Mailbox<ConnIn>> = self
                        .conns
                        .values()
                        .filter_map(|i| i.inbox.clone())
                        .collect();
                    for inbox in doomed {
                        tokio::spawn(async move {
                            let _ = inbox.send(ConnIn::Shutdown).await;
                        });
                    }
                    self.conns.clear();
                    // 3. Stop every room (a single room via its control
                    //    channel; a sharded room via one Shutdown per
                    //    shard) — processed on the next tick; the
                    //    composition root aborts the ticker afterwards,
                    //    which closes the broadcast as a backstop for any
                    //    room that misses the window.
                    // `std::mem::take` rather than `drain()`: the loop
                    // body awaits, and a live drain borrow would fight
                    // the awaited sends' executor hops in future edits.
                    //
                    // No `RoomGone` notice here, DELIBERATELY: this is
                    // the whole-server shutdown — the collector dies with
                    // the ticker right after its final report, so there
                    // is no leak to prune; dropping the accumulators
                    // first would instead erase every room's LAST report
                    // window (consumers read the room line off that final
                    // report). The destroy and unexpected-death paths DO
                    // notify: they happen mid-flight, where an accumulator
                    // would otherwise outlive its room forever.
                    for (id, entry) in std::mem::take(&mut self.rooms) {
                        match (entry.control, entry.shards) {
                            (Some(control), _) => {
                                let _ = control.send(RoomControl::Shutdown).await;
                            }
                            (None, Some(group)) => {
                                for tx in &group.mailboxes {
                                    let _ = tx.send(ShardMsg::Shutdown).await;
                                }
                            }
                            (None, None) => {}
                        }
                        debug!(room = %id, "room stopped");
                    }
                    // 4. Stop the actor now. (It cannot wait for the mailbox
                    //    to close: it holds a clone of it — `self_mailbox` —
                    //    for dispatcher reporting, so EOF would never come.)
                    //    Dispatchers already received Close and will exit on
                    //    their own; their stray reports fail against the
                    //    dropped inbox, harmlessly. The death watchers exit
                    //    the same way: when the ticker abort closes each
                    //    room's tick channel, every watcher's report fails
                    //    against our dropped mailbox and the watcher stops
                    //    (no task leaks beyond the server's lifetime).
                    break;
                }
            }
        }
        debug!("registry actor stopped");
    }

    /// Notify every connection affiliated with `room` that the room is
    /// gone ([`ConnIn::RoomGone`], fire-and-forget spawned sends) and clear
    /// their affiliations; their inbox is kept (clone, don't take) so they
    /// can still receive `Shutdown` or later notifications.
    ///
    /// Shared by the destroy path and the unexpected-death path (see
    /// `RegistryMsg::RoomDied`) on purpose: members must not be able to
    /// tell how the room ended.
    fn notify_room_gone(&mut self, room: RoomId) {
        let mut doomed = Vec::new();
        let mut dead_detached: Vec<ConnectionId> = Vec::new();
        for (conn, info) in self.conns.iter_mut() {
            if info.room == Some(room) {
                info.room = None;
                info.entity = None;
                if info.detached {
                    // A parked player's socket is already gone: no
                    // notification can reach anyone and the affiliation is
                    // over — the entry goes (the room ending released its
                    // slot room-side too).
                    info.detached = false;
                    dead_detached.push(*conn);
                } else if let Some(inbox) = info.inbox.clone() {
                    doomed.push((*conn, inbox));
                }
            }
        }
        for conn in dead_detached {
            self.conns.remove(&conn);
        }
        for (conn, inbox) in doomed {
            // Fire-and-forget notification (no reply needed).
            tokio::spawn(async move {
                let _ = inbox.send(ConnIn::RoomGone(room)).await;
                debug!(%conn, room = %room, "notified: room gone");
            });
        }
    }

    /// Leave without a dispatcher (no join/leave is in flight for this
    /// connection, so the table can be updated synchronously).
    fn direct_leave(&mut self, conn: ConnectionId) {
        let (room, entity) = match self.conns.get(&conn) {
            Some(i) => (i.room, i.entity),
            None => (None, None),
        };
        if let (Some(room), Some(entity)) = (room, entity) {
            let had_room = self
                .rooms
                .get(&room)
                .map(|e| e.control.is_some() || e.shards.is_some())
                .unwrap_or(false);
            if had_room {
                self.send_leave_direct(conn, room, entity);
                // Sharded room: the registry's counter loses the member
                // (see `ShardGroup`).
                if let Some(e) = self.rooms.get_mut(&room).and_then(|e| e.shards.as_mut()) {
                    e.members = e.members.saturating_sub(1);
                }
                // Counted here (not via LeaveDone): this path has no
                // dispatcher, so the room's leave would otherwise be
                // invisible to the registry counter.
                self.reg_leaves += 1;
            }
            if let Some(info) = self.conns.get_mut(&conn) {
                info.room = None;
                info.entity = None;
            }
            debug!(%conn, room = %room, "player despawned");
        } else {
            debug!(%conn, "despawn without affiliation");
        }
    }

    /// Send a leave with no dispatcher (the `ConnClosed` /
    /// [`Self::direct_leave`] paths): to the room's control channel, or —
    /// for a sharded room — to ALL of its shards (exactly one of them owns
    /// the connection; the entity-id guard makes the others no-ops, and
    /// the leave's epoch is 0, which can only lower the tombstones the
    /// join itself already recorded — see `crate::shard`, "Migration
    /// protocol").
    fn send_leave_direct(&mut self, conn: ConnectionId, room: RoomId, entity: EntityId) {
        let handle = match self.rooms.get(&room) {
            Some(e) => match (&e.control, &e.shards) {
                (Some(control), _) => RoomHandle::Single(control.clone()),
                (None, Some(group)) => RoomHandle::Sharded(group.mailboxes.clone()),
                (None, None) => return,
            },
            None => return,
        };
        tokio::spawn(async move {
            match handle {
                RoomHandle::Single(control) => {
                    let _ = control.send(RoomControl::Leave { conn, entity }).await;
                }
                RoomHandle::Sharded(mailboxes) => {
                    for tx in &mailboxes {
                        let _ = tx
                            .send(ShardMsg::Leave {
                                conn,
                                entity,
                                epoch: 0,
                            })
                            .await;
                    }
                }
            }
        });
    }

    /// Send a DETACH with no dispatcher (the dispatcher-less
    /// [`RegistryMsg::ConnClosed`] path): to the room's control channel,
    /// or — for a sharded room — to ALL of its shards (exactly one of them
    /// owns the connection; the entity-id guard makes the others no-ops).
    /// The ROOM decides despawn-vs-hold via its `on_disconnect` policy;
    /// this path only reports the transport death.
    fn send_detach_direct(&mut self, conn: ConnectionId, room: RoomId, entity: EntityId) {
        let Some(identity) = self.conns.get(&conn).map(|i| i.identity.clone())
        else {
            return;
        };
        let handle = match self.rooms.get(&room) {
            Some(e) => match (&e.control, &e.shards) {
                (Some(control), _) => RoomHandle::Single(control.clone()),
                (None, Some(group)) => RoomHandle::Sharded(group.mailboxes.clone()),
                (None, None) => return,
            },
            None => return,
        };
        tokio::spawn(async move {
            match handle {
                RoomHandle::Single(control) => {
                    let _ = control
                        .send(RoomControl::Detach {
                            conn,
                            entity,
                            identity,
                        })
                        .await;
                }
                RoomHandle::Sharded(mailboxes) => {
                    for tx in &mailboxes {
                        let _ = tx
                            .send(ShardMsg::Detach {
                                conn,
                                entity,
                                identity: identity.clone(),
                            })
                            .await;
                    }
                }
            }
        });
    }

    /// One dispatcher task per connection with a room relationship in
    /// flight. It is the *only* sender of room control messages for that
    /// connection, so per-connection ordering (join → leave → rejoin) is
    /// guaranteed, and the registry never awaits a room from its own task.
    ///
    /// The dispatcher serializes this connection's room-relationship ops
    /// (join → leave → rejoin in dispatch order). Join epochs are minted
    /// GLOBALLY by the registry at dispatch time and arrive stamped on
    /// the op (see `RoomOp::Join::epoch`): the old per-connection counter
    /// reset to 1 on every new session, so a reconnect of a parked
    /// identity tripped the resume staleness guard once before a retry
    /// succeeded.
    fn spawn_conn_ops(
        conn: ConnectionId,
        registry: Mailbox<RegistryMsg>,
    ) -> mpsc::Sender<RoomOp<St>> {
        let (op_tx, mut op_rx) = mpsc::channel::<RoomOp<St>>(16);
        tokio::spawn(async move {
            // (room, entity, handle, the join's epoch, the resume key)
            let mut in_room: Option<(RoomId, EntityId, RoomHandle<St>, u64, String)> =
                None;
            while let Some(op) = op_rx.recv().await {
                match op {
                    RoomOp::Join {
                        room,
                        handle,
                        shard,
                        generation,
                        epoch,
                        out,
                        identity,
                        reply,
                    } => {
                        // An identified join IS a resume attempt (§14.3):
                        // route it at the park ledger first; fall back to
                        // the plain join when nothing holds the identity.
                        let outcome = if identity.is_empty() {
                            Self::dispatch_plain_join(
                                conn,
                                room,
                                &handle,
                                shard,
                                epoch,
                                out,
                            )
                            .await
                        } else {
                            Self::dispatch_resume(
                                conn,
                                room,
                                &handle,
                                shard,
                                epoch,
                                identity.clone(),
                                out,
                            )
                            .await
                        };
                        match outcome {
                            OpOutcome::Joined(entity, actions) => {
                                in_room = Some((room, entity, handle, epoch, identity));
                                let _ = reply.send(Ok((entity, actions)));
                                let _ = registry
                                    .send(RegistryMsg::SpawnDone {
                                        conn,
                                        room,
                                        entity,
                                        generation,
                                    })
                                    .await;
                            }
                            // The room rejected the join structurally (a full
                            // room): propagate the room's error to the
                            // connection actor (it maps `RoomFull` to the
                            // `ERROR` frame's own code), record no room
                            // state, and — for a sharded room — release
                            // the registry's capacity reservation.
                            OpOutcome::Rejected(e) => {
                                let _ = reply.send(Err(e));
                                let _ = registry
                                    .send(RegistryMsg::SpawnFailed {
                                        conn,
                                        room,
                                        generation,
                                    })
                                    .await;
                            }
                            OpOutcome::Gone => {
                                // Control channel gone (room destroyed) or the
                                // room dropped the reply.
                                let _ = reply.send(Err(CoreError::RoomGone));
                                let _ = registry
                                    .send(RegistryMsg::SpawnFailed {
                                        conn,
                                        room,
                                        generation,
                                    })
                                    .await;
                            }
                        }
                    }
                    RoomOp::Leave { room } => {
                        if let Some((r, entity, handle, ep, _id)) = in_room.take()
                            && r == room
                        {
                            Self::send_room_leave(conn, entity, ep, handle).await;
                            let _ = registry
                                .send(RegistryMsg::LeaveDone { conn, room: r })
                                .await;
                        }
                    }
                    RoomOp::Close => {
                        if let Some((r, entity, handle, _ep, identity)) = in_room.take() {
                            // Transport death: DETACH, not leave — the
                            // ROOM's policy decides despawn-vs-hold (§3).
                            Self::send_room_detach(conn, entity, identity, handle).await;
                            // The affiliation is KEPT (parked slot held,
                            // §4): DetachDone marks the entry instead of
                            // clearing it.
                            let _ = registry
                                .send(RegistryMsg::DetachDone { conn, room: r })
                                .await;
                        }
                        let _ = registry.send(RegistryMsg::OpsClosed { conn }).await;
                        break;
                    }
                }
            }
            // Normal exit: the registry dropped the op channel (shutdown or
            // the dispatcher was never needed again). Any room-side state is
            // either already left (the last op was a Leave) or the room is
            // being torn down (its world is dropped) — nothing to clean.
        });
        op_tx
    }

    /// Send a leave for a dispatcher-held room affiliation: to the single
    /// room's control channel, or — for a sharded room — to ALL of its
    /// shards (exactly one owns the connection; the entity-id guard makes
    /// the others no-ops, and the epoch travels so a late migration of the
    /// same join is rejected — see `crate::shard`).
    async fn send_room_leave(
        conn: ConnectionId,
        entity: EntityId,
        epoch: u64,
        handle: RoomHandle<St>,
    ) {
        match handle {
            RoomHandle::Single(control) => {
                let _ = control.send(RoomControl::Leave { conn, entity }).await;
            }
            RoomHandle::Sharded(mailboxes) => {
                for tx in &mailboxes {
                    let _ = tx
                        .send(ShardMsg::Leave {
                            conn,
                            entity,
                            epoch,
                        })
                        .await;
                }
            }
        }
    }

    /// The detach counterpart of [`Self::send_room_leave`]: transport
    /// death routes `RoomControl::Detach` / a broadcast
    /// `ShardMsg::Detach` — the room's policy (`on_disconnect`) decides
    /// despawn-vs-hold; the registry never does (§3).
    async fn send_room_detach(
        conn: ConnectionId,
        entity: EntityId,
        identity: String,
        handle: RoomHandle<St>,
    ) {
        match handle {
            RoomHandle::Single(control) => {
                let _ = control
                    .send(RoomControl::Detach {
                        conn,
                        entity,
                        identity,
                    })
                    .await;
            }
            RoomHandle::Sharded(mailboxes) => {
                for tx in &mailboxes {
                    let _ = tx
                        .send(ShardMsg::Detach {
                            conn,
                            entity,
                            identity: identity.clone(),
                        })
                        .await;
                }
            }
        }
    }

    /// The ordinary fresh join round trip (the pre-reconnect shape):
    /// single rooms get a control `Join`; sharded rooms a `ShardMsg::Join`
    /// on the home shard. Returns the outcome class the dispatcher acts on.
    async fn dispatch_plain_join(
        conn: ConnectionId,
        _room: RoomId,
        handle: &RoomHandle<St>,
        shard: Option<usize>,
        epoch: u64,
        out: mpsc::Sender<FrameBatch>,
    ) -> OpOutcome {
        let (joined_tx, joined_rx) =
            oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
        let sent = match handle {
            RoomHandle::Single(control) => control
                .send(RoomControl::Join {
                    conn,
                    out,
                    reply: joined_tx,
                })
                .await
                .is_ok(),
            RoomHandle::Sharded(mailboxes) => {
                let i = shard.expect("sharded join carries its shard");
                mailboxes[i]
                    .send(ShardMsg::Join {
                        conn,
                        epoch,
                        out,
                        reply: joined_tx,
                    })
                    .await
                    .is_ok()
            }
        };
        match (sent, joined_rx.await) {
            (true, Ok(Ok((entity, actions)))) => OpOutcome::Joined(entity, actions),
            (true, Ok(Err(e))) => OpOutcome::Rejected(e),
            _ => OpOutcome::Gone,
        }
    }

    /// The implicit resume round trip (§14.3 + §6): single rooms get one
    /// `RoomControl::Resume` (the room falls back to a fresh join itself);
    /// sharded rooms broadcast `ShardMsg::Resume` to EVERY shard and the
    /// outcomes are folded here — at most one accept is structurally
    /// possible (the ledger record lives on exactly one shard), an
    /// all-miss folds into a fallback plain join on the home shard, and a
    /// tripped epoch guard propagates as a rejection (a newer session
    /// already owns the identity).
    async fn dispatch_resume(
        conn: ConnectionId,
        room: RoomId,
        handle: &RoomHandle<St>,
        shard: Option<usize>,
        epoch: u64,
        identity: String,
        out: mpsc::Sender<FrameBatch>,
    ) -> OpOutcome {
        match handle {
            RoomHandle::Single(control) => {
                let (joined_tx, joined_rx) =
                    oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
                let sent = control
                    .send(RoomControl::Resume {
                        conn,
                        epoch,
                        identity,
                        out,
                        reply: joined_tx,
                    })
                    .await
                    .is_ok();
                match (sent, joined_rx.await) {
                    (true, Ok(Ok((entity, actions)))) => OpOutcome::Joined(entity, actions),
                    (true, Ok(Err(e))) => OpOutcome::Rejected(e),
                    _ => OpOutcome::Gone,
                }
            }
            RoomHandle::Sharded(mailboxes) => {
                // One oneshot per shard; each shard answers exactly once
                // (its CONTROL phase drains its FIFO inbox). Awaiting them
                // all is bounded by the shard count (≤ 16 by the grid
                // topology).
                let n = mailboxes.len();
                let (agg_tx, mut agg_rx) = mpsc::channel::<
                    Result<Option<(EntityId, Mailbox<Action>)>, CoreError>,
                >(n.max(1));
                for tx in mailboxes {
                    let (reply_tx, reply_rx) = oneshot::channel();
                    if tx
                        .send(ShardMsg::Resume {
                            conn,
                            epoch,
                            identity: identity.clone(),
                            out: out.clone(),
                            reply: reply_tx,
                        })
                        .await
                        .is_ok()
                    {
                        // Forward this shard's eventual answer into the
                        // aggregate; a dropped shard dies with its oneshot
                        // and simply contributes nothing.
                        let value = agg_tx.clone();
                        tokio::spawn(async move {
                            if let Ok(answer) = reply_rx.await {
                                let _ = value.send(answer).await;
                            }
                        });
                    }
                }
                let mut accepted: Option<(EntityId, Mailbox<Action>)> = None;
                let mut stale: Option<CoreError> = None;
                for _ in 0..n {
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        agg_rx.recv(),
                    )
                    .await
                    {
                        Ok(Some(Ok(Some(pair)))) => accepted = Some(pair),
                        Ok(Some(Err(e))) => stale = Some(e),
                        _ => {}
                    }
                }
                if let Some((entity, actions)) = accepted {
                    return OpOutcome::Joined(entity, actions);
                }
                if let Some(e) = stale {
                    return OpOutcome::Rejected(e);
                }
                // All shards answered "not here": transparent fresh join
                // (§5) through the ordinary path.
                Self::dispatch_plain_join(conn, room, handle, shard, epoch, out).await
            }
        }
    }
}
