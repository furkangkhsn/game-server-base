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

/// Under tokio's paused clock the ticks are stamped on that clock: two
/// consecutive stamps are one period apart (to the timer's millisecond
/// granularity, without drift), however little real time passes
/// (BACKLOG F10 — with wall-clock stamps they were microseconds apart and
/// a room's `dt` froze).
#[tokio::test(start_paused = true)]
async fn ticks_are_stamped_on_the_runtime_clock() {
    let (ticker, _task) = Ticker::spawn(30.0, 64).expect("rate");
    let mut rx = ticker.subscribe();
    let first = rx.recv().await.expect("tick").at;
    let mut prev = first;
    for _ in 0..30 {
        let at = rx.recv().await.expect("tick").at;
        let gap = at.saturating_duration_since(prev);
        assert!(
            gap.abs_diff(ticker.period()) <= Duration::from_millis(1),
            "tick gap {gap:?}"
        );
        prev = at;
    }
    let span = prev.saturating_duration_since(first);
    assert!(
        span.abs_diff(ticker.period() * 30) <= Duration::from_millis(1),
        "30 periods took {span:?}"
    );
}
