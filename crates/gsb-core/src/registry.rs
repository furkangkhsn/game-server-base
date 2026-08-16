//! The registry actor: the server's control plane.
//!
//! A single channel-driven actor that owns:
//! - the room table: `RoomId → (mailbox, pacer)`;
//! - the connection table: `ConnectionId → ConnInfo` (kept for the
//!   connection's *whole* lifetime, so the notification path — the inbox —
//!   is never lost mid-session);
//! - the relationship dispatchers: one small task per connection that has
//!   a room relationship in flight, serializing that connection's
//!   join/leave operations (see [`RoomOp`]);
//! - the [`RoomFactory`], which is how the (game-specific) room logic gets
//!   into the core without the core knowing any game types.
//!
//! No locks: every cross-actor value (mailboxes, one-shot replies) is moved
//! through channels. In particular the registry **never awaits a room**:
//! `SpawnPlayer` hands the room round-trip to the connection's dispatcher
//! and returns immediately, so one slow room can never block the control
//! plane (joins elsewhere, room creation, shutdown).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::channel::{FrameBatch, Inbox, Mailbox, channel};
use crate::conn::ConnIn;
use crate::error::CoreError;
use crate::id::{ConnectionId, EntityId, RoomId};
use crate::room::{RoomActor, RoomConfig, RoomLogic, RoomMsg, spawn_pacer};

/// Builds a room's world + logic. Provided by the composition root; the core
/// never names the concrete game types.
pub type RoomFactory<W> =
    Arc<dyn Fn(RoomId, &RoomConfig) -> (W, Box<dyn RoomLogic<W>>) + Send + Sync>;

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
    /// Spawn a player entity in a room and report the entity + room mailbox.
    ///
    /// Non-blocking with respect to the room: the round-trip is dispatched
    /// to the connection's relationship task and the reply may arrive
    /// later. A slow room can therefore never stall the registry.
    SpawnPlayer {
        conn: ConnectionId,
        room: RoomId,
        /// The connection's outbound channel, handed to the room for fan-out.
        out: mpsc::Sender<FrameBatch>,
        reply: oneshot::Sender<Result<(EntityId, Mailbox<RoomMsg>), CoreError>>,
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
    /// Join `room`: round-trip `PlayerJoined`, reply to the connection
    /// actor, report [`RegistryMsg::SpawnDone`] to the registry.
    Join {
        room: RoomId,
        room_mailbox: Mailbox<RoomMsg>,
        out: mpsc::Sender<FrameBatch>,
        reply: oneshot::Sender<Result<(EntityId, Mailbox<RoomMsg>), CoreError>>,
    },
    /// Leave `room`: send `PlayerLeft` (with the entity this dispatcher saw
    /// the join create), then report [`RegistryMsg::LeaveDone`].
    Leave { room: RoomId },
    /// Drain the queue (processing whatever is left, including a final
    /// leave), report [`RegistryMsg::OpsClosed`], exit.
    Close,
}

