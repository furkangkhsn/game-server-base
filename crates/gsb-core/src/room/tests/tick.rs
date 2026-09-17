//! The tick loop: rates, catch-up, lag, shutdown, and the config
//! period every budget is measured against.

use super::*;

#[tokio::test]
async fn room_steps_on_ticks_and_pulls_actions() {
    let (dt_tx, mut dts) = mpsc::channel(16);
    let (op_tx, mut ops) = mpsc::channel(16);
    let mut h = Harness::new(
        1,
        RoomConfig {
            id: RoomId(1),
            ..Default::default()
        },
        RecLogic {
            dts: dt_tx,
            ops: op_tx,
        },
    );
    let period = Duration::from_secs_f64(1.0 / 30.0);

    let (entity, actions) = h.join(ConnectionId(7), 1).await;
    assert_eq!(entity, 1);

    // Actions flow over the per-connection channel and are pulled at
    // the next step.
    actions
        .send(Action {
            conn: ConnectionId(7),
            player: PlayerId(7),
            op: 0x1001,
            payload: bytes::Bytes::new(),
        })
        .await
        .unwrap();
    h.tick(period);
    h.tick(period);

    // 3 steps so far (one from the join helper, two here): dts are the
    // exact nominal period (synthetic timestamps).
    for _ in 0..3 {
        let dt = tokio::time::timeout(Duration::from_secs(2), dts.recv())
            .await
            .expect("timed out")
            .expect("dts closed");
        assert!(
            dt.abs_diff(period) < Duration::from_micros(1),
            "dt {dt:?} != period {period:?}"
        );
    }
    let op = tokio::time::timeout(Duration::from_secs(2), ops.recv())
        .await
        .expect("timed out")
        .expect("ops closed");
    assert_eq!(op, 0x1001);

    h.shutdown().await;
}

#[tokio::test]
async fn catchup_clamps_dt_after_long_gap() {
    let (dt_tx, mut dts) = mpsc::channel(16);
    let (op_tx, _ops) = mpsc::channel(16);
    let mut h = Harness::new(
        1,
        RoomConfig {
            id: RoomId(1),
            ..Default::default()
        }, // max_catchup = 4
        RecLogic {
            dts: dt_tx,
            ops: op_tx,
        },
    );
    let period = Duration::from_secs_f64(1.0 / 30.0);

    // Two normal steps, then a 2 s gap: the step's dt must be clamped
    // to 4 periods (frame-rate independence in steady state; bounded
    // slow-motion across the stall).
    h.tick(period);
    h.tick(period);
    h.next_tick += 1; // consume index 3 as "missed"
    let at = h.t0 + period * 4 + Duration::from_secs(2);
    h.tick_tx
        .send(TickInfo { tick: 4, at })
        .expect("subscriber alive");

    let first = dts.recv().await.expect("dts");
    let second = dts.recv().await.expect("dts");
    let third = dts.recv().await.expect("dts");
    assert!(first.abs_diff(period) < Duration::from_micros(1));
    assert!(second.abs_diff(period) < Duration::from_micros(1));
    assert!(
        third.abs_diff(period * 4) < Duration::from_micros(1),
        "clamped dt {third:?} != 4 * period {period:?}"
    );

    h.shutdown().await;
}

#[tokio::test]
async fn slower_room_steps_on_every_kth_global_tick() {
    let (dt_tx, mut dts) = mpsc::channel(16);
    let (op_tx, _ops) = mpsc::channel(16);
    // Room at 15 Hz under a 60 Hz global ticker: run_every = 4.
    let mut h = Harness::new(
        4,
        RoomConfig {
            id: RoomId(1),
            tick_hz: 15.0,
            ..Default::default()
        },
        RecLogic {
            dts: dt_tx,
            ops: op_tx,
        },
    );
    let global_period = Duration::from_secs_f64(1.0 / 60.0);

    for _ in 0..8 {
        h.tick(global_period);
    }

    // Steps happened on ticks 4 and 8 only: 2 dts of 4 global periods.
    let room_period = Duration::from_secs_f64(1.0 / 15.0);
    for _ in 0..2 {
        let dt = tokio::time::timeout(Duration::from_secs(2), dts.recv())
            .await
            .expect("timed out")
            .expect("dts closed");
        assert!(
            dt.abs_diff(room_period) < Duration::from_micros(1),
            "dt {dt:?} != room period {room_period:?}"
        );
    }

    h.shutdown().await;
}

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

#[tokio::test]
async fn room_exits_when_ticker_closes() {
    let (dt_tx, _dts) = mpsc::channel(16);
    let (op_tx, _ops) = mpsc::channel(16);
    let (tick_tx, tick_rx) = broadcast::channel(4);
    let (_control, control_rx) = channel(16);
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
        tick_rx,
        control_rx,
        1,
        null_metrics_tx(),
        None,
    );
    let handle = tokio::spawn(actor.run());
    drop(tick_tx); // ticker aborted → broadcast closes
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("room did not exit on closed ticker")
        .expect("room task panicked");
}

#[test]
fn config_period() {
    let c = RoomConfig {
        tick_hz: 30.0,
        ..Default::default()
    };
    assert!((c.period().as_secs_f64() - 1.0 / 30.0).abs() < 1e-9);
}

/// `period` is total for hand-built configs: every rate without a
/// usable period yields the typed-in-spirit fallback instead of the
/// `Duration::from_secs_f64` panic (mirrors ticker.rs's
/// `spawn_rejects_rates_without_a_period`; the registry rejects these
/// configs, but direct construction bypasses it — see `period`'s
/// docs). An absurdly HIGH rate truncates its sub-nanosecond period
/// to zero and falls back too (a zero period would divide-by-zero
/// every cadence derivation downstream).
#[test]
fn period_is_total_for_hand_built_configs() {
    for bad in [0.0, -30.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1e15] {
        let c = RoomConfig {
            tick_hz: bad,
            ..Default::default()
        };
        assert_eq!(
            c.period(),
            FALLBACK_TICK_PERIOD,
            "tick_hz = {bad} must fall back, not panic"
        );
    }
    // A normal rate is untouched by the guard.
    let c = RoomConfig {
        tick_hz: 60.0,
        ..Default::default()
    };
    assert!((c.period().as_secs_f64() - 1.0 / 60.0).abs() < 1e-9);
}
