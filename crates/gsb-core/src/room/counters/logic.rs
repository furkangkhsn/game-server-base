//! The logic's own counters at the sample: the one thing the actor
//! checks about them (the bound), shared by the room and shard actors.

use super::RoomCounters;
use crate::id::RoomId;
use crate::metrics::{LOGIC_COUNTERS_MAX, LogicCounters};
use tracing::warn;

impl RoomCounters {
    /// The logic put more distinct names than a sample holds
    /// ([`LOGIC_COUNTERS_MAX`]): the extra values are dropped (and
    /// counted in the set). The names are static per game, so this is a
    /// declaration mistake, said once per actor rather than every
    /// sample.
    pub(crate) fn note_logic(&mut self, room: RoomId, logic: &LogicCounters) {
        if logic.dropped() > 0 && !self.logic_dropped_warned {
            self.logic_dropped_warned = true;
            warn!(
                %room,
                max = LOGIC_COUNTERS_MAX,
                dropped = logic.dropped(),
                "the logic reports more counters than a sample holds; the extra ones are dropped"
            );
        }
    }
}
