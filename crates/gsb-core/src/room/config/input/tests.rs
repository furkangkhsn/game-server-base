//! The input port: the action channel's capacity.

use crate::room::{InputRate, RoomConfig};

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

/// A limit is two positive numbers; either zero is no limit at all (a
/// bucket that admits nothing, or never refills) and is refused.
#[test]
fn an_input_rate_is_two_positive_numbers() {
    let r = InputRate::new(20, 5).expect("valid");
    assert_eq!((r.per_sec(), r.burst()), (20, 5));
    assert_eq!(InputRate::new(0, 5), None);
    assert_eq!(InputRate::new(20, 0), None);
    assert_eq!(InputRate::new(0, 0), None);
}

/// Off by default: a room built from the defaults limits nothing.
#[test]
fn the_default_room_has_no_input_limit() {
    assert_eq!(RoomConfig::default().input_rate, None);
}
