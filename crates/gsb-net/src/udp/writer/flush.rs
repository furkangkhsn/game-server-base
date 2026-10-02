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
            ..self.feedback_totals()
        };
        self.flusher.flush(totals, last);
    }

    /// The game band's feedback counters (module `crate::udp::feedback`).
    fn feedback_totals(&self) -> TransportCounters {
        let c = &self.feedback.counts;
        TransportCounters {
            udp_game_announces_received: c.announces,
            udp_game_probes_sent: c.probes_sent,
            udp_game_probes_send_failed: c.probes_send_failed,
            udp_game_probes_unanswered: c.probes_unanswered,
            udp_game_reports_received: c.reports,
            udp_game_reports_late: c.late,
            udp_game_reports_invalid: c.invalid,
            udp_game_reports_clamped: c.clamped,
            udp_game_datagrams_reported_sent: c.reported_sent,
            udp_game_datagrams_reported_lost: c.reported_lost,
            udp_game_rtt_samples: c.rtt_samples,
            udp_game_rtt_sum_us: c.rtt_sum_us,
            ..Default::default()
        }
    }
}
