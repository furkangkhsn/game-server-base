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
        }
    }

    /// Send the door's refusals, timeouts and failures to the collector
    /// (B58): from the intake task, when a flush is due after an accept,
    /// and once more (`last`) when the door closes. A timeout or failure
    /// after the last accept is reported at the next accept or at the
    /// close.
    pub(crate) fn flush_metrics(&self, flusher: &mut Flusher, last: bool) {
        if !last && !flusher.due() {
            return;
        }
        let s = self.stats();
        let totals = TransportCounters {
            handshakes_refused: s.refused,
            handshakes_timed_out: s.timed_out,
            handshakes_failed: s.failed,
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
            "handshake intake stopped"
        );
    }
}
