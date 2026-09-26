//! The input port: the action channel's capacity.

use crate::room::RoomConfig;

fn capacity(n: usize) -> usize {
    RoomConfig {
        action_capacity: n,
        ..RoomConfig::default()
    }
    .action_channel()
    .0
    .max_capacity()
}

/// The configured capacity, except that zero is one slot (F21).
#[test]
fn the_action_channel_holds_the_capacity_and_zero_is_one() {
    assert_eq!(capacity(256), 256);
    assert_eq!(capacity(1), 1);
    assert_eq!(capacity(0), 1);
}
