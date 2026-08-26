//! Unit tests for [`super`] (moved out of the module file so the
//! implementation reads on its own; still a child module, so
//! `use super::*` reaches the parent's private items exactly as
//! before).

use super::*;

/// The spawn boundary rejects every rate without a usable period with
/// a typed error instead of panicking (`1.0 / hz` used to reach
/// `Duration::from_secs_f64`, which panics on exactly these inputs).
/// No tokio runtime needed: the error path returns before any task is
/// spawned; the success path is exercised by every registry/ticket
/// integration test through their live ticker.
#[test]
fn spawn_rejects_rates_without_a_period() {
    for bad in [0.0, -30.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(
            matches!(
                Ticker::spawn(bad, 64),
                Err(CoreError::InvalidTickRate { .. })
            ),
            "hz = {bad} must be rejected"
        );
    }
    // Absurdly high: 1/hz truncates below one nanosecond — a zero
    // period would busy-loop the runtime instead of ticking.
    assert!(matches!(
        Ticker::spawn(1e15, 64),
        Err(CoreError::InvalidTickRate { .. })
    ));
}
