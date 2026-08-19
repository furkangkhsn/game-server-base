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
use crate::ticker::Ticker;

/// Builds a room's world + logic. Provided by the composition root; the core
/// never names the concrete game types. `G` is the game logic's group key
/// ([`RoomLogic::GroupKey`]); the room stores per-group state under it.
pub type RoomFactory<W, G> =
    Arc<dyn Fn(RoomId, &RoomConfig) -> (W, Box<dyn RoomLogic<W, GroupKey = G>>) + Send + Sync>;

/// Messages addressed to the registry actor.
#[derive(Debug)]
pub enum RegistryMsg {
    /// Create and start a room.
    CreateRoom {
        config: RoomConfig,
        reply: oneshot::Sender<Result<RoomId, CoreError>>,
    },
    /// Shut down a room (its players' entities are dropped; connections are
    /// notified via [`ConnIn::RoomGone`]).
    DestroyRoom { id: RoomId },
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
    /// A dispatched leave completed: clear the affiliation.
    LeaveDone { conn: ConnectionId, room: RoomId },
    /// A connection's dispatcher task exited; drop its slot.
    OpsClosed { conn: ConnectionId },
}

/// Operations on a connection's room relationship, processed by that
/// connection's dispatcher task — **in order**, which is what makes
/// leave→rejoin race-free: a `Leave` can never overtake (or be overtaken
/// by) the `Join` it follows.
enum RoomOp {
    /// Join `room`: round-trip the control `Join`, reply to the connection
    /// actor (with the per-connection action channel), report
    /// [`RegistryMsg::SpawnDone`] to the registry.
    Join {
        room: RoomId,
        room_control: Mailbox<RoomControl>,
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

struct RoomEntry {
    control: Mailbox<RoomControl>,
}

#[derive(Default)]
struct ConnInfo {
    room: Option<RoomId>,
    entity: Option<EntityId>,
    /// Set via [`RegistryMsg::ConnOpened`]; kept for the connection's whole
    /// lifetime so `RoomGone`/`Shutdown` can always reach it.
    inbox: Option<Mailbox<ConnIn>>,
}

/// The registry actor.
pub struct Registry<W, G> {
    factory: RoomFactory<W, G>,
    inbox: Inbox<RegistryMsg>,
    /// Sender half of our own mailbox: cloned to dispatcher tasks so they
    /// can report back.
    self_mailbox: Mailbox<RegistryMsg>,
    rooms: HashMap<RoomId, RoomEntry>,
    conns: HashMap<ConnectionId, ConnInfo>,
    conn_ops: HashMap<ConnectionId, mpsc::Sender<RoomOp>>,
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
}

impl<W, G> Registry<W, G>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
{
    pub fn new(
        inbox: Inbox<RegistryMsg>,
        self_mailbox: Mailbox<RegistryMsg>,
        factory: RoomFactory<W, G>,
        ticker: Ticker,
        // Outbound metrics path (see `crate::metrics`): a bounded channel;
        // the registry sends with the synchronous `try_send` (no await).
        metrics: mpsc::Sender<MetricsEvent>,
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

    /// Run until the mailbox is closed.
    pub async fn run(mut self) {
        debug!("registry actor started");
        while let Some(msg) = self.inbox.recv().await {
            match msg {
                RegistryMsg::CreateRoom { config, reply } => {
                    let id = config.id;
                    if self.rooms.contains_key(&id) {
                        let _ = reply.send(Err(CoreError::RoomExists(id.0)));
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
                    let (world, logic) = (self.factory)(id, &config);
                    let (control_tx, control_rx) = channel(config.control_capacity);
                    tokio::spawn(
                        RoomActor::new(
                            config,
                            world,
                            logic,
                            self.ticker.subscribe(),
                            control_rx,
                            run_every,
                            self.metrics.clone(),
                        )
                        .run(),
                    );
                    self.rooms.insert(
                        id,
                        RoomEntry {
                            control: control_tx,
                        },
                    );
                    self.reg_created += 1;
                    self.emit_metrics();
                    debug!(room = %id, "room created");
                    let _ = reply.send(Ok(id));
                }
                RegistryMsg::DestroyRoom { id } => {
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
                        let _ = entry.control.send(RoomControl::Shutdown).await;
                        self.reg_destroyed += 1;
                        self.emit_metrics();
                        debug!(room = %id, "room destroyed");
                    }
                }
                RegistryMsg::SpawnPlayer {
                    conn,
                    room,
                    out,
                    reply,
                } => {
                    let Some(control) = self.rooms.get(&room).map(|e| e.control.clone()) else {
                        let _ = reply.send(Err(CoreError::RoomNotFound(room.0)));
                        continue;
                    };
                    let op_tx = self
                        .conn_ops
                        .entry(conn)
                        .or_insert_with(|| Self::spawn_conn_ops(conn, self.self_mailbox.clone()));
                    if op_tx
                        .try_send(RoomOp::Join {
                            room,
                            room_control: control,
                            out,
                            reply,
                        })
                        .is_err()
                    {
                        // Op queue full (pathological): the op (and its reply)
                        // is dropped; the connection actor observes the
                        // dropped reply and sends an ERROR frame.
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
                    let (room, entity) = self
                        .conns
                        .remove(&conn)
                        .map(|i| (i.room, i.entity))
                        .unwrap_or((None, None));
                    match self.conn_ops.remove(&conn) {
                        Some(op_tx) => {
                            let _ = op_tx.try_send(RoomOp::Close);
                        }
                        None => {
                            if let (Some(room), Some(entity)) = (room, entity)
                                && let Some(control) =
                                    self.rooms.get(&room).map(|e| e.control.clone())
                            {
                                tokio::spawn(async move {
                                    let _ = control.send(RoomControl::Leave { conn, entity }).await;
                                });
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
                    // (The `info` borrow is scoped to the block so the
                    // `&mut self` `emit_metrics` call below does not conflict
                    // with it.)
                    // (The `info` borrow is scoped inside the `match` so the
                    // `&mut self` `emit_metrics` call below does not conflict
                    // with it.)
                    let spawned = match self.conns.get_mut(&conn) {
                        Some(info) => {
                            info.room = Some(room);
                            info.entity = Some(entity);
                            true
                        }
                        None => false,
                    };
                    if spawned {
                        self.reg_joins += 1;
                        self.emit_metrics();
                        debug!(%conn, room = %room, %entity, "player spawned");
                    }
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
                    // 3. Stop every room via its control channel (processed
                    //    on the next tick; the composition root aborts the
                    //    ticker afterwards, which closes the broadcast as a
                    //    backstop for any room that misses the window).
                    for (id, entry) in self.rooms.drain() {
                        let _ = entry.control.send(RoomControl::Shutdown).await;
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
            if let Some(control) = self.rooms.get(&room).map(|e| e.control.clone()) {
                tokio::spawn(async move {
                    let _ = control.send(RoomControl::Leave { conn, entity }).await;
                });
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

    /// One dispatcher task per connection with a room relationship in
    /// flight. It is the *only* sender of room control messages for that
    /// connection, so per-connection ordering (join → leave → rejoin) is
    /// guaranteed, and the registry never awaits a room from its own task.
    fn spawn_conn_ops(conn: ConnectionId, registry: Mailbox<RegistryMsg>) -> mpsc::Sender<RoomOp> {
        let (op_tx, mut op_rx) = mpsc::channel::<RoomOp>(16);
        tokio::spawn(async move {
            let mut in_room: Option<(RoomId, EntityId, Mailbox<RoomControl>)> = None;
            while let Some(op) = op_rx.recv().await {
                match op {
                    RoomOp::Join {
                        room,
                        room_control,
                        out,
                        reply,
                    } => {
                        let (joined_tx, joined_rx) =
                            oneshot::channel::<(EntityId, Mailbox<Action>)>();
                        let sent = room_control
                            .send(RoomControl::Join {
                                conn,
                                out,
                                reply: joined_tx,
                            })
                            .await
                            .is_ok();
                        match (sent, joined_rx.await) {
                            (true, Ok((entity, actions))) => {
                                in_room = Some((room, entity, room_control));
                                let _ = reply.send(Ok((entity, actions)));
                                let _ = registry
                                    .send(RegistryMsg::SpawnDone { conn, room, entity })
                                    .await;
                            }
                            _ => {
                                // Control channel gone (room destroyed) or the
                                // room dropped the reply.
                                let _ = reply.send(Err(CoreError::RoomGone));
                            }
                        }
                    }
                    RoomOp::Leave { room } => {
                        if let Some((r, entity, control)) = in_room.take()
                            && r == room
                        {
                            let _ = control.send(RoomControl::Leave { conn, entity }).await;
                            let _ = registry
                                .send(RegistryMsg::LeaveDone { conn, room: r })
                                .await;
                        }
                    }
                    RoomOp::Close => {
                        if let Some((r, entity, control)) = in_room.take() {
                            let _ = control.send(RoomControl::Leave { conn, entity }).await;
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
}
