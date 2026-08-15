//! The connection actor: one per client connection.
//!
//! Three tasks cooperate per connection (the "1 reader + 1 writer" pattern):
//!
//! ```text
//! socket read half ──▶ [reader pump] ──InMsg::Frame──▶ connection actor ◀──RegistryMsg── registry
//!                                                        │  (mailbox-driven; its
//! socket write half ◀── [writer pump] ◀──FrameBody─────┘   only await is recv)
//! room broadcast fan-out channels ─────────────────────────┘
//! ```
//!
//! The actor decodes the envelope, runs the auth/join/leave state machine
//! against the registry, and forwards game-band opcodes to the room's
//! mailbox. It never multiplexes: every branch is a channel receive.

use tokio::sync::oneshot;
use tracing::{debug, warn};

use gsb_protocol::op;
use gsb_protocol::{FrameBody, MessageTable, ProtoError, base};

use crate::channel::{FrameBatch, Inbox, Mailbox};
use crate::error::CoreError;
use crate::id::{ConnectionId, EntityId, RoomId};
use crate::registry::RegistryMsg;
use crate::room::RoomMsg;

/// Messages addressed to the connection actor.
#[derive(Debug)]
pub enum ConnIn {
    /// A frame decoded from the network (envelope intact).
    Frame(FrameBody),
    /// The peer closed or the socket errored; the actor should clean up.
    Closed { reason: String },
    /// The room the connection was in got destroyed.
    RoomGone(RoomId),
    /// Server-wide shutdown.
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnState {
    WaitingAuth,
    Authed,
    InRoom { room: RoomId },
}

/// The connection actor.
pub struct ConnectionActor {
    conn: ConnectionId,
    state: ConnState,
    table: std::sync::Arc<MessageTable>,
    registry: Mailbox<RegistryMsg>,
    inbox: Inbox<ConnIn>,
    /// To the writer pump; also cloned to the room for fan-out.
    out: Mailbox<FrameBatch>,
    room_mailbox: Option<Mailbox<RoomMsg>>,
}

impl ConnectionActor {
    pub fn new(
        conn: ConnectionId,
        table: std::sync::Arc<MessageTable>,
        registry: Mailbox<RegistryMsg>,
        inbox: Inbox<ConnIn>,
        out: Mailbox<FrameBatch>,
    ) -> Self {
        Self {
            conn,
            state: ConnState::WaitingAuth,
            table,
            registry,
            inbox,
            out,
            room_mailbox: None,
        }
    }

    /// Run until the peer is gone or the server shuts down.
    ///
    /// Note: the accept loop sends [`RegistryMsg::ConnOpened`] (it owns the
    /// sender half of this actor's inbox) *before* spawning the actor, so
    /// ordering with the first client frame is guaranteed.
    pub async fn run(mut self) {
        while let Some(msg) = self.inbox.recv().await {
            match msg {
                ConnIn::Frame(frame) => self.handle_frame(frame).await,
                ConnIn::Closed { reason } => {
                    debug!(%self.conn, %reason, "connection closed by peer/io");
                    break;
                }
                ConnIn::RoomGone(room) => {
                    warn!(%self.conn, room = %room, "room destroyed; detaching");
                    self.detach();
                    let _ = self
                        .send_frame(
                            op::base::ERROR,
                            &base::Error {
                                code: 5,
                                message: "room destroyed".into(),
                            },
                        )
                        .await;
                    break;
                }
                ConnIn::Shutdown => {
                    debug!(%self.conn, "connection shutdown (server)");
                    break;
                }
            }
        }

        // Cleanup: tell the registry so the player entity is despawned.
        let _ = self
            .registry
            .send(RegistryMsg::ConnClosed { conn: self.conn })
            .await;
    }

    fn detach(&mut self) {
        self.state = ConnState::Authed;
        self.room_mailbox = None;
    }

