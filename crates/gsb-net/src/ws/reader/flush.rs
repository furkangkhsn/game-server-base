//! The reader's control replies and their losses (BACKLOG B58): a close
//! frame or a pong the full control queue refused is counted, and the
//! counts reach the collector (see `crate::metrics`). A child of
//! [`super`], so the counters stay private to the reader.

use gsb_core::metrics::TransportCounters;
use tokio::sync::mpsc::error::TrySendError;

use crate::ws::*;

impl super::WsReader {
    /// Queue one control reply for the socket writer — best effort
    /// (poll context means `try_send`). A FULL queue drops it, counted by
    /// kind; a CLOSED one means the socket writer is gone with the
    /// connection, and nothing could carry any frame.
    pub(super) fn queue_control(&mut self, op: u8, payload: Vec<u8>) {
        if let Err(TrySendError::Full(_)) = self.ctrl.try_send(WsOut::Control(op, payload)) {
            if op == OP_CLOSE {
                self.close_frames_dropped += 1;
            } else {
                self.pongs_dropped += 1;
            }
            self.flush_metrics(false);
        }
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
