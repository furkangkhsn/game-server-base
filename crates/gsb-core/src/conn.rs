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
//! per-connection action channel (non-blocking `try_send`: a flooding
//! client drops its own input, never stalls the actor or the room). It
//! never multiplexes: every branch is a channel receive.

use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::{debug, warn};

use gsb_protocol::op;
use gsb_protocol::{FrameBody, MessageTable, ProtoError, base};

use crate::channel::{FrameBatch, Inbox, Mailbox};
use crate::error::CoreError;
use crate::id::{ConnectionId, EntityId, RoomId};
use crate::metrics::{ConnSample, MetricsEvent};
use crate::registry::RegistryMsg;
use crate::room::Action;

/// How often an active connection flushes its wire-byte counters as a
/// sample (a connection with no inbound frames does not flush until its
/// final flush at close — it has nothing new to report in the meantime).
const METRICS_FLUSH_EVERY: Duration = Duration::from_millis(500);

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
    /// The room's per-connection action channel (set on join, cleared on
    /// leave / room-gone). Game-band opcodes are `try_send`-ed here.
    actions: Option<Mailbox<Action>>,
    /// Local wire-byte counters (see [`crate::metrics`]); flushed as
    /// deltas — on inbound frames at most once per `METRICS_FLUSH_EVERY`
    /// and a final time at close. The dominant outbound traffic (room
    /// fan-out) is counted by the room, not here.
    m_in_bytes: u64,
    m_in_frames: u64,
    m_out_bytes: u64,
    m_out_frames: u64,
    m_flushed_in_bytes: u64,
    m_flushed_in_frames: u64,
    m_flushed_out_bytes: u64,
    m_flushed_out_frames: u64,
    m_last_flush: Instant,
    /// Outbound metrics path (synchronous unbounded send — the actor's
    /// only await stays the inbox `recv`).
    metrics: mpsc::UnboundedSender<MetricsEvent>,
}

impl ConnectionActor {
    pub fn new(
        conn: ConnectionId,
        table: std::sync::Arc<MessageTable>,
        registry: Mailbox<RegistryMsg>,
        inbox: Inbox<ConnIn>,
        out: Mailbox<FrameBatch>,
        // Outbound metrics path (see `crate::metrics`).
        metrics: mpsc::UnboundedSender<MetricsEvent>,
    ) -> Self {
        Self {
            conn,
            state: ConnState::WaitingAuth,
            table,
            registry,
            inbox,
            out,
            actions: None,
            m_in_bytes: 0,
            m_in_frames: 0,
            m_out_bytes: 0,
            m_out_frames: 0,
            m_flushed_in_bytes: 0,
            m_flushed_in_frames: 0,
            m_flushed_out_bytes: 0,
            m_flushed_out_frames: 0,
            m_last_flush: Instant::now(),
            metrics,
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
                ConnIn::Frame(frame) => {
                    // Metrics: count this frame's wire bytes (frame body:
                    // 2-byte op + payload) before handling it.
                    self.m_in_bytes =
                        self.m_in_bytes.saturating_add(2 + frame.payload.len() as u64);
                    self.m_in_frames += 1;
                    self.maybe_flush_metrics(false);
                    self.handle_frame(frame).await
                }
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

        // Metrics: final flush of whatever is unflushed (marks the
        // connection's end).
        self.maybe_flush_metrics(true);

        // Cleanup: tell the registry so the player entity is despawned.
        let _ = self
            .registry
            .send(RegistryMsg::ConnClosed { conn: self.conn })
            .await;
    }

    /// Flush this connection's wire-byte counters as a delta sample when
    /// there is new data and the flush interval has passed (or unconditionally for the
    /// final flush). Synchronous: the only check is an `Instant`
    /// comparison on each inbound frame, so the actor's only await stays
    /// the inbox `recv`.
    fn maybe_flush_metrics(&mut self, last: bool) {
        let in_b = self.m_in_bytes - self.m_flushed_in_bytes;
        let in_f = self.m_in_frames - self.m_flushed_in_frames;
        let out_b = self.m_out_bytes - self.m_flushed_out_bytes;
        let out_f = self.m_out_frames - self.m_flushed_out_frames;
        if in_b == 0 && in_f == 0 && out_b == 0 && out_f == 0 {
            return;
        }
        if !last && Instant::now().duration_since(self.m_last_flush) < METRICS_FLUSH_EVERY {
            return;
        }
        self.m_flushed_in_bytes = self.m_in_bytes;
        self.m_flushed_in_frames = self.m_in_frames;
        self.m_flushed_out_bytes = self.m_out_bytes;
        self.m_flushed_out_frames = self.m_out_frames;
        self.m_last_flush = Instant::now();
        let _ = self.metrics.send(MetricsEvent::Conn(ConnSample {
            conn: self.conn,
            bytes_in: in_b,
            bytes_out: out_b,
            frames_in: in_f,
            frames_out: out_f,
            last,
        }));
    }

    fn detach(&mut self) {
        self.state = ConnState::Authed;
        self.actions = None;
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
                    oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
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
        let mailbox = match &self.actions {
            Some(mb) => mb,
            None => {
                // Not in a room (or the room went away): report it.
                self.reply_err(ProtoError::NotInRoom).await;
                return;
            }
        };
        // The payload is forwarded encoded; the game crate decodes it.
        // Non-blocking: a flooding connection drops its own input (bounded
        // per-connection memory) and never stalls its actor or the room. A
        // closed channel means the room is gone: detach.
        match mailbox.try_send(Action {
            conn: self.conn,
            op: frame.op,
            payload: frame.payload,
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!(%self.conn, op = frame.op, "action channel full; input dropped");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                warn!(%self.conn, "action channel closed while forwarding; detaching");
                self.detach();
            }
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
            // Metrics: count this control frame's wire bytes (frame body:
            // 2-byte op + payload). Room fan-out bytes are counted by the
            // room, not here.
            self.m_out_bytes =
                self.m_out_bytes.saturating_add(2 + fb.payload.len() as u64);
            self.m_out_frames += 1;
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
