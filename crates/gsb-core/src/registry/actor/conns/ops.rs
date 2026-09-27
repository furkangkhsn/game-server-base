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
    pub(in crate::registry) fn spawn_conn_ops(
        conn: ConnectionId,
        registry: Mailbox<RegistryMsg>,
    ) -> mpsc::Sender<RoomOp<St, Sp>> {
        let (op_tx, mut op_rx) = mpsc::channel::<RoomOp<St, Sp>>(16);
        tokio::spawn(async move {
            // (room, entity, handle, the join's epoch, the resume key)
            let mut in_room: Option<InRoom<St, Sp>> = None;
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
                        if let Some((r, entity, handle, ep, _id)) = in_room.take()
                            && r == room
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
            let _ = registry.send(RegistryMsg::OpsClosed { conn }).await;
        });
        op_tx
    }
}
