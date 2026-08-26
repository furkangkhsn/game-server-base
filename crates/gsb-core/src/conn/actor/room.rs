//! The JOIN_ROOM_REQ path: admission, the reply, and the action-channel
//! wiring a joined session runs on.


use tokio::sync::oneshot;
use tracing::{debug, warn};

use gsb_protocol::op;
use gsb_protocol::{FrameBody, ProtoError, base};

use crate::channel::Mailbox;
use crate::error::CoreError;
use crate::id::{EntityId, RoomId};
use crate::registry::RegistryMsg;
use crate::room::Action;
use crate::conn::*;


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
                    &base::Error {
                        code: 11,
                        message: format!(
                            "ticket pins room {} (the platform-set room); \
                             joining room {} is rejected",
                            v.room, room
                        ),
                    },
                )
                .await;
            return;
        }
        let (reply_tx, reply_rx) =
            oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
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
            self.reply_err(ProtoError::Other("registry gone".into()))
                .await;
            return;
        }
        match reply_rx.await {
            Ok(Ok((entity, actions))) => {
                self.state = ConnState::InRoom { room };
                self.actions = Some(actions);
                let _ = self
                    .send_frame(
                        op::base::JOIN_ROOM_RESULT,
                        &base::JoinRoomResult { entity },
                    )
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
                let code = match &e {
                    CoreError::RoomFull(_) => 8,
                    CoreError::RoomRetired(_) => 12,
                    _ => 4,
                };
                let _ = self
                    .send_frame(
                        op::base::ERROR,
                        &base::Error {
                            code,
                            message: e.to_string(),
                        },
                    )
                    .await;
            }
            Err(_) => {
                let _ = self
                    .send_frame(
                        op::base::ERROR,
                        &base::Error {
                            code: 4,
                            message: "registry unavailable".into(),
                        },
                    )
                    .await;
            }
        }
    }
}
