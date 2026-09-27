//! Leaving a room, by every route: the direct table update, and the
//! sends that reach a single room or every shard of one.

use std::fmt::Debug;
use std::hash::Hash;

use tracing::debug;

use crate::id::{ConnectionId, EntityId, RoomId};
use crate::registry::*;
use crate::room::RoomControl;
use crate::shard::ShardMsg;

use crate::registry::actor::Registry;

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
    /// Leave without a dispatcher (no join/leave is in flight for this
    /// connection, so the table can be updated synchronously).
    pub(super) fn direct_leave(&mut self, conn: ConnectionId) {
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

    /// The room side a dispatcher-less send reaches: the single room's
    /// control channel or every shard's mailbox; `None` for a room the
    /// table no longer holds.
    pub(super) fn room_handle(&self, room: RoomId) -> Option<RoomHandle<St, Sp>> {
        let e = self.rooms.get(&room)?;
        match (&e.control, &e.shards) {
            (Some(control), _) => Some(RoomHandle::Single(control.clone())),
            (None, Some(group)) => Some(RoomHandle::Sharded(group.mailboxes.clone())),
            (None, None) => None,
        }
    }

    /// Send a leave with no dispatcher (the `ConnClosed` /
    /// [`Self::direct_leave`] paths): to the room's control channel, or —
    /// for a sharded room — to ALL of its shards (exactly one of them owns
    /// the connection; the entity-id guard makes the others no-ops, and
    /// the leave's epoch is 0, which can only lower the tombstones the
    /// join itself already recorded — see `crate::shard`, "Migration
    /// protocol").
    pub(super) fn send_leave_direct(&mut self, conn: ConnectionId, room: RoomId, entity: EntityId) {
        let Some(handle) = self.room_handle(room) else {
            return;
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
    pub(super) fn send_detach_direct(
        &mut self,
        conn: ConnectionId,
        room: RoomId,
        entity: EntityId,
    ) {
        let Some(identity) = self.conns.get(&conn).map(|i| i.identity.clone()) else {
            return;
        };
        let Some(handle) = self.room_handle(room) else {
            return;
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

    /// Send a leave for a dispatcher-held room affiliation: to the single
    /// room's control channel, or — for a sharded room — to ALL of its
    /// shards (exactly one owns the connection; the entity-id guard makes
    /// the others no-ops, and the epoch travels so a late migration of the
    /// same join is rejected — see `crate::shard`).
    pub(super) async fn send_room_leave(
        conn: ConnectionId,
        entity: EntityId,
        epoch: u64,
        handle: RoomHandle<St, Sp>,
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
    pub(super) async fn send_room_detach(
        conn: ConnectionId,
        entity: EntityId,
        identity: String,
        handle: RoomHandle<St, Sp>,
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
}
