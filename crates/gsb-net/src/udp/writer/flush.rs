//! The writer's loss counters on their way to the collector (BACKLOG
//! B58; see `crate::metrics`). A CHILD of [`super`], so the writer's
//! state stays private.

use gsb_core::metrics::TransportCounters;

impl super::UdpWriter {
    /// Send the loss counters to the collector (B58): `last` as the
    /// writer ends, otherwise when a flush is due. A session that lost
    /// nothing sends nothing.
    pub(super) fn flush_metrics(&mut self, last: bool) {
        if !last && !self.flusher.due() {
            return;
        }
        let totals = TransportCounters {
            udp_frames_dropped_oversized: self.dropped_oversized,
            udp_control_frames_abandoned: self.abandoned,
            udp_frames_drained: self.drained,
            ..Default::default()
        };
        self.flusher.flush(totals, last);
    }
}
