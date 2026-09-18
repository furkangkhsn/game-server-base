//! Falling behind the global ticker: what the room DOES about it
//! (skip the missed indices, catch up through the next step's
//! wall-clock `dt`) and what it REPORTS about it (`lagged_events` /
//! `lagged_ticks` — the room's "am I missing ticks" signal).
//!
//! The behaviour half has been tested since the tick loop existed; the
//! counters had never been read back, so the only thing standing behind
//! the operator's lag signal was the source.

use super::*;

#[tokio::test]
async fn lagged_receiver_catches_up_and_keeps_stepping() {
    let (dt_tx, mut dts) = mpsc::channel(16);
    let (op_tx, _ops) = mpsc::channel(16);
    // Buffer of 2: flooding it makes the receiver lag deterministically
    // *before* the room starts consuming.
    let (tick_tx, lagged_rx) = broadcast::channel(2);
    let (_control, control_rx) = channel(16);
    let t0 = Instant::now();
    let period = Duration::from_secs_f64(1.0 / 30.0);
    for i in 1..=10u64 {
        tick_tx
            .send(TickInfo {
                tick: i,
                at: t0 + Duration::from_secs_f64(i as f64 * period.as_secs_f64()),
            })
            .expect("channel open");
    }
    let actor = RoomActor::new(
        RoomConfig {
            id: RoomId(1),
            ..Default::default()
        },
        (),
        Box::new(RecLogic {
            dts: dt_tx,
            ops: op_tx,
        }),
        lagged_rx,
        control_rx,
        1,
        null_metrics_tx(),
        None,
    );
    let handle = tokio::spawn(actor.run());

    // The room skips the lagged ticks (Lagged → continue) and steps on
    // the two still-buffered ticks (9 and 10), then the sender is
    // dropped → Closed → clean exit.
    drop(tick_tx);
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("room did not exit on closed ticker")
        .expect("room task panicked");
    let mut count = 0;
    while let Ok(dt) = dts.try_recv() {
        count += 1;
        assert!(dt <= period * 2);
    }
    assert_eq!(count, 2, "expected exactly the two buffered ticks");
}

/// The lag COUNTERS, on the same real path: `lagged_events` counts the
/// occurrences and `lagged_ticks` the tick indices missed by them.
///
/// The two are a pair and each answers a different question — "how often
/// did this room fall off the ticker" vs. "how much simulated time did it
/// skip" — so a single occurrence that missed eight indices must read
/// `1` and `8`, not `1`/`1` or `8`/`8`. The room's `steps` is the third
/// leg: the missed indices are SKIPPED, never stepped, so the counter and
/// the step count must add up to the ticks that were sent.
///
/// Deterministic, not timing-dependent: the ticks are all sent before the
/// spawned actor is ever polled (no await in between on this
/// single-threaded test runtime), so the receiver is exactly eight
/// behind when it first calls `recv()`.
#[tokio::test]
async fn lag_counts_one_event_and_every_missed_tick_index() {
    let (dt_tx, _dts) = mpsc::channel(16);
    let (op_tx, _ops) = mpsc::channel(16);
    // Buffer of 2 against 10 sent ticks: 8 missed, then ticks 9 and 10
    // are still buffered and get stepped.
    let (tick_tx, lagged_rx) = broadcast::channel(2);
    let (_control, control_rx) = channel(16);
    let (metrics_tx, mut metrics_rx) = mpsc::channel::<MetricsEvent>(16);
    let t0 = Instant::now();
    let period = Duration::from_secs_f64(1.0 / 30.0);
    for i in 1..=10u64 {
        tick_tx
            .send(TickInfo {
                tick: i,
                at: t0 + Duration::from_secs_f64(i as f64 * period.as_secs_f64()),
            })
            .expect("channel open");
    }
    let actor = RoomActor::new(
        RoomConfig {
            id: RoomId(1),
            tick_hz: 30.0,
            // Sample on EVERY step: the default 1 Hz cadence against a
            // 30 Hz tick would first emit on step 30, and this room only
            // ever takes two steps.
            metrics_cadence_hz: 30.0,
            ..Default::default()
        },
        (),
        Box::new(RecLogic {
            dts: dt_tx,
            ops: op_tx,
        }),
        lagged_rx,
        control_rx,
        1,
        metrics_tx,
        None,
    );
    let handle = tokio::spawn(actor.run());
    drop(tick_tx);
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("room did not exit on closed ticker")
        .expect("room task panicked");

    let mut latest = None;
    while let Ok(ev) = metrics_rx.try_recv() {
        if let MetricsEvent::Room(s) = ev {
            latest = Some(s);
        }
    }
    let s = latest.expect("the room sampled on each of its two steps");
    assert_eq!(
        s.lagged_events, 1,
        "one `Lagged` occurrence, however many indices it swallowed"
    );
    assert_eq!(
        s.lagged_ticks, 8,
        "ten ticks sent into a two-deep buffer: eight indices were missed"
    );
    assert_eq!(
        s.steps, 2,
        "the missed indices are SKIPPED, not stepped: only the two still \
         buffered ticks produced steps"
    );
}
