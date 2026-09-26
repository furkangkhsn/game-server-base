//! Crystallization's counters (BACKLOG F9, `docs/CROSS-SHARD.md` §4c
//! item 5): the operator's view of the fight table and the holds,
//! reported through the core's logic-counter seam
//! (`GameLogic::logic_counters`) by every sharded room that opted in —
//! a room without crystallization reports none of them. The per-wire
//! detail stays in the `gsb_kit::crystal` debug lines.

use gsb_core::metrics::{LogicCounter, LogicCounters};

use crate::sharded::crystal::{Crystal, Release};

const MOVES: LogicCounter = LogicCounter::sum(
    "crystal_moves",
    "Entities a ripe cross-seam fight pinned to their partner's shard (each one migration), cumulative.",
);
const RELEASE_QUIET: LogicCounter = LogicCounter::sum(
    "crystal_release_quiet",
    "Holds ended because the fight went quiet, cumulative.",
);
const RELEASE_BAND: LogicCounter = LogicCounter::sum(
    "crystal_release_band",
    "Holds ended because the held entity left the band, cumulative.",
);
const RELEASE_PARTNER: LogicCounter = LogicCounter::sum(
    "crystal_release_partner",
    "Holds ended because the partner is no longer on the shard, cumulative.",
);
const UNTRACKED: LogicCounter = LogicCounter::sum(
    "crystal_untracked",
    "Cross-seam contacts the fight table's cap refused (that pair does not crystallize), cumulative.",
);
const FIGHTS_PEAK: LogicCounter = LogicCounter::max(
    "crystal_fights_peak",
    "Most pairs the fight table has tracked at once (its cap is 1024).",
);

/// What the pass has done, cumulative: movers pinned, holds ended by
/// cause.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(in crate::sharded) struct CrystalStats {
    pub(in crate::sharded) moves: u64,
    pub(in crate::sharded) release_quiet: u64,
    pub(in crate::sharded) release_band: u64,
    pub(in crate::sharded) release_partner: u64,
}

impl CrystalStats {
    /// A hold ended for `why`.
    pub(in crate::sharded) fn release(&mut self, why: Release) {
        match why {
            Release::Quiet => self.release_quiet += 1,
            Release::Band => self.release_band += 1,
            Release::Partner => self.release_partner += 1,
        }
    }
}

impl Crystal {
    /// Put the six counters (every one, zeros included).
    pub(in crate::sharded) fn counters(&self, out: &mut LogicCounters) {
        let s = &self.stats;
        out.put(&MOVES, s.moves);
        out.put(&RELEASE_QUIET, s.release_quiet);
        out.put(&RELEASE_BAND, s.release_band);
        out.put(&RELEASE_PARTNER, s.release_partner);
        out.put(&UNTRACKED, self.book.untracked);
        out.put(&FIGHTS_PEAK, self.book.peak as u64);
    }
}
