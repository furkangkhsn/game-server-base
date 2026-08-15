//! The registry actor: the server's control plane.
//!
//! A single channel-driven actor that owns:
//! - the room table: `RoomId → (mailbox, pacer)`;
//! - the connection table: `ConnectionId → ConnInfo`;
//! - the [`RoomFactory`], which is how the (game-specific) room logic gets
//!   into the core without the core knowing any game types.
//!
//! No locks: every cross-actor value (mailboxes, one-shot replies) is moved
//! through channels.

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
    SpawnPlayer {
        conn: ConnectionId,
        room: RoomId,
        /// The connection's outbound channel, handed to the room for fan-out.
        out: mpsc::Sender<FrameBatch>,
        reply: oneshot::Sender<Result<(EntityId, Mailbox<RoomMsg>), CoreError>>,
    },
    /// Remove a player from its room (leave or disconnect).
    DespawnPlayer { conn: ConnectionId },
    /// A connection registered itself so the registry can notify it later.
    ConnOpened {
        conn: ConnectionId,
        inbox: Mailbox<ConnIn>,
    },
    /// A connection went away; despawn its entity if any.
    ConnClosed { conn: ConnectionId },
    /// Shut everything down: notify connections, destroy all rooms.
    Shutdown,
}

struct RoomEntry {
    mailbox: Mailbox<RoomMsg>,
    pacer: JoinHandle<()>,
}

#[derive(Default)]
struct ConnInfo {
    room: RoomId,
    /// Set via [`RegistryMsg::ConnOpened`].
    inbox: Option<Mailbox<ConnIn>>,
}

/// The registry actor.
pub struct Registry<W> {
    factory: RoomFactory<W>,
    inbox: Inbox<RegistryMsg>,
    rooms: HashMap<RoomId, RoomEntry>,
    conns: HashMap<ConnectionId, ConnInfo>,
}

impl<W> Registry<W>
where
    W: Send + 'static,
{
    pub fn new(inbox: Inbox<RegistryMsg>, factory: RoomFactory<W>) -> Self {
        Self {
            factory,
            inbox,
            rooms: HashMap::new(),
            conns: HashMap::new(),
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
                        // Notify this room's connections first.
                        let mut doomed = Vec::new();
                        for (conn, info) in self.conns.iter_mut() {
                            if info.room == id {
                                info.room = RoomId(0);
                                if let Some(inbox) = info.inbox.take() {
                                    doomed.push((*conn, inbox));
                                }
                            }
                        }
                        for (_, inbox) in doomed {
                            // Fire-and-forget notification (no reply needed).
                            tokio::spawn(async move {
                                let _ = inbox.send(ConnIn::RoomGone(id)).await;
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
                    // Extract owned handles first so no borrow of `self.rooms`
                    // spans the awaits below.
                    let (mailbox, entity_reply) = match self.rooms.get(&room) {
                        Some(entry) => {
                            let (tx, rx) = oneshot::channel::<EntityId>();
                            (entry.mailbox.clone(), (tx, rx))
                        }
                        None => {
                            let _ = reply.send(Err(CoreError::RoomNotFound(room.0)));
                            continue;
                        }
                    };
                    let (entity_tx, entity_rx) = entity_reply;
                    if mailbox
                        .send(RoomMsg::PlayerJoined {
                            conn,
                            out,
                            reply: entity_tx,
                        })
                        .await
                        .is_err()
                    {
                        let _ = reply.send(Err(CoreError::RoomGone));
                        continue;
                    }
                    match entity_rx.await {
                        Ok(entity) => {
                            self.conns.entry(conn).or_default().room = room;
                            let _ = reply.send(Ok((entity, mailbox)));
                            debug!(%conn, room = %room, entity, "player spawned");
                        }
                        Err(_) => {
                            let _ = reply.send(Err(CoreError::RoomGone));
                        }
                    }
                }
                RegistryMsg::DespawnPlayer { conn } => {
                    self.despawn(conn);
                }
                RegistryMsg::ConnOpened { conn, inbox } => {
                    let info = self.conns.entry(conn).or_default();
                    info.inbox = Some(inbox);
                }
                RegistryMsg::ConnClosed { conn } => {
                    self.despawn(conn);
                }
                RegistryMsg::Shutdown => {
                    warn!("registry shutting down");
                    let mut doomed = Vec::new();
                    for (_, info) in self.conns.iter_mut() {
                        if let Some(inbox) = info.inbox.take() {
                            doomed.push(inbox);
                        }
                    }
                    for inbox in doomed {
                        tokio::spawn(async move {
                            let _ = inbox.send(ConnIn::Shutdown).await;
                        });
                    }
                    self.conns.clear();
                    for (id, entry) in self.rooms.drain() {
                        entry.pacer.abort();
                        let _ = entry.mailbox.send(RoomMsg::Shutdown).await;
                        debug!(room = %id, "room stopped");
                    }
                }
            }
        }
        debug!("registry actor stopped");
    }

    /// Remove `conn` from the connection table and tell its room (if any) to
    /// drop the player entity. The mailbox send happens in a spawned task so
    /// no borrow of `self.rooms` is held across an await.
    fn despawn(&mut self, conn: ConnectionId) {
        let Some(mut info) = self.conns.remove(&conn) else {
            return;
        };
        let room = std::mem::replace(&mut info.room, RoomId(0));
        if room.0 != 0 {
            if let Some(mailbox) = self.rooms.get(&room).map(|e| e.mailbox.clone()) {
                tokio::spawn(async move {
                    let _ = mailbox.send(RoomMsg::PlayerLeft { conn }).await;
                });
            }
            debug!(%conn, room = %room, "player despawned");
        } else {
            debug!(%conn, "connection closed");
        }
    }
}
