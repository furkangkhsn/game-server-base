//! Recording WHY the session ended: the server-close verdict the actor
//! reports once, on its final metrics flush (see `ServerClose`).

use tracing::debug;

use crate::conn::*;

impl super::ConnectionActor {
    /// Record a server verdict. The FIRST one wins: a close notice that
    /// then fails to send (`w_closing`) must not overwrite the verdict
    /// that sent it.
    pub(super) fn server_closing(&mut self, cause: ServerClose) {
        self.server_close.get_or_insert(cause);
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
                ConnIn::Closed { .. } | ConnIn::Shutdown => return,
                ConnIn::Frame(_) | ConnIn::RoomGone(_) => {}
            }
        }
        self.server_close = Some(ServerClose::OutboundDead);
    }
}
