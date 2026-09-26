//! The rate rule: a divisor of the global rate, and a keep-alive no
//! faster than the room's own tick.

use crate::error::CoreError;
use crate::room::RoomConfig;

fn room(tick_hz: f64, keepalive_hz: f64) -> RoomConfig {
    RoomConfig {
        tick_hz,
        keepalive_hz,
        ..RoomConfig::default()
    }
}

/// A rate that divides the global one steps every k-th tick.
#[test]
fn a_dividing_rate_gives_its_divisor() {
    assert_eq!(room(30.0, 1.0).step_divisor(30.0).ok(), Some(1));
    assert_eq!(room(15.0, 1.0).step_divisor(60.0).ok(), Some(4));
    assert_eq!(room(10.0, 0.0).step_divisor(30.0).ok(), Some(3));
    // A disabled keep-alive is never compared.
    assert_eq!(room(1.0, -1.0).step_divisor(30.0).ok(), Some(30));
}

/// A rate that does not divide (or is no rate at all) is `TickRate`.
#[test]
fn a_rate_that_does_not_divide_is_refused() {
    for tick in [7.0, 45.0, 60.0, 0.0, -15.0, f64::NAN, f64::INFINITY] {
        let err = room(tick, 0.0).step_divisor(30.0).expect_err("refused");
        assert!(matches!(err, CoreError::TickRate { .. }), "{tick}: {err}");
    }
}

/// A keep-alive faster than the room's tick is `KeepaliveRate`; equal
/// is allowed.
#[test]
fn a_keepalive_faster_than_the_tick_is_refused() {
    let err = room(10.0, 12.0).step_divisor(30.0).expect_err("refused");
    assert!(matches!(err, CoreError::KeepaliveRate { .. }), "{err}");
    assert_eq!(room(10.0, 10.0).step_divisor(30.0).ok(), Some(3));
}
