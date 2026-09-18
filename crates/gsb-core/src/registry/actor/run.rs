//! The mailbox loop: the registry's ONE awaited source. The larger
//! arms delegate to sibling modules; the small ones answer inline
//! from the tables.

use std::fmt::Debug;
use std::hash::Hash;

use tracing::debug;

use crate::registry::*;

use crate::registry::actor::Registry;

mod shutdown;

impl<W, G, St, Sp> Registry<W, G, St, Sp>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
    St: Debug + Send + 'static,
    // The strip payload's trait bounds (`GameLogic::Strip`) — the
    // registry never inspects payloads, but both actor shapes it spawns
    // require them.
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Run until the mailbox is closed.
    pub async fn run(mut self) {
        debug!("registry actor started");
        while let Some(msg) = self.inbox.recv().await {
            match msg {
                RegistryMsg::CreateRoom { config, reply } => {
                    self.on_create_room(config, reply).await
                }
                RegistryMsg::DestroyRoom { id, reply } => self.on_destroy_room(id, reply).await,
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
                } => self.on_spawn_player(conn, room, out, identity, reply).await,
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
                RegistryMsg::ConnOpened { conn, inbox } => self.on_conn_opened(conn, inbox).await,
                RegistryMsg::Authed { conn } => {
                    // The connection finished AUTH: it leaves the
                    // unauthenticated pool (§4), freeing cap space for a
                    // new open. An absent entry means the connection was
                    // rejected at a cap or closed before authenticating —
                    // nothing to mark.
                    match self.conns.get_mut(&conn) {
                        Some(info) => {
                            info.authed = true;
                            debug!(
                                %conn,
                                "connection authenticated; leaves the unauthenticated pool"
                            );
                        }
                        None => {
                            debug!(%conn, "auth notice for an unregistered connection");
                        }
                    }
                }
                RegistryMsg::ConnClosed { conn } => self.on_conn_closed(conn).await,
                RegistryMsg::SpawnDone {
                    conn,
                    room,
                    entity,
                    generation,
                } => self.on_spawn_done(conn, room, entity, generation).await,
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
                        if let Some(e) = self.rooms.get_mut(&room).and_then(|e| e.shards.as_mut()) {
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
                RegistryMsg::DetachDespawned { conn, room } => {
                    // Guarded on BOTH marks: a row that is no longer
                    // detached (a resume re-affiliated it) or no longer in
                    // this room has already been settled by the event that
                    // changed it — this report is then a stale echo and
                    // must not touch anything.
                    let released = self
                        .conns
                        .get(&conn)
                        .is_some_and(|i| i.detached && i.room == Some(room));
                    if released {
                        self.conns.remove(&conn);
                        // The detached row was carrying this member (a
                        // detach deliberately does NOT decrement, §4), so
                        // the release hands the count back — the same
                        // accounting the resume-supersedence path does.
                        if let Some(e) = self.rooms.get_mut(&room).and_then(|e| e.shards.as_mut()) {
                            e.members = e.members.saturating_sub(1);
                        }
                        // The despawn went through the room's ordinary
                        // leave funnel, so it counts as a leave here too.
                        self.reg_leaves += 1;
                        self.emit_metrics();
                        debug!(
                            %conn,
                            room = %room,
                            "detach ended in despawn: row released (slot returned)"
                        );
                    }
                }
                RegistryMsg::OpsClosed { conn } => {
                    self.conn_ops.remove(&conn);
                }
                RegistryMsg::RoomDied {
                    id,
                    shard,
                    generation,
                } => self.on_room_died(id, shard, generation).await,
                RegistryMsg::Shutdown => {
                    self.on_shutdown().await;
                    break;
                }
            }
        }
        debug!("registry actor stopped");
    }
}