struct RoomEntry {
    mailbox: Mailbox<RoomMsg>,
    pacer: JoinHandle<()>,
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
pub struct Registry<W> {
    factory: RoomFactory<W>,
    inbox: Inbox<RegistryMsg>,
    /// Sender half of our own mailbox: cloned to dispatcher tasks so they
    /// can report back.
    self_mailbox: Mailbox<RegistryMsg>,
    rooms: HashMap<RoomId, RoomEntry>,
    conns: HashMap<ConnectionId, ConnInfo>,
    conn_ops: HashMap<ConnectionId, mpsc::Sender<RoomOp>>,
}

impl<W> Registry<W>
where
    W: Send + 'static,
{
    pub fn new(
        inbox: Inbox<RegistryMsg>,
        self_mailbox: Mailbox<RegistryMsg>,
        factory: RoomFactory<W>,
    ) -> Self {
        Self {
            factory,
            inbox,
            self_mailbox,
            rooms: HashMap::new(),
            conns: HashMap::new(),
            conn_ops: HashMap::new(),
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
                    let (world, logic) = (self.factory)(id, &config);
                    let (mailbox, inbox) = channel(config.mailbox_capacity);
                    let pacer = spawn_pacer(mailbox.clone(), config.period());
                    tokio::spawn(RoomActor::new(config, world, logic, inbox).run());
                    self.rooms.insert(id, RoomEntry { mailbox, pacer });
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
                        entry.pacer.abort();
                        let _ = entry.mailbox.send(RoomMsg::Shutdown).await;
                        debug!(room = %id, "room destroyed");
                    }
                }
                RegistryMsg::SpawnPlayer {
                    conn,
                    room,
                    out,
                    reply,
                } => {
                    let Some(mailbox) = self.rooms.get(&room).map(|e| e.mailbox.clone()) else {
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
                            room_mailbox: mailbox,
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
                                && let Some(mailbox) =
                                    self.rooms.get(&room).map(|e| e.mailbox.clone())
                            {
                                tokio::spawn(async move {
                                    let _ =
                                        mailbox.send(RoomMsg::PlayerLeft { conn, entity }).await;
                                });
                            }
                        }
                    }
                    debug!(%conn, "connection closed");
                }
                RegistryMsg::SpawnDone { conn, room, entity } => {
                    // Ordered per-connection (from the dispatcher). If the
                    // connection is unknown it died mid-join; the
                    // dispatcher's Close already cleaned up the room side.
                    if let Some(info) = self.conns.get_mut(&conn) {
                        info.room = Some(room);
                        info.entity = Some(entity);
                        debug!(%conn, room = %room, %entity, "player spawned");
                    }
                }
                RegistryMsg::LeaveDone { conn, room } => {
                    if let Some(info) = self.conns.get_mut(&conn)
                        && info.room == Some(room)
                    {
                        info.room = None;
                        info.entity = None;
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
                    // 3. Stop every room.
                    for (id, entry) in self.rooms.drain() {
                        entry.pacer.abort();
                        let _ = entry.mailbox.send(RoomMsg::Shutdown).await;
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
            if let Some(mailbox) = self.rooms.get(&room).map(|e| e.mailbox.clone()) {
                tokio::spawn(async move {
                    let _ = mailbox.send(RoomMsg::PlayerLeft { conn, entity }).await;
                });
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
    /// flight. It is the *only* sender of room messages for that connection,
    /// so per-connection ordering (join → leave → rejoin) is guaranteed,
    /// and the registry never awaits a room from its own task.
    fn spawn_conn_ops(conn: ConnectionId, registry: Mailbox<RegistryMsg>) -> mpsc::Sender<RoomOp> {
        let (op_tx, mut op_rx) = mpsc::channel::<RoomOp>(16);
        tokio::spawn(async move {
            let mut in_room: Option<(RoomId, EntityId, Mailbox<RoomMsg>)> = None;
            while let Some(op) = op_rx.recv().await {
                match op {
                    RoomOp::Join {
                        room,
                        room_mailbox,
                        out,
                        reply,
                    } => {
                        let (entity_tx, entity_rx) = oneshot::channel::<EntityId>();
                        let joined = room_mailbox
                            .send(RoomMsg::PlayerJoined {
                                conn,
                                out,
                                reply: entity_tx,
                            })
                            .await
                            .is_ok();
                        match (joined, entity_rx.await) {
                            (true, Ok(entity)) => {
                                in_room = Some((room, entity, room_mailbox.clone()));
                                let _ = reply.send(Ok((entity, room_mailbox)));
                                let _ = registry
                                    .send(RegistryMsg::SpawnDone { conn, room, entity })
                                    .await;
                            }
                            _ => {
                                let _ = reply.send(Err(CoreError::RoomGone));
                            }
                        }
                    }
                    RoomOp::Leave { room } => {
                        if let Some((r, entity, mailbox)) = in_room.take()
                            && r == room
                        {
                            let _ = mailbox.send(RoomMsg::PlayerLeft { conn, entity }).await;
                            let _ = registry
                                .send(RegistryMsg::LeaveDone { conn, room: r })
                                .await;
                        }
                    }
                    RoomOp::Close => {
                        if let Some((r, entity, mailbox)) = in_room.take() {
                            let _ = mailbox.send(RoomMsg::PlayerLeft { conn, entity }).await;
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
