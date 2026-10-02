//! The bounded waits at the end of a client's run (BACKLOG F51, B88):
//! for the answers the protocol still owes it once the run's deadline
//! has passed — its JOIN's (and its first snapshot), its LEAVE's.
//!
//! The bound counts the client's own WAITING, not the wall clock: the
//! wait is taken in slices, and a slice is charged at most its own
//! length. A client descheduled through its window (a starved or frozen
//! process) used to wake past the deadline and stop without reading the
//! answer already in its socket — `left=0` on a run whose server
//! counted every leave (`leaves=12` against `left=3` under F35's
//! starvation; `left=0` with a frozen process).

use std::time::{Duration, Instant};

/// How long a client waits for an answer the protocol owes it, in its
/// own waiting time. The rUDP reliable band's liveness bound
/// (`REL_NO_ACK_FATAL`, 5 s — at least four re-sends at its 1 s
/// `MAX_RTO` ceiling; DESIGN §6): the transport is still delivering a
/// control frame — the LEAVE — until then, so a shorter wait declares
/// "not left" for a leave that is still on its way (the old 500 ms was
/// half of one `MAX_RTO`). On a stream the same bound covers a starved
/// server. The normal path ends at the answer, long before it.
pub(crate) const PROTOCOL_WAIT: Duration = Duration::from_secs(5);

/// One slice of a wait: the most a stall inside one receive can cost
/// the bound.
const SLICE: Duration = Duration::from_millis(100);

/// A wait's remaining budget.
pub(crate) struct Wait {
    left: Duration,
}

impl Wait {
    /// A fresh wait of `total`.
    pub(crate) fn new(total: Duration) -> Self {
        Self { left: total }
    }

    /// The next receive's timeout; `None` once the budget is spent.
    pub(crate) fn slice(&self) -> Option<Duration> {
        (!self.left.is_zero()).then(|| self.left.min(SLICE))
    }

    /// A receive with timeout `slice` that started at `at` returned:
    /// charge what it waited — never more than the slice (the overrun
    /// is time the client was not running).
    pub(crate) fn charge(&mut self, slice: Duration, at: Instant) {
        self.left = self.left.saturating_sub(at.elapsed().min(slice));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stalled_slice_costs_only_its_length() {
        let mut w = Wait::new(Duration::from_millis(250));
        let slice = w.slice().expect("a budget");
        assert_eq!(slice, SLICE);
        // A receive that "took" ten seconds (the process was frozen).
        w.charge(slice, Instant::now() - Duration::from_secs(10));
        assert_eq!(w.slice(), Some(SLICE), "150 ms left");
        w.charge(SLICE, Instant::now() - SLICE);
        assert_eq!(w.slice(), Some(Duration::from_millis(50)));
        w.charge(Duration::from_millis(50), Instant::now() - SLICE);
        assert_eq!(w.slice(), None, "spent");
    }
}
