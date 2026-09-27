//! Birth, the mailbox loop, the periodic metrics flush, and the
//! detach that ends a session without ending its entity.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Instant;

use tokio::sync::mpsc;
use tracing::{debug, warn};

use gsb_protocol::op;
use gsb_protocol::{MessageTable, base};

use crate::channel::{FrameBatch, Inbox, Mailbox};
use crate::conn::*;
use crate::id::ConnectionId;
use crate::metrics::{ConnSample, MetricsEvent};
use crate::registry::RegistryMsg;

impl super::ConnectionActor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        conn: ConnectionId,
        // The peer's address (for the violation-close signal; see the
        // `peer` field). The accept loop learns it from the transport.
        peer: SocketAddr,
        table: std::sync::Arc<MessageTable>,
        registry: Mailbox<RegistryMsg>,
        inbox: Inbox<ConnIn>,
        out: Mailbox<FrameBatch>,
        // Outbound metrics path (see `crate::metrics`): a bounded channel;
        // the actor sends with the synchronous `try_send` (no await).
        metrics: mpsc::Sender<MetricsEvent>,
        // The server's ticket hook (see the field): `None` = local auth.
        auth: Option<crate::auth::TicketAuth>,
    ) -> Self {
        Self {
            conn,
            peer,
            state: ConnState::WaitingAuth,
            table,
            registry,
            inbox,
            out,
            actions: None,
            auth,
            ticket: None,
            identity: String::new(),
            m_in_bytes: 0,
            m_in_frames: 0,
            m_out_bytes: 0,
            m_out_frames: 0,
            m_actions_dropped: 0,
            m_actions_dropped_warned: false,
            m_actions_dropped_closed: 0,
            m_requests_dropped_closed: 0,
            m_requests_dropped_full: 0,
            m_requests_no_room: 0,
            m_input_limited: 0,
            m_input_limited_warned: false,
            input: Default::default(),
            v_score: 0,
            v_events: 0,
            v_answered: 0,
            v_closing: false,
            m_violations: 0,
            auth_attempts: VecDeque::new(),
            last_hb_ack: None,
            m_preauth_hb_extra: 0,
            m_hb_extra: 0,
            m_flushed_preauth_hb_extra: 0,
            m_flushed_hb_extra: 0,
            preauth_frames: 0,
            p_closing: false,
            w_closing: false,
            server_close: None,
            m_flushed_in_bytes: 0,
            m_flushed_in_frames: 0,
            m_flushed_out_bytes: 0,
            m_flushed_out_frames: 0,
            m_metrics_dropped: 0,
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
                    self.m_in_bytes = self
                        .m_in_bytes
                        .saturating_add(2 + frame.payload.len() as u64);
                    self.m_in_frames += 1;
                    self.maybe_flush_metrics(false);
                    // Pre-auth total frame budget (§3.3): counted BEFORE
                    // dispatch, so the crossing frame is not processed at
                    // all — an unauthenticated peer that keeps producing
                    // frames past the budget gets the close notice instead
                    // of one more round of server work. Every check is
                    // gated on WaitingAuth, so auth success naturally ends
                    // the counting (nothing to reset).
                    if self.state == ConnState::WaitingAuth {
                        self.preauth_frames += 1;
                        if self.preauth_frames > PREAUTH_FRAME_BUDGET {
                            self.close_preauth_budget().await;
                            break;
                        }
                    }
                    self.handle_frame(frame).await;
                    // The violation budget may have been exhausted while
                    // handling the frame (the `ERROR` code 9 close notice
                    // was already sent by `reply_err`); tear down now,
                    // exactly like a `ServerClosed`. Same teardown for the
                    // §3.3 pre-auth budget (its own code-9 notice was sent
                    // by `close_preauth_budget`).
                    // (The two budgets recorded their verdict when they
                    // fired; a dead outbound path is attributed here.)
                    if self.v_closing || self.p_closing || self.w_closing {
                        if self.w_closing {
                            self.adopt_pending_close();
                        }
                        break;
                    }
                }
                ConnIn::Closed { reason } => {
                    // Client-side end: no server verdict is recorded.
                    debug!(%self.conn, %reason, "connection closed by peer/io");
                    break;
                }
                ConnIn::StreamRejected { reason } => {
                    // The transport refused the byte stream: the server's
                    // verdict (see `ServerClose::StreamRejected`), told
                    // to the client as a best-effort ERROR 9.
                    self.on_stream_rejected(&reason);
                    break;
                }
                ConnIn::ServerClosed {
                    cause: cause @ (ServerClose::IdleInput | ServerClose::Kicked),
                    reason,
                } => {
                    // A room closed the session — its input-idle ceiling
                    // (E6) or the game's kick (E8): a verdict, told best
                    // effort — never waiting.
                    self.on_room_close(cause, &reason);
                    break;
                }
                ConnIn::ServerClosed { cause, reason } => {
                    // The server made this decision (idle timeout, write
                    // stall, connection capacity, …). Unlike a peer EOF the
                    // client may still be listening: tell it why, then
                    // clean up. The verdict is recorded BEFORE the notice,
                    // so a notice that cannot be sent does not lose it.
                    self.server_closing(cause);
                    warn!(%self.conn, ?cause, %reason, "server closing connection");
                    let _ = self
                        .send_frame(
                            op::base::ERROR,
                            &base::Error::new(base::ErrorCode::ServerClosed, reason),
                        )
                        .await;
                    break;
                }
                ConnIn::RoomGone(room) => {
                    self.server_closing(ServerClose::RoomGone);
                    warn!(%self.conn, room = %room, "room destroyed; detaching");
                    self.detach();
                    let _ = self
                        .send_frame(
                            op::base::ERROR,
                            &base::Error::new(base::ErrorCode::RoomDestroyed, "room destroyed"),
                        )
                        .await;
                    break;
                }
                ConnIn::LeftRoom { room } => self.on_left_room(room),
                ConnIn::Shutdown => {
                    // The server is stopping: a best-effort ERROR 14
                    // that never waits on the client, then the end.
                    self.on_shutdown();
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
    pub(super) fn maybe_flush_metrics(&mut self, last: bool) {
        let in_b = self.m_in_bytes - self.m_flushed_in_bytes;
        let in_f = self.m_in_frames - self.m_flushed_in_frames;
        let out_b = self.m_out_bytes - self.m_flushed_out_bytes;
        let out_f = self.m_out_frames - self.m_flushed_out_frames;
        let adrops = self.m_actions_dropped;
        let aclosed = self.m_actions_dropped_closed;
        let rclosed = self.m_requests_dropped_closed;
        let rfull = self.m_requests_dropped_full;
        let rnoroom = self.m_requests_no_room;
        let hb_pre = self.m_preauth_hb_extra - self.m_flushed_preauth_hb_extra;
        let hb_authed = self.m_hb_extra - self.m_flushed_hb_extra;
        let drops = self.m_metrics_dropped;
        let viols = self.m_violations;
        let limited = self.m_input_limited;
        // The server-close verdict rides the FINAL sample only (one per
        // session), and forces it out even when every delta is zero — a
        // connection refused at birth has sent and received nothing, and
        // its refusal must still be counted.
        let server_close = if last { self.server_close } else { None };
        if in_b == 0
            && in_f == 0
            && out_b == 0
            && out_f == 0
            && adrops == 0
            && aclosed == 0
            && rclosed == 0
            && rfull == 0
            && rnoroom == 0
            && hb_pre == 0
            && hb_authed == 0
            && drops == 0
            && viols == 0
            && limited == 0
            && server_close.is_none()
        {
            return;
        }
        if !last && Instant::now().duration_since(self.m_last_flush) < METRICS_FLUSH_EVERY {
            return;
        }
        self.m_flushed_in_bytes = self.m_in_bytes;
        self.m_flushed_in_frames = self.m_in_frames;
        self.m_flushed_out_bytes = self.m_out_bytes;
        self.m_flushed_out_frames = self.m_out_frames;
        self.m_actions_dropped = 0;
        self.m_actions_dropped_closed = 0;
        self.m_requests_dropped_closed = 0;
        self.m_requests_dropped_full = 0;
        self.m_requests_no_room = 0;
        self.m_flushed_preauth_hb_extra = self.m_preauth_hb_extra;
        self.m_flushed_hb_extra = self.m_hb_extra;
        self.m_metrics_dropped = 0;
        self.m_violations = 0;
        self.m_input_limited = 0;
        self.m_last_flush = Instant::now();
        // A3: bounded channel + synchronous `try_send`. On a full channel the
        // sample is dropped (harmless — the counters are cumulative deltas and
        // the next flush or the final flush carries the rest) and counted in
        // the *next* sample.
        if let Err(mpsc::error::TrySendError::Full(_)) =
            self.metrics.try_send(MetricsEvent::Conn(ConnSample {
                conn: self.conn,
                bytes_in: in_b,
                bytes_out: out_b,
                frames_in: in_f,
                frames_out: out_f,
                actions_dropped: adrops,
                actions_dropped_closed: aclosed,
                requests_dropped_closed: rclosed,
                requests_dropped_full: rfull,
                requests_no_room: rnoroom,
                heartbeats_throttled_preauth: hb_pre,
                heartbeats_throttled_authed: hb_authed,
                metrics_dropped: drops,
                violations: viols,
                input_rate_limited: limited,
                server_close,
                last,
            }))
        {
            self.m_metrics_dropped += 1;
        }
    }

    pub(super) fn detach(&mut self) {
        self.state = ConnState::Authed;
        self.actions = None;
    }
}
