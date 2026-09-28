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
            udp_game_datagrams_send_failed: self.game_send_failed,
            udp_control_datagrams_send_failed: self.control_send_failed,
            udp_frames_unsent: self.unsent,
            writer_verdicts_deferred: self.verdicts_deferred,
            udp_control_retransmits_timeout: self.retransmits,
            ..Default::default()
        };
        self.flusher.flush(totals, last);
    }
}
