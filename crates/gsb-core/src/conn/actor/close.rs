//! Recording WHY the session ended: the server-close verdict the actor
//! reports once, on its final metrics flush (see `ServerClose`) — and
//! the best-effort close notice of the ends that must never wait on the
//! client (see `try_notice`).

use tokio::sync::mpsc::error::TrySendError;
use tracing::debug;

use gsb_protocol::{base, op};

use crate::conn::*;

/// The `ERROR` code 14 message. The code carries the client's decision
/// (reconnect later or elsewhere); the text is for a human reading a log.
const SERVER_STOPPING_MESSAGE: &str = "server stopping: reconnect later or to another server";

impl super::ConnectionActor {
    /// Record a server verdict. The FIRST one wins: a close notice that
    /// then fails to send (`w_closing`) must not overwrite the verdict
    /// that sent it.
    pub(super) fn server_closing(&mut self, cause: ServerClose) {
        self.server_close.get_or_insert(cause);
    }

    /// The server is stopping (`ConnIn::Shutdown`): announce it with
    /// `ERROR` code 14 and end. Not a verdict on the session, so nothing
    /// is recorded (see `ServerClose`, "deliberately NOT a reason").
    pub(super) fn on_shutdown(&mut self) {
        debug!(%self.conn, "connection shutdown (server)");
        self.try_notice(base::ErrorCode::ServerStopping, SERVER_STOPPING_MESSAGE);
    }

    /// The transport refused the inbound byte stream
    /// (`ConnIn::StreamRejected`): record the verdict, then announce it
    /// with `ERROR` code 9 like every other server verdict — best effort,
    /// like the stop notice: the peer that sent bytes the transport
    /// refuses is exactly the one not to wait on. The reader half is
    /// untrustworthy past this point; the writer half usually is not (an
    /// oversized frame breaks nothing outbound). Where it is, the notice
    /// simply never lands: a door that has already said goodbye in its
    /// own vocabulary (the WebSocket close frame) drops it, a TLS stream
    /// broken by a corrupt record fails the write.
    pub(super) fn on_stream_rejected(&mut self, reason: &str) {
        self.server_closing(ServerClose::StreamRejected);
        debug!(%self.conn, %reason, "inbound stream rejected by the transport");
        self.try_notice(
            base::ErrorCode::ServerClosed,
            format!("stream rejected: {reason}"),
        );
    }

    /// A ROOM closed this session, relayed by the registry: its
    /// input-idle ceiling (`ServerClose::IdleInput`, BACKLOG E6 —
    /// `afk_action = disconnect`) or the game's kick
    /// (`ServerClose::Kicked`, E8 — `TickCtx::kick`). Record the verdict,
    /// then announce it with `ERROR` code 9 like every other server
    /// verdict — best effort, like the stop notice. The member this
    /// reaches is very likely one that stopped READING too (a
    /// backgrounded client, or the very reason the game kicks it): the
    /// awaited notice of the older code-9 closes would park this actor
    /// until the write-stall window closes the queue, forever with the
    /// window off, keeping open the very socket the room asked to close.
    /// Logged at `debug`: the room decided, and a room shedding members
    /// sheds many.
    pub(super) fn on_room_close(&mut self, cause: ServerClose, reason: &str) {
        self.server_closing(cause);
        debug!(%self.conn, ?cause, %reason, "the room closed this connection");
        self.try_notice(base::ErrorCode::ServerClosed, reason.to_owned());
    }

    /// Queue a close notice WITHOUT waiting: a synchronous `try_send`
    /// onto the outbound queue, then the caller ends the session.
    ///
    /// Why not the awaited `send_frame` the other closes use: these ends
    /// must never depend on the client. A client that stopped reading
    /// keeps its queue full, and an awaited send would park this actor
    /// until the writer pump's stall window closes the queue — forever
    /// with the window off — once per such client, on a server that is
    /// trying to stop. A full queue means the client is behind anyway:
    /// the notice is dropped and the client gets only the close. A
    /// CLOSED queue means the writer is gone: nothing can carry it.
    ///
    /// Queued, it is the last frame this actor sends; the writer pump
    /// drains what is queued and then closes the socket (the stream
    /// doors) — the notice goes out ahead of the close.
    fn try_notice(&mut self, code: base::ErrorCode, message: impl Into<String>) {
        let Some(fb) = self
            .table
            .frame(op::base::ERROR, &base::Error::new(code, message))
        else {
            return;
        };
        let bytes = 2 + fb.payload.len() as u64;
        match self.out.try_send(vec![fb]) {
            Ok(()) => {
                self.m_out_bytes = self.m_out_bytes.saturating_add(bytes);
                self.m_out_frames += 1;
            }
            // Both failures are counted (B57), apart: a full queue lost
            // the notice to a client that was not reading
            // (`close_notices_dropped`); a closed one had no writer left
            // to carry any frame (`frames_out_closed`, like `send_frame`).
            Err(TrySendError::Full(_)) => {
                self.m_close_notices_dropped += 1;
                debug!(%self.conn, ?code, "close notice dropped: the outbound queue is full");
            }
            Err(TrySendError::Closed(_)) => self.m_frames_out_closed += 1,
        }
    }

    /// Attribute a dead outbound path (`w_closing`).
    ///
    /// The outbound channel closes when the writer pump exits, and the
    /// pump exits for two reasons: a socket write FAILED (the peer is
    /// gone — a client-side end), or nothing was written for the whole
    /// write-stall window (a server verdict). Either way the actor learns
    /// it first as a failed send, typically while parked on the channel
    /// the stalled writer stopped draining — and the pump's own report
    /// (`ServerClosed` from the writer, `Closed` from the reader) is then
    /// already waiting in the mailbox behind it. Reading only the send
    /// error would book every write stall as `OutboundDead`.
    ///
    /// So look before leaving: a synchronous `try_recv` drain (no await
    /// is added — the run loop is exiting anyway, and nothing drained
    /// here would be acted on). A pending server verdict is adopted; a
    /// pending peer close means the peer left (not counted); a pending
    /// shutdown is not counted either (see `ServerClose`). Only when
    /// nothing explains it is the close booked as `OutboundDead`.
    pub(super) fn adopt_pending_close(&mut self) {
        if self.server_close.is_some() {
            return;
        }
        while let Ok(msg) = self.inbox.try_recv() {
            match msg {
                ConnIn::ServerClosed { cause, reason } => {
                    debug!(%self.conn, ?cause, %reason, "dead outbound path: adopting the pending verdict");
                    self.server_close = Some(cause);
                    return;
                }
                ConnIn::StreamRejected { .. } => {
                    self.server_close = Some(ServerClose::StreamRejected);
                    return;
                }
                ConnIn::Closed { .. } | ConnIn::Shutdown | ConnIn::ShutdownOvertaking(_) => return,
                // A frame looked through is never processed: counted
                // (B60), like the ones `abandon_inbox` finds after it.
                ConnIn::Frame(frame) => self.count_unprocessed(frame.op),
                ConnIn::RoomGone(_)
                | ConnIn::LeftRoom { .. }
                | ConnIn::Path(_)
                | ConnIn::PeerChanged { .. } => {}
            }
        }
        self.server_close = Some(ServerClose::OutboundDead);
    }
}
