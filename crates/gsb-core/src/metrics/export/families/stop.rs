//! The per-room families of what a stopping room/shard still held beyond
//! its sessions (B68, see `crate::metrics::StopCounts`): one cumulative
//! counter per field, reported from the final sample.

use super::{RoomFamily, counter};

/// The stop's leftovers, in the counters' order. The last five move on
/// shard rows only (0 on a single room).
pub(super) const STOP: [RoomFamily; 9] = [
    counter(
        "gsb_room_joins_unprocessed_total",
        "Join ops still queued when the room/shard stopped, never admitted (the connection is answered RoomGone), cumulative.",
        |r| r.stop.joins_unprocessed,
    ),
    counter(
        "gsb_room_resumes_unprocessed_total",
        "Resume attempts still queued when the room/shard stopped (on a shard: the one holding the parked identity), never looked up, cumulative.",
        |r| r.stop.resumes_unprocessed,
    ),
    counter(
        "gsb_room_leaves_unprocessed_total",
        "Leave ops for a member here still queued when the room/shard stopped, cumulative.",
        |r| r.stop.leaves_unprocessed,
    ),
    counter(
        "gsb_room_detaches_unprocessed_total",
        "Detach ops (a member's transport died) still queued when the room/shard stopped: the disconnect policy never ran, cumulative.",
        |r| r.stop.detaches_unprocessed,
    ),
    counter(
        "gsb_room_migrations_in_dropped_total",
        "Entities migrating into a shard (its neighbour's migrations_out) still in its inbox when it stopped, never installed, cumulative.",
        |r| r.stop.migrations_in_dropped,
    ),
    counter(
        "gsb_room_effects_unsent_total",
        "Remote effects a shard still held to send when it stopped (retry buffer, the tick's outbox), cumulative.",
        |r| r.stop.effects_unsent,
    ),
    counter(
        "gsb_room_effects_unapplied_total",
        "Remote effects a shard had received and not applied when it stopped, cumulative.",
        |r| r.stop.effects_unapplied,
    ),
    counter(
        "gsb_room_team_imports_unapplied_total",
        "Team imports (relayed visible sets, view copies) still in a shard's inbox when it stopped, cumulative.",
        |r| r.stop.team_imports_unapplied,
    ),
    counter(
        "gsb_room_border_updates_unapplied_total",
        "Border updates (a neighbour's strip exchange or resync request, view copies) still in a shard's inbox when it stopped, cumulative.",
        |r| r.stop.border_updates_unapplied,
    ),
];

#[cfg(test)]
mod tests;
