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
//!
//! No locks: every cross-actor value (mailboxes, one-shot replies) is moved
//! through channels. In particular the registry **never awaits a room**:
//! `SpawnPlayer` hands the room round-trip to the connection's dispatcher
//! and returns immediately, so one slow room can never block the control
//! plane (joins elsewhere, room creation, shutdown).
//!
//! Room lifecycle is channel-driven: creating a room is a `subscribe` on
//! the ticker plus a control channel; destroying one sends a control
//! `Shutdown` (processed on the room's next tick) — there are no per-room
//! tasks to track or abort.

use std::collections::HashMap;
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
    SpawnDone {
        conn: ConnectionId,
        room: RoomId,
        entity: EntityId,
    },
    /// A dispatched join was rejected by the room (e.g. the shard's
    /// wire-id range is exhausted): release the capacity reservation.
    SpawnFailed { conn: ConnectionId, room: RoomId },
    /// A dispatched leave completed: clear the affiliation.
    LeaveDone { conn: ConnectionId, room: RoomId },
    /// A connection's dispatcher task exited; drop its slot.
    OpsClosed { conn: ConnectionId },
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
    Join {
        room: RoomId,
        handle: RoomHandle<St>,
        /// The home shard index (sharded rooms only).
        shard: Option<usize>,
        out: mpsc::Sender<FrameBatch>,
        reply: oneshot::Sender<Result<(EntityId, Mailbox<Action>), CoreError>>,
    },
    /// Leave `room`: send control `Leave` (with the entity this dispatcher
    /// saw the join create), then report [`RegistryMsg::LeaveDone`].
    Leave { room: RoomId },
    /// Drain the queue (processing whatever is left, including a final
    /// leave), report [`RegistryMsg::OpsClosed`], exit.
    Close,
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
}

#[derive(Default)]
struct ConnInfo {
    room: Option<RoomId>,
    entity: Option<EntityId>,
    /// Set via [`RegistryMsg::ConnOpened`]; kept for the connection's whole
    /// lifetime so `RoomGone`/`Shutdown` can always reach it.
    inbox: Option<Mailbox<ConnIn>>,
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
    conns: HashMap<ConnectionId, ConnInfo>,
    conn_ops: HashMap<ConnectionId, mpsc::Sender<RoomOp<St>>>,
    ticker: Ticker,
    /// Local control-plane counters (flushed as a sample whenever a table
    /// changes — event-driven; no timer, no new await; see
    /// [`crate::metrics`]).
    reg_created: u64,
    reg_destroyed: u64,
    reg_joins: u64,
    reg_leaves: u64,
    reg_opens: u64,
    reg_closes: u64,
    /// Metric samples dropped on a full (bounded) metrics channel,
    /// cumulative.
    reg_metrics_dropped: u64,
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
            conns: HashMap::new(),
            conn_ops: HashMap::new(),
            ticker,
            reg_created: 0,
            reg_destroyed: 0,
            reg_joins: 0,
            reg_leaves: 0,
            reg_opens: 0,
            reg_closes: 0,
            reg_metrics_dropped: 0,
            metrics,
            max_connections,
            result_sink,
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

