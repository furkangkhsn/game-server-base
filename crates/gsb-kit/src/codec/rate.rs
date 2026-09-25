//! A record's send-rate class ([`SendEvery`], KIT-ARCHITECTURE §4.1,
//! §10 "A10") and the kit's due schedule: which room steps a CHANGED
//! record of a class may go out on.
//!
//! The schedule is a pure function of `(step, wire id, class)`: no state
//! travels with a record, so every shard that shows a record — its
//! owner, a neighbour borrowing it, one importing its body — and every
//! shard it migrates to computes the same due steps.

/// How often a record's CHANGES need to go out (KIT-ARCHITECTURE §4.1,
/// §10 "A10"): on every room step (the default — every game's bytes
/// before A10), or on at most every 2nd, 4th, 8th or 16th step.
///
/// The game picks the class per record from its wire value
/// ([`RecordCodec::send_every`](crate::codec::RecordCodec::send_every));
/// the kit's delta engines hold a changed record that is not DUE yet and
/// send its CURRENT value on its next due step. Entering a view, leaving
/// it (`removed`, `cell_exits`) and every full frame ignore the class.
///
/// **Why powers of two.** The due steps of the classes NEST (a step due
/// for every 8th is due for every 4th, 2nd and 1st — one phase per
/// record), so a record that changes class while a change is pending
/// still goes out within the largest period it had: the bound holds
/// without the kit remembering anything about the record's past.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(u8)]
pub enum SendEvery {
    /// Every step (30 Hz on a 30 Hz room) — the default.
    #[default]
    Tick = 0,
    /// At most every 2nd step (15 Hz on a 30 Hz room).
    Ticks2 = 1,
    /// At most every 4th step.
    Ticks4 = 2,
    /// At most every 8th step.
    Ticks8 = 3,
    /// At most every 16th step.
    Ticks16 = 4,
}

impl SendEvery {
    /// The class's period in room steps: 1, 2, 4, 8 or 16. A change of a
    /// record in this class goes out at most `ticks() − 1` steps after
    /// the step it was made on.
    #[inline]
    #[must_use]
    pub const fn ticks(self) -> u64 {
        1 << (self as u8)
    }

    /// Whether a record with wire id `wire` in this class is due on room
    /// step `step`: always for [`SendEvery::Tick`], else on one step in
    /// every [`Self::ticks`], at the record's own phase.
    ///
    /// **Phase spreading.** The phase is a fixed mix of the wire id (the
    /// top bits of a Fibonacci hash), so a class's records are due
    /// evenly across its period rather than all on one step — also for
    /// the arithmetic progressions a sharded room mints (A30: shard `i`
    /// of `N` draws `i + 1`, `N + i + 1`, …, all congruent mod `N`, so
    /// the id itself mod the period would put a shard's records on one
    /// step).
    #[inline]
    #[must_use]
    pub const fn due(self, step: u64, wire: u64) -> bool {
        let mask = self.ticks() - 1;
        mask == 0 || step.wrapping_add(phase(wire)) & mask == 0
    }
}

/// A record's phase: the top four bits of its wire id's Fibonacci hash.
#[inline]
const fn phase(wire: u64) -> u64 {
    wire.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 60
}

#[cfg(test)]
mod tests;
