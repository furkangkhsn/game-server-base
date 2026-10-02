//! The combat feed (BACKLOG B81): every landed [`Hit`] is published with
//! `try_send` — an observability feed, never awaited on the tick path —
//! and every hit it cannot take is COUNTED, by why, on the logic-counter
//! seam (F9): [`HITS_DROPPED_FULL`] (the feed was full: its reader fell
//! behind), [`HITS_DROPPED_CLOSED`] (its reader is gone) — the core's
//! full/closed split. The bound is the caller's: the capacity of the
//! mailbox it hands `set_combat_feed`. A reader that keeps up with the
//! room's hits drops nothing; the count is how a too-small bound shows.
//!
//! A count is put once it is non-zero (the core's
//! `logic_counters_dropped` rule, F17): a room whose feed never dropped
//! a hit reports exactly what it did before, and one with no feed has
//! nothing to drop.

use gsb_core::channel::Mailbox;
use gsb_core::metrics::{LogicCounter, LogicCounters};
use tokio::sync::mpsc::error::TrySendError;

use super::Hit;

/// Landed hits the feed dropped because it was full (F9 counter).
pub const HITS_DROPPED_FULL: LogicCounter = LogicCounter::sum(
    "combat_hits_dropped_full",
    "Landed hits the combat feed dropped because it was full, cumulative.",
);

/// Landed hits the feed dropped because its reader was gone (F9
/// counter).
pub const HITS_DROPPED_CLOSED: LogicCounter = LogicCounter::sum(
    "combat_hits_dropped_closed",
    "Landed hits the combat feed dropped because its reader was gone, cumulative.",
);

/// The optional feed and its loss counts.
#[derive(Default)]
pub(crate) struct Feed {
    tx: Option<Mailbox<Hit>>,
    dropped_full: u64,
    dropped_closed: u64,
}

impl Feed {
    /// Publish on `tx` from now on.
    pub(crate) fn attach(&mut self, tx: Mailbox<Hit>) {
        self.tx = Some(tx);
    }

    /// Publish `hit`; a hit the feed cannot take is counted, not kept.
    pub(super) fn publish(&mut self, hit: Hit) {
        let Some(tx) = &self.tx else { return };
        match tx.try_send(hit) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => self.dropped_full += 1,
            Err(TrySendError::Closed(_)) => self.dropped_closed += 1,
        }
    }

    /// Put the loss counts that are non-zero (module docs).
    pub(crate) fn counters(&self, out: &mut LogicCounters) {
        let counts = [
            (&HITS_DROPPED_FULL, self.dropped_full),
            (&HITS_DROPPED_CLOSED, self.dropped_closed),
        ];
        for (counter, n) in counts {
            if n > 0 {
                out.put(counter, n);
            }
        }
    }
}
