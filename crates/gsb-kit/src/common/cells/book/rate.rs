//! The cell book's send-rate half (KIT-ARCHITECTURE §10 "A10"): a
//! record's change INSIDE its cell waits for the record's due step
//! ([`SendEvery::due`]); everything else the book records — a record
//! appearing, crossing, exiting — goes out at once, and every full piece
//! reads the buckets, which always hold the current value.
//!
//! **Why the change lists stay sound.** A deferral is per RECORD, never
//! per group: every established group that shows the cell holds the
//! same last-sent value of a pending record (a group that got a full
//! meanwhile holds the current one — the due upsert re-sends it, an
//! idempotent surplus), so the cell's delta piece stays one encoding
//! shared by all of them.

use std::fmt::Debug;
use std::hash::Hash;

use crate::codec::SendEvery;

use super::CellBook;

impl<W: Clone + Eq, C: Copy + Eq + Hash + Debug> CellBook<W, C> {
    /// Primitive: a record's wire content changed WITHIN `cell` (already
    /// known different). Due this step (always, in the default class):
    /// an upsert in the cell's change list, as [`Self::record_update`].
    /// Not due: the bucket takes the value (every full shows it) and the
    /// record waits in [`CellBook::deferred`] with its current class.
    pub(crate) fn record_change(&mut self, cell: C, wire: u64, value: W, every: SendEvery) {
        if every.due(self.step, wire) {
            self.forget_deferred(wire);
            self.record_update(cell, wire, value);
            return;
        }
        if let Some(b) = self.buckets.get_mut(&cell) {
            b.insert(wire, value);
        }
        self.deferred.insert(wire, (cell, every));
    }

    /// The record left its cell or re-entered the view (the caller
    /// sends its current value, or its exit): nothing is pending.
    #[inline]
    pub(crate) fn forget_deferred(&mut self, wire: u64) {
        if !self.deferred.is_empty() {
            self.deferred.remove(&wire);
        }
    }

    /// Release every pending record due on this step into its cell's
    /// change list with its CURRENT value (the bucket's) — the roll's
    /// first act, after every content source of the tick landed.
    pub(crate) fn release_due(&mut self) {
        if self.deferred.is_empty() {
            return;
        }
        let step = self.step;
        let mut released = std::mem::take(&mut self.released);
        self.deferred.retain(|&wire, &mut (cell, every)| {
            let due = every.due(step, wire);
            if due {
                released.push((wire, cell));
            }
            !due
        });
        for (wire, cell) in released.drain(..) {
            let value = self.buckets.get(&cell).and_then(|b| b.get(&wire)).cloned();
            if let Some(value) = value {
                self.record_update(cell, wire, value);
            }
        }
        self.released = released;
    }
}
