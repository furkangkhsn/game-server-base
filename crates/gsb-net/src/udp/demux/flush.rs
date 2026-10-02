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
            udp_pending_source_moves_kept: self.per_source.moves_kept,
            ..self.seal_totals()
        };
        self.flusher.flush(totals, last);
    }

    /// A sealed door's counters (B5a, module `record`; zero on a
    /// plaintext door).
    pub(super) fn seal_totals(&self) -> TransportCounters {
        let Some(seal) = &self.seal else {
            return TransportCounters::default();
        };
        let (c, r) = (&seal.counts, &seal.counts.refused);
        TransportCounters {
            udp_proofs_refused_budget: seal.budget.refused,
            udp_proofs_refused_plaintext: c.proofs_refused_plaintext,
            udp_handshakes_malformed: c.handshakes_malformed,
            udp_handshakes_failed_decrypt: c.handshakes_failed_decrypt,
            udp_handshakes_failed_internal: c.handshakes_failed_internal,
            udp_datagrams_unsealed: c.datagrams_unsealed,
            seal_integrity_limit: r[0],
            seal_malformed: r[1],
            seal_too_old: r[2],
            seal_replayed: r[3],
            seal_wrong_phase: r[4],
            seal_forged: r[5],
            udp_sessions_ended_seal_limit: c.sessions_ended_limit,
            udp_path_candidates_not_newest: c.candidates_not_newest,
            udp_acks_not_queued: c.acks_not_queued,
            udp_path_challenges_not_queued: c.challenges_not_queued,
            udp_stateless_resets_sent: c.resets_sent,
            udp_stateless_resets_rate_limited: seal.resets.as_ref().map_or(0, |b| b.refused),
            udp_stateless_resets_send_failed: c.resets_send_failed,
            ..Default::default()
        }
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
