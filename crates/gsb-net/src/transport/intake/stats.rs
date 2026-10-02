//! A door's handshake counters: the snapshot `Listener::handshake_stats`
//! returns, the summary the intake task logs when it stops (the rUDP
//! demux's stop summary, for the handshake doors), and — since B58 —
//! the refusals, timeouts and failures the intake task sends to the
//! collector (`crate::metrics`).

use std::sync::atomic::Ordering;

use gsb_core::metrics::TransportCounters;
use tracing::info;

use crate::metrics::Flusher;
use crate::transport::intake::Intake;

/// A door's handshake counters (`Listener::handshake_stats`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HandshakeStats {
    /// Connections holding a slot now: handshaking, or finished and not
    /// yet taken by the accept loop.
    pub in_flight: u64,
    /// Handshakes that finished and were queued for the accept loop.
    pub completed: u64,
    /// Connections refused (closed unhandshaken) at the bound.
    pub refused: u64,
    /// Handshakes cut at their deadline.
    pub timed_out: u64,
    /// Handshakes that failed (the client's fault: bad request, bad TLS).
    pub failed: u64,
    /// Handshakes in flight when the door closed, cut (B74).
    pub cut_closed: u64,
    /// Finished handshakes still queued for the accept loop when the door
    /// closed, dropped (B74; also in `completed`).
    pub unaccepted_closed: u64,
    /// Connections refused (closed unhandshaken; QUIC: `refuse`) because
    /// their source held the per-source cap (D11).
    pub refused_per_source: u64,
    /// QUIC connections from an unproven source at the per-source cap,
    /// asked to prove their address (a stateless Retry, no slot) instead
    /// of being refused (D11).
    pub retried_per_source: u64,
}

impl Intake {
    /// The counters now.
    pub(crate) fn stats(&self) -> HandshakeStats {
        HandshakeStats {
            in_flight: self.held.load(Ordering::Acquire) as u64,
            completed: self.completed.load(Ordering::Relaxed),
            refused: self.refused.load(Ordering::Relaxed),
            timed_out: self.timed_out.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            cut_closed: self.cut.load(Ordering::Relaxed),
            unaccepted_closed: self.unaccepted.load(Ordering::Relaxed),
            refused_per_source: self.refused_per_source.load(Ordering::Relaxed),
            retried_per_source: self.retried_per_source.load(Ordering::Relaxed),
        }
    }

    /// Send the door's refusals, timeouts and failures to the collector
    /// (B58), and what its close cut or dropped (B74): from the intake
    /// task, when a flush is due after an accept, and once more (`last`)
    /// when the door closes — after the close has settled (see
    /// `Intake::settle`, in `close`). A timeout or failure after the last accept is
    /// reported at the next accept or at the close.
    pub(crate) fn flush_metrics(&self, flusher: &mut Flusher, last: bool) {
        if !last && !flusher.due() {
            return;
        }
        let s = self.stats();
        let totals = TransportCounters {
            handshakes_refused: s.refused,
            handshakes_timed_out: s.timed_out,
            handshakes_failed: s.failed,
            handshakes_cut_closed: s.cut_closed,
            handshakes_unaccepted_closed: s.unaccepted_closed,
            handshakes_refused_per_source: s.refused_per_source,
            handshakes_retried_per_source: s.retried_per_source,
            ..Default::default()
        };
        flusher.flush(totals, last);
    }

    /// The intake task's last word (the rUDP demux's stop summary, for
    /// the handshake doors).
    pub(crate) fn log_summary(&self) {
        let s = self.stats();
        info!(
            door = self.kind,
            completed = s.completed,
            refused = s.refused,
            timed_out = s.timed_out,
            failed = s.failed,
            cut_closed = s.cut_closed,
            unaccepted_closed = s.unaccepted_closed,
            refused_per_source = s.refused_per_source,
            retried_per_source = s.retried_per_source,
            "handshake intake stopped"
        );
    }
}