    async fn handle_frame(&mut self, frame: FrameBody) {
        match frame.op {
            op::base::AUTH_REQ => {
                if self.state != ConnState::WaitingAuth {
                    self.reply_err(ProtoError::AlreadyAuthenticated).await;
                    return;
                }
                let auth: base::Auth = match self.decode::<base::Auth>(frame.op, frame) {
                    Ok(m) => m,
                    Err(e) => {
                        self.reply_err(e).await;
                        return;
                    }
                };
                self.state = ConnState::Authed;
                debug!(%self.conn, name = %auth.name, "authenticated");
                let _ = self
                    .send_frame(
                        op::base::AUTH_RESULT,
                        &base::AuthResult {
                            ok: true,
                            reason: String::new(),
                        },
                    )
                    .await;
            }
            op::base::JOIN_ROOM_REQ => {
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
                let (reply_tx, reply_rx) =
                    oneshot::channel::<Result<(EntityId, Mailbox<RoomMsg>), CoreError>>();
                if self
                    .registry
                    .send(RegistryMsg::SpawnPlayer {
                        conn: self.conn,
                        room,
                        out: self.out.clone(),
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
                    Ok(Ok((entity, mailbox))) => {
                        self.state = ConnState::InRoom { room };
                        self.room_mailbox = Some(mailbox);
                        let _ = self
                            .send_frame(
                                op::base::JOIN_ROOM_RESULT,
                                &base::JoinRoomResult { entity },
                            )
                            .await;
                        debug!(%self.conn, room = %room, entity, "joined room");
                    }
                    Ok(Err(e)) => {
                        let _ = self
                            .send_frame(
                                op::base::ERROR,
                                &base::Error {
                                    code: 4,
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
            op::base::LEAVE_ROOM_REQ => {
                let ConnState::InRoom { room } = self.state else {
                    self.reply_err(ProtoError::NotInRoom).await;
                    return;
                };
                self.detach();
                let _ = self
                    .registry
                    .send(RegistryMsg::DespawnPlayer { conn: self.conn })
                    .await;
                let _ = self
                    .send_frame(op::base::LEAVE_ROOM_RESULT, &base::LeaveRoomResult {})
                    .await;
                debug!(%self.conn, room = %room, "left room");
            }
            op::base::HEARTBEAT => {
                let hb: base::Heartbeat = match self.decode::<base::Heartbeat>(frame.op, frame) {
                    Ok(m) => m,
                    Err(e) => {
                        self.reply_err(e).await;
                        return;
                    }
                };
                let _ = self
                    .send_frame(
                        op::base::HEARTBEAT_ACK,
                        &base::HeartbeatAck { tick: hb.tick },
                    )
                    .await;
            }
            _ => self.forward_to_room(frame).await,
        }
    }

    async fn forward_to_room(&mut self, frame: FrameBody) {
        let mailbox = match &self.room_mailbox {
            Some(mb) => mb,
            None => {
                self.reply_err(ProtoError::NotInRoom).await;
                return;
            }
        };
        // The payload is forwarded encoded; the game crate decodes it.
        if mailbox
            .send(RoomMsg::Action {
                conn: self.conn,
                op: frame.op,
                payload: frame.payload,
            })
            .await
            .is_err()
        {
            warn!(%self.conn, "room mailbox closed while forwarding action");
            self.detach();
        }
    }

    /// Decode a frame body into a concrete message type via the table.
    fn decode<T: std::any::Any + Send + 'static>(
        &self,
        op: u16,
        frame: FrameBody,
    ) -> Result<T, ProtoError> {
        let decoded = self.table.decode(op, &frame.payload)?;
        decoded
            .downcast::<T>()
            .map(|b| *b)
            .map_err(|_| ProtoError::Other("type mismatch in message table".into()))
    }

    async fn send_frame<M: prost::Message + std::any::Any>(&mut self, op: u16, msg: &M) {
        if let Some(fb) = self.table.frame(op, msg) {
            let _ = self.out.send(vec![fb]).await;
        }
    }

    async fn reply_err(&mut self, e: ProtoError) {
        let (code, message) = match &e {
            ProtoError::UnknownOpcode(_) => (1, e.to_string()),
            ProtoError::Decode { .. } => (2, e.to_string()),
            ProtoError::NotAuthenticated => (3, e.to_string()),
            ProtoError::AlreadyAuthenticated => (3, e.to_string()),
            ProtoError::RoomNotFound(_) => (4, e.to_string()),
            ProtoError::NotInRoom => (6, e.to_string()),
            _ => (7, e.to_string()),
        };
        let _ = self
            .send_frame(op::base::ERROR, &base::Error { code, message })
            .await;
    }
}
