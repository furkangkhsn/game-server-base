//! The change-detection window (KIT-ARCHITECTURE §4.4): the kit closes
//! it — `World::clear_trackers`, world-wide — exactly once per tick, at
//! the end of `update` (the same lock as the open room's, §8.3).

use super::*;
use crate::identity::WireId;

/// §8.3: without a per-tick `clear_trackers` the world's removed-
/// component buffers only grow — every despawn a leave ever caused stays
/// buffered for the room's lifetime (the core never calls it: there is
/// no system scheduler). The room closes the window in `update`, so a
/// tick's despawn is readable until that tick's update and gone after
/// it: the buffer is bounded by one tick's churn, not by the room's age.
#[test]
fn team_room_closes_the_change_window_every_tick() {
    let mut world = World::new();
    let mut room = TeamRoom::new(25.0);
    for tick in 1..=5u64 {
        let admission = room.on_join(&mut world, ConnectionId(tick));
        room.on_leave(&mut world, admission.player);
        assert_eq!(
            world.removed::<WireId>().count(),
            1,
            "tick {tick}: only this tick's despawn is pending"
        );
        room.update(&mut world, &ctx(tick));
        assert_eq!(
            world.removed::<WireId>().count(),
            0,
            "tick {tick}: the update closed the window"
        );
    }
}
