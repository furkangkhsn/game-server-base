//! The demux's loss counters on their way to the collector (BACKLOG
//! B58; see `crate::metrics`). A child of [`super`], so the counters stay
//! private to the demux's module tree.

use gsb_core::metrics::TransportCounters;

impl super::Demux {
    /// Send the loss counters to the collector (B58): `last` as the
    /// demux ends (see `Drop`), otherwise when a flush is due.
    pub(super) fn flush_metrics(&mut self, last: bool) {
        if !last && !self.flusher.due() {
            return;
        }
        let totals = TransportCounters {
            udp_requests_dropped_full: self.full_requests,
            udp_actions_dropped_full: self.full_actions,
            udp_control_frames_dropped_full: self.full_controls,
            udp_acks_not_forwarded: self.ack_piggyback_failed,
            udp_datagrams_oversized: self.oversized_in,
            udp_datagrams_malformed: self.bad_datagrams,
            udp_bad_cookies: self.bad_cookie,
            udp_frags_refused: self.frag_refused,
            udp_sessions_dropped_accept_full: self.endpoints_dropped,
            ..Default::default()
        };
        self.flusher.flush(totals, last);
    }
}

/// The demux's last word to the collector (B58). On `Drop`, so it also
/// runs when the listener's `close` aborts the task (the loop's own exit
/// is only the socket read failing).
impl Drop for super::Demux {
    fn drop(&mut self) {
        self.flush_metrics(true);
    }
}
