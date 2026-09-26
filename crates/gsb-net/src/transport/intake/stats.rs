//! A door's handshake counters: the snapshot `Listener::handshake_stats`
//! returns, and the summary the intake task logs when it stops (the
//! rUDP demux's stop summary, for the handshake doors — the server has
//! no metrics path for listeners).

use std::sync::atomic::Ordering;

use tracing::info;

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
