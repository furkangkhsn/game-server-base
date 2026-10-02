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
            udp_sessions_dropped_accept_gone: self.accept_gone,
            udp_acks_send_failed: self.acks_send_failed,
            udp_challenges_send_failed: self.challenges_send_failed,
            udp_requests_dropped_closed: self.closed_requests,
            udp_actions_dropped_closed: self.closed_actions,
            udp_control_frames_dropped_closed: self.closed_controls,
            udp_datagrams_no_session: self.no_session,
            udp_game_reports_not_forwarded: self.reports_not_forwarded,
            udp_cids_assigned: self.mig.cids_assigned,
            udp_entropy_draws_failed: self.mig.entropy_failed,
            udp_cid_unknown: self.mig.cid_unknown,
            udp_path_validations_started: self.mig.validations_started,
            udp_path_challenges_sent: self.mig.challenges_sent,
            udp_path_challenges_send_failed: self.mig.challenges_send_failed,
            udp_path_amplification_capped: self.mig.amplification_capped,
            udp_path_address_in_use: self.mig.address_in_use,
            udp_path_responses_unmatched: self.mig.responses_unmatched,
            udp_path_changes_not_forwarded: self.mig.changes_not_forwarded,
            udp_path_validations_timed_out: self.mig.validations_timed_out,
            udp_path_validations_superseded: self.mig.validations_superseded,
            udp_path_validations_open_at_end: self.mig.validations_open_at_end,
            udp_migrations: self.mig.migrations,
            udp_migrations_port_only: self.mig.migrations_port_only,
            udp_proofs_refused_per_source: self.per_source.refused,
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
