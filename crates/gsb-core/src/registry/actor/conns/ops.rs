//! The per-connection dispatcher task: it serializes that
//! connection's room operations so the registry never awaits a room.

use crate::channel::Mailbox;
use crate::error::CoreError;
use crate::id::ConnectionId;
use crate::registry::actor::Registry;
use crate::registry::*;
use std::fmt::Debug;
use std::hash::Hash;
use tokio::sync::mpsc;

#[cfg(test)]
mod tests;

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
    ///
    /// `seed` is the membership the task starts in: `None` for a new
    /// dispatcher, the table's affiliation for a replacement (see
    /// [`Self::respawn_conn_ops`]). `serial` names the task in its
    /// `OpsClosed` (see [`Self::on_ops_closed`]).
    pub(in crate::registry) fn spawn_conn_ops(
        conn: ConnectionId,
        registry: Mailbox<RegistryMsg>,
        seed: Option<InRoom<St, Sp>>,
        serial: u64,
    ) -> mpsc::Sender<RoomOp<St, Sp>> {
        let (op_tx, mut op_rx) = mpsc::channel::<RoomOp<St, Sp>>(16);
        tokio::spawn(async move {
            // (room, entity, handle, the join's epoch, the resume key)
            let mut in_room: Option<InRoom<St, Sp>> = seed;
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
                        input_rate,
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
                                identity.clone(),
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
                                let _ = reply.send(Ok(Seat {
                                    entity,
                                    actions,
                                    input_rate,
                                }));
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
                        // Compare BEFORE taking (B64): a leave for a room
                        // this connection is not in answers nothing, as
                        // ever, and keeps the membership it does hold.
                        if in_room.as_ref().is_some_and(|m| m.0 == room)
                            && let Some((r, entity, handle, ep, _id)) = in_room.take()
                        {
                            Self::send_room_leave(conn, entity, ep, handle).await;
                            let _ = registry
                                .send(RegistryMsg::LeaveDone { conn, room: r })
                                .await;
                        }
                    }
                    RoomOp::Close => break,
                }
            }
            // The end of this connection's room ops: its `Close`, or its
            // queue closing without one — the registry drops the only
            // sender when the connection closes (or at shutdown), also
            // when the queue was too full to take the `Close` (B61). Either
            // way every op queued ahead has run in order, so `in_room` is
            // the membership they left: transport death DETACHes it, not
            // leaves it — the ROOM's policy decides despawn-vs-hold (§3).
            if let Some((r, entity, handle, _ep, identity)) = in_room.take() {
                Self::send_room_detach(conn, entity, identity, handle).await;
                // The affiliation is KEPT (parked slot held, §4):
                // DetachDone marks the entry instead of clearing it.
                let _ = registry
                    .send(RegistryMsg::DetachDone { conn, room: r })
                    .await;
            }
            let _ = registry.send(RegistryMsg::OpsClosed { conn, serial }).await;
        });
        op_tx
    }

    /// Replace a GONE dispatcher (B63): its queue closed while the
    /// registry still held its sender, which then refused every later
    /// join of the connection. The fresh task takes the slot in
    /// `conn_ops` and is returned for the op the old one refused.
    ///
    /// The membership the old task held went with it — but the table
    /// still records the one it last reported. A connection joins only
    /// from outside a room, so a membership still on the table here is a
    /// leave the old task accepted and never ran. The fresh task starts
    /// in it and its first op is that leave: the room sees the leave
    /// before the retried join, from the one task, in order (a direct
    /// leave would race the join into the same room, and the room's
    /// entity guard would then drop it as stale). Its `LeaveDone` settles
    /// the row as any leave does. A membership only the old task knew (a
    /// join it never reported) is past recovery, as at close (B61).
    pub(in crate::registry) fn respawn_conn_ops(
        &mut self,
        conn: ConnectionId,
    ) -> mpsc::Sender<RoomOp<St, Sp>> {
        let held = self.conns.get(&conn).and_then(|i| {
            let (room, entity) = i.room.zip(i.entity)?;
            let handle = self.room_handle(room)?;
            // Epoch 0, as the dispatcher-less leave sends (`send_leave_direct`).
            Some((room, entity, handle, 0, i.identity.clone()))
        });
        let room = held.as_ref().map(|h| h.0);
        let op_tx = self.install_conn_ops(conn, held);
        if let Some(room) = room {
            // A fresh queue of 16 takes it (or the join fails with it).
            let _ = op_tx.try_send(RoomOp::Leave { room });
        }
        op_tx
    }

    /// Spawn a dispatcher under a fresh serial and put it in the
    /// connection's slot, replacing whatever the slot held; returns a
    /// sender for the op at hand.
    pub(in crate::registry) fn install_conn_ops(
        &mut self,
        conn: ConnectionId,
        seed: Option<InRoom<St, Sp>>,
    ) -> mpsc::Sender<RoomOp<St, Sp>> {
        self.next_ops_serial += 1;
        let serial = self.next_ops_serial;
        let op_tx = Self::spawn_conn_ops(conn, self.self_mailbox.clone(), seed, serial);
        self.conn_ops.insert(conn, (serial, op_tx.clone()));
        op_tx
    }

    /// A dispatcher task exited (`OpsClosed`): its slot goes — only if the
    /// slot is still that dispatcher's (B65). A dispatcher replaced while
    /// gone (B63) reports after its replacement took the slot; dropping
    /// the fresh one's sender would close its queue, and it would detach
    /// the live membership.
    pub(in crate::registry) fn on_ops_closed(&mut self, conn: ConnectionId, serial: u64) {
        if self.conn_ops.get(&conn).is_some_and(|(s, _)| *s == serial) {
            self.conn_ops.remove(&conn);
        }
    }
}