    /// Run until the mailbox is closed.
    pub async fn run(mut self) {
        debug!("registry actor started");
        while let Some(msg) = self.inbox.recv().await {
            match msg {
                RegistryMsg::CreateRoom { config, reply } => {
                    let id = config.id;
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
                    match built {
                        BuiltRoom::Single { world, logic } => {
                            let (control_tx, control_rx) = channel(config.control_capacity);
                            tokio::spawn(
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
                            self.rooms.insert(
                                id,
                                RoomEntry {
                                    control: Some(control_tx),
                                    shards: None,
                                    config: config.clone(),
                                },
                            );
                        }
                        BuiltRoom::Sharded { shards, home_shard } => {
                            let n = shards.len();
                            debug_assert!(n >= 1, "a sharded room needs >= 1 shard");
                            // One channel per shard: the registry keeps the
                            // original sender, and every neighbor of the
                            // shard holds a CLONE of it (tokio mpsc: many
                            // senders, one receiver). So each shard's
                            // mailbox carries both the registry's control
                            // messages and its neighbors' protocol messages
                            // (Migrate/Border) — the shard actor's single
                            // `try_recv` drain handles all of them.
                            //
                            // Pass 1 — one channel per shard (the registry
                            // keeps the original sender; every neighbor
                            // holds a clone — tokio mpsc: many senders, one
                            // receiver), and each actor's `neighbors` vec
                            // (`txs[a][b]` = the sender shard a uses to
                            // reach shard b, indexed by the receiver's
                            // index — the actor indexes it that way;
                            // non-neighbor slots are dummies, closed
                            // senders that are never sent to).
                            let mut rxs: Vec<Inbox<ShardMsg<St>>> = Vec::with_capacity(n);
                            let mut reg_txs = Vec::with_capacity(n);
                            let (dummy_tx, _dummy_rx) = channel::<ShardMsg<St>>(1);
                            for _ in 0..n {
                                let (tx, rx) = channel(config.control_capacity);
                                reg_txs.push(tx.clone());
                                rxs.push(rx);
                            }
                            let mut txs: Vec<Vec<Mailbox<ShardMsg<St>>>> =
                                Vec::with_capacity(n);
                            for a in 0..n {
                                let mut row = Vec::with_capacity(n);
                                for (b, tx_b) in reg_txs.iter().enumerate() {
                                    // neighbors() is game knowledge (the
                                    // grid topology); it is read after the
                                    // factory built the logics.
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
                            // Pass 2 — spawn the shard actors (indices = the
                            // vec order; the sample id / neighbor slots rely
                            // on it).
                            for (i, (world, logic)) in shards.into_iter().enumerate() {
                                // `rxs` was built in shard order (pass 1), so
                                // popping the front pairs each shard with its
                                // own receiver.
                                let rx = rxs.remove(0);
                                tokio::spawn(
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
                            }
                            self.rooms.insert(
                                id,
                                RoomEntry {
                                    control: None,
                                    shards: Some(ShardGroup {
                                        mailboxes: reg_txs,
                                        home: home_shard,
                                        cap: config
                                            .max_players
                                            .map(|c| c as u64),
                                        members: 0,
                                        pending: 0,
                                    }),
                                    config,
                                },
                            );
                            debug!(room = %id, shards = n, "sharded room created");
                        }
                    }
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
                        // Clear the affiliation of this room's connections;
                        // keep their inbox (clone, don't take) so they can
                        // still receive Shutdown or later RoomGone frames.
                        let mut doomed = Vec::new();
                        for (conn, info) in self.conns.iter_mut() {
                            if info.room == Some(id) {
                                info.room = None;
                                info.entity = None;
                                if let Some(inbox) = info.inbox.clone() {
                                    doomed.push((*conn, inbox));
                                }
                            }
                        }
                        for (conn, inbox) in doomed {
                            // Fire-and-forget notification (no reply needed).
                            tokio::spawn(async move {
                                let _ = inbox.send(ConnIn::RoomGone(id)).await;
                                debug!(%conn, room = %id, "notified: room gone");
                            });
                        }
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
                        self.emit_metrics();
                        debug!(room = %id, "room destroyed");
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
                    reply,
                } => {
                    let Some(entry) = self.rooms.get(&room) else {
                        let _ = reply.send(Err(CoreError::RoomNotFound(room.0)));
                        continue;
                    };
                    let sharded_room = entry.shards.is_some();
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
                    if op_tx
                        .try_send(RoomOp::Join {
                            room,
                            handle,
                            shard: shard_idx,
                            out,
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
                    // The connection actor is gone for good: remove the entry
                    // entirely. If a dispatcher exists it performs the final
                    // leave itself (Close drains the queue); otherwise we
                    // send the leave directly.
                    let Some(info) = self.conns.remove(&conn) else {
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
                    let (room, entity) = (info.room, info.entity);
                    match self.conn_ops.remove(&conn) {
                        Some(op_tx) => {
                            let _ = op_tx.try_send(RoomOp::Close);
                        }
                        None => {
                            if let (Some(room), Some(entity)) = (room, entity) {
                                self.send_leave_direct(conn, room, entity);
                                // Sharded room: the registry's counter loses
                                // a member (a shard cannot count the room —
                                // see `ShardGroup`).
                                if let Some(e) =
                                    self.rooms.get_mut(&room).and_then(|e| e.shards.as_mut())
                                {
                                    e.members = e.members.saturating_sub(1);
                                }
                            }
                        }
                    }
                    self.reg_closes += 1;
                    self.emit_metrics();
                    debug!(%conn, "connection closed");
                }
                RegistryMsg::SpawnDone { conn, room, entity } => {
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
                RegistryMsg::SpawnFailed { conn, room } => {
                    // The room rejected the dispatched join (e.g. the
                    // shard's wire-id range is exhausted): release the
                    // capacity reservation (the join never counted).
                    if let Some(e) = self.rooms.get_mut(&room).and_then(|e| e.shards.as_mut()) {
                        e.pending = e.pending.saturating_sub(1);
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
                RegistryMsg::OpsClosed { conn } => {
                    self.conn_ops.remove(&conn);
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
                    for (id, entry) in self.rooms.drain() {
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
                    //    dropped inbox, harmlessly.
                    break;
                }
            }
        }
        debug!("registry actor stopped");
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

    /// One dispatcher task per connection with a room relationship in
    /// flight. It is the *only* sender of room control messages for that
    /// connection, so per-connection ordering (join → leave → rejoin) is
    /// guaranteed, and the registry never awaits a room from its own task.
    ///
    /// The dispatcher also mints the connection's **join epochs** (one per
    /// `Join` op, a local monotonic counter — see `crate::shard`,
    /// "Migration protocol"): the epoch travels with every `Join`/`Leave`
    /// of that join, and a sharded room's shards gate late migrations on
    /// it.
    fn spawn_conn_ops(
        conn: ConnectionId,
        registry: Mailbox<RegistryMsg>,
    ) -> mpsc::Sender<RoomOp<St>> {
        let (op_tx, mut op_rx) = mpsc::channel::<RoomOp<St>>(16);
        tokio::spawn(async move {
            let mut epoch: u64 = 0;
            // (room, entity, handle, the join's epoch)
            let mut in_room: Option<(RoomId, EntityId, RoomHandle<St>, u64)> = None;
            while let Some(op) = op_rx.recv().await {
                match op {
                    RoomOp::Join {
                        room,
                        handle,
                        shard,
                        out,
                        reply,
                    } => {
                        epoch = epoch.wrapping_add(1);
                        let (joined_tx, joined_rx) = oneshot::channel::<
                            Result<(EntityId, Mailbox<Action>), CoreError>,
                        >();
                        let sent = match &handle {
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
                            (true, Ok(Ok((entity, actions)))) => {
                                in_room = Some((room, entity, handle, epoch));
                                let _ = reply.send(Ok((entity, actions)));
                                let _ = registry
                                    .send(RegistryMsg::SpawnDone { conn, room, entity })
                                    .await;
                            }
                            // The room rejected the join structurally (a full
                            // room): propagate the room's error to the
                            // connection actor (it maps `RoomFull` to the
                            // `ERROR` frame's own code), record no room
                            // state, and — for a sharded room — release
                            // the registry's capacity reservation.
                            (true, Ok(Err(e))) => {
                                let _ = reply.send(Err(e));
                                let _ = registry
                                    .send(RegistryMsg::SpawnFailed { conn, room })
                                    .await;
                            }
                            _ => {
                                // Control channel gone (room destroyed) or the
                                // room dropped the reply.
                                let _ = reply.send(Err(CoreError::RoomGone));
                                let _ = registry
                                    .send(RegistryMsg::SpawnFailed { conn, room })
                                    .await;
                            }
                        }
                    }
                    RoomOp::Leave { room } => {
                        if let Some((r, entity, handle, ep)) = in_room.take()
                            && r == room
                        {
                            Self::send_room_leave(conn, entity, ep, handle).await;
                            let _ = registry
                                .send(RegistryMsg::LeaveDone { conn, room: r })
                                .await;
                        }
                    }
                    RoomOp::Close => {
                        if let Some((r, entity, handle, ep)) = in_room.take() {
                            Self::send_room_leave(conn, entity, ep, handle).await;
                            let _ = registry
                                .send(RegistryMsg::LeaveDone { conn, room: r })
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
}
