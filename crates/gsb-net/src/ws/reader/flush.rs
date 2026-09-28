//! The reader's control replies and their losses (BACKLOG B58, B83): a
//! close frame or a pong the control queue refused — full, or closed
//! behind a stopped socket writer — is counted, and the counts reach the
//! collector (see `crate::metrics`). A child of [`super`], so the
//! counters stay private to the reader.

use gsb_core::metrics::TransportCounters;
use tokio::sync::mpsc::error::TrySendError;

use crate::ws::*;

impl super::WsReader {
    /// Queue one control reply for the socket writer — best effort
    /// (poll context means `try_send`). A refused reply is counted by
    /// kind and by cause: a FULL queue (B58), or a CLOSED one (B83) — the
    /// socket writer had already stopped on a failed socket write, and
    /// the reply could reach no wire.
    pub(super) fn queue_control(&mut self, op: u8, payload: Vec<u8>) {
        let closed = match self.ctrl.try_send(WsOut::Control(op, payload)) {
            Ok(()) => return,
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Closed(_)) => true,
        };
        let count = match (op == OP_CLOSE, closed) {
            (true, false) => &mut self.close_frames_dropped,
            (false, false) => &mut self.pongs_dropped,
            (true, true) => &mut self.close_frames_dropped_closed,
            (false, true) => &mut self.pongs_dropped_closed,
        };
        *count += 1;
        self.flush_metrics(false);
    }

    /// Send the counts to the collector: `last` as the reader goes (see
    /// `Drop`), otherwise when a flush is due.
    fn flush_metrics(&mut self, last: bool) {
        if !last && !self.flusher.due() {
            return;
        }
        let totals = TransportCounters {
            ws_close_frames_dropped: self.close_frames_dropped,
            ws_pongs_dropped: self.pongs_dropped,
            ws_close_frames_dropped_closed: self.close_frames_dropped_closed,
            ws_pongs_dropped_closed: self.pongs_dropped_closed,
            ..Default::default()
        };
        self.flusher.flush(totals, last);
    }
}

/// The reader's last word to the collector, as its pump drops it.
impl Drop for super::WsReader {
    fn drop(&mut self) {
        self.flush_metrics(true);
    }
}
