//! The JOIN_ROOM_REQ path: admission, the reply, and the action-channel
//! wiring a joined session runs on.

use tokio::sync::oneshot;
use tracing::{debug, warn};

use gsb_protocol::op;
use gsb_protocol::{FrameBody, ProtoError, base};

use crate::conn::*;
use crate::error::CoreError;
use crate::id::RoomId;
use crate::metrics::MetricsEvent;
use crate::registry::{RegistryMsg, Seat};

impl super::ConnectionActor {
    /// The JOIN_ROOM_REQ arm of [`Self::handle_frame`]: admission into a
    /// room, the reply, and the action-channel wiring that follows it.
    pub(super) async fn handle_join(&mut self, frame: FrameBody) {
        if self.state != ConnState::Authed {
            self.reply_err(ProtoError::NotAuthenticated).await;
            return;
        }
        let join: base::JoinRoom = match self.decode::<base::JoinRoom>(frame.op, frame) {
            Ok(m) => m,
            Err(e) => {
                self.reply_err(e).await;
                return;
            }
        };
        let room = RoomId(join.room_id);
        // Ticket pin: a ticket-auth connection may only join the
        // room its ticket names (the platform set up THE match,
        // not "any room on this server"). A mismatch is a NORMAL
        // rejection (ERROR code 11 — the connection stays alive
        // and may join the pinned room); it is not a violation
        // (the frame is well-formed; the client simply aimed at
        // the wrong room).
        if let Some(v) = &self.ticket
            && room != v.room
        {
            warn!(%self.conn, room = %room, pinned = %v.room, "join rejected: ticket pins a different room");
            let _ = self
                .send_frame(
                    op::base::ERROR,
                    &base::Error::new(
                        base::ErrorCode::RoomMismatch,
                        format!(
                            "ticket pins room {} (the platform-set room); \
                             joining room {} is rejected",
                            v.room, room
                        ),
                    ),
                )
                .await;
            return;
        }
        let (reply_tx, reply_rx) = oneshot::channel::<Result<Seat, CoreError>>();
        if self
            .registry
            .send(RegistryMsg::SpawnPlayer {
                conn: self.conn,
                room,
                out: self.out.clone(),
                identity: self.identity.clone(),
                reply: reply_tx,
            })
            .await
            .is_err()
        {
            // The registry has stopped and closed its mailbox (F53): the
            // join was never queued, so nothing behind the registry can
            // count it — counted here, once (F54), stop-message idiom
            // (the collector may be draining its last events).
            crate::channel::post(&self.metrics, MetricsEvent::JoinUnsent);
            self.reply_err(ProtoError::Other("registry gone".into()))
                .await;
            return;
        }
        match reply_rx.await {
            Ok(Ok(Seat {
                entity,
                actions,
                input_rate,
            })) => {
                self.state = ConnState::InRoom { room };
                self.actions = Some(actions);
                // The room's input limit comes with its channel.
                self.enter_input_rate(input_rate);
                let _ = self
                    .send_frame(op::base::JOIN_ROOM_RESULT, &base::JoinRoomResult { entity })
                    .await;
                debug!(%self.conn, room = %room, entity, "joined room");
            }
            Ok(Err(e)) => {
                // `RoomFull` gets its own code (8) so the client can
                // tell "this room is full — pick another one or
                // retry later" (8) from a permanent failure like a
                // missing room (4). Either way the connection stays
                // alive: a gentle reject costs one small frame and
                // keeps the session reusable, while a silent close
                // looks like a network failure and sends the
                // client into a reconnect/backoff loop against a
                // server that is (by definition) already busy.
                // `RoomRetired` gets code 12 (§8): "definitively
                // over — return to the lobby, never retry", the
                // client decision ERROR 4 cannot express. A stale
                // resume (`ResumeStale`) is an ordinary rejection:
                // code 4 with its reason; the client re-auths and
                // its next join falls through to a fresh join.
                let _ = self
                    .send_frame(
                        op::base::ERROR,
                        &base::Error::new(e.wire_code(), e.to_string()),
                    )
                    .await;
            }
            Err(_) => {
                let _ = self
                    .send_frame(
                        op::base::ERROR,
                        &base::Error::new(base::ErrorCode::RoomOpFailed, "registry unavailable"),
                    )
                    .await;
            }
        }
    }

    /// The registry's [`ConnIn::LeftRoom`] (BACKLOG B40): the room ended
    /// this membership and the registry settled its row. Leave the room
    /// state the way the client's own `LEAVE_ROOM_REQ` would — nothing on
    /// the wire — so the next `JOIN_ROOM_REQ` is admitted (not the hard
    /// `ERROR 3` a join from inside a room gets) and a game frame is
    /// answered as any frame outside a room is (`ERROR 6`, race class).
    ///
    /// Guarded: only the membership in `room` whose action channel the
    /// room has closed (or that a closed forward already dropped). After
    /// a leave and a new join in between, the connection holds the new
    /// membership's OPEN channel and the notice is stale.
    pub(super) fn on_left_room(&mut self, room: RoomId) {
        let ours = self.state == ConnState::InRoom { room }
            && self.actions.as_ref().is_none_or(|a| a.is_closed());
        if ours {
            self.detach();
            debug!(%self.conn, room = %room, "the room ended the membership; the connection stays");
        } else {
            debug!(%self.conn, room = %room, "stale left-room notice ignored");
        }
    }
}
