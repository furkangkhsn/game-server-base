//! The sample FLUSH itself: the A2 cadence, and the A3 drop counter that
//! makes a saturating metrics channel visible.
//!
//! `metrics_dropped` is the last counter in the room sample with no
//! real-path test, and it is the awkward one: the only way to raise it is
//! to make the room's own outlet fail, which is also the outlet the count
//! has to travel on. It is reported precisely so an operator can see the
//! collector falling behind (DESIGN §12: harmless — the counters are
//! cumulative, so the next sample carries everything — but not invisible).
//! A counter that could never actually be read would defeat that, and
//! this test is what says it can.

use super::*;
use crate::room::actor::RoomActor;

/// A room whose metrics channel is ONE deep and which samples on every
/// step: the test controls exactly when the outlet is free.
fn rig(metrics_tx: mpsc::Sender<MetricsEvent>) -> RoomActor<(), (), ()> {
    let (_tick_tx, tick_rx) = broadcast::channel(16);
    let (_control, control_rx) = channel(16);
    let (dts, _dt_rx) = mpsc::channel(64);
    let (ops, _op_rx) = mpsc::channel(64);
    // The peers are dropped with this function's scope on purpose: an
    // empty world with no members takes the same step either way, and
    // these tests read the sample, not the fan-out.
    RoomActor::new(
        RoomConfig {
            id: RoomId(31),
            tick_hz: 30.0,
            keepalive_hz: 0.0,
            metrics_cadence_hz: 30.0, // sample every step
            ..Default::default()
        },
        (),
        Box::new(RecLogic { dts, ops }),
        tick_rx,
        control_rx,
        1,
        metrics_tx,
        None,
    )
}

fn tick(n: u64) -> TickInfo {
    TickInfo {
        tick: n,
        at: Instant::now(),
    }
}

/// A full metrics channel drops the sample and COUNTS it, and the count
/// reaches the next sample that gets through.
///
/// Both halves matter. The first is the architecture's promise: the
/// bounded channel is written with a synchronous `try_send` precisely so
/// the tick body never awaits (DESIGN §12), and the price is a dropped
/// sample — which must not be silent. The second is what makes the price
/// bounded: the counters are cumulative, so the very next sample that
/// gets out carries both the drop count AND everything the dropped
/// samples would have said. A `metrics_dropped` that were reset on flush,
/// or incremented on the wrong branch, would break the only signal an
/// operator has for a collector falling behind.
#[tokio::test]
async fn a_full_metrics_channel_drops_the_sample_and_counts_it() {
    let (tx, mut rx) = mpsc::channel::<MetricsEvent>(1);
    let mut a = rig(tx);

    // Step 1 fills the one-deep channel.
    assert!(a.step(&tick(1)));
    assert_eq!(
        a.sample().metrics_dropped,
        0,
        "the first sample went through"
    );

    // Steps 2 and 3 find it full: both samples are dropped and counted.
    assert!(a.step(&tick(2)));
    assert!(a.step(&tick(3)));
    assert_eq!(
        a.sample().metrics_dropped,
        2,
        "two samples were dropped on a full channel and both counted"
    );

    // Free the outlet and step again: the next sample carries the drop
    // count and the full cumulative state, so nothing was actually lost.
    let first = rx.try_recv().expect("the step-1 sample is buffered");
    let MetricsEvent::Room(first) = first else {
        panic!("the room sends room samples")
    };
    assert_eq!(first.steps, 1, "the buffered sample is step 1's");
    assert_eq!(first.metrics_dropped, 0, "nothing had been dropped yet");

    assert!(a.step(&tick(4)));
    let MetricsEvent::Room(latest) = rx.try_recv().expect("step 4's sample got out") else {
        panic!("the room sends room samples")
    };
    assert_eq!(
        latest.metrics_dropped, 2,
        "the drop count travels on the next sample that gets through"
    );
    assert_eq!(
        latest.steps, 4,
        "and so does everything the dropped samples would have carried: \
         the counters are cumulative, which is why the drop is harmless"
    );
}

/// The A2 cadence: with `metrics_cadence_hz` below the tick rate the room
/// samples every `tick_hz / metrics_cadence_hz` steps — and a step that
/// does not sample is not a drop.
///
/// The separation is the point. Both a skipped sample and a dropped one
/// mean "no sample this step", and conflating them would make every
/// normally-configured room (1 Hz cadence against a 30 Hz tick) report 29
/// drops a second and look permanently saturated.
#[tokio::test]
async fn a_skipped_cadence_step_is_not_a_dropped_sample() {
    let (tx, mut rx) = mpsc::channel::<MetricsEvent>(16);
    let (_tick_tx, tick_rx) = broadcast::channel(16);
    let (_control, control_rx) = channel(16);
    let (dts, _dt_rx) = mpsc::channel(64);
    let (ops, _op_rx) = mpsc::channel(64);
    let mut a = RoomActor::new(
        RoomConfig {
            id: RoomId(32),
            tick_hz: 30.0,
            keepalive_hz: 0.0,
            metrics_cadence_hz: 10.0, // one sample every 3 steps
            ..Default::default()
        },
        (),
        Box::new(RecLogic { dts, ops }),
        tick_rx,
        control_rx,
        1,
        tx,
        None,
    );

    for t in 1..=6 {
        assert!(a.step(&tick(t)));
    }

    let mut got = Vec::new();
    while let Ok(MetricsEvent::Room(s)) = rx.try_recv() {
        got.push(s.steps);
    }
    assert_eq!(
        got,
        vec![3, 6],
        "six steps at a one-in-three cadence emit exactly two samples"
    );
    assert_eq!(
        a.sample().metrics_dropped,
        0,
        "the four steps that did not sample are the CADENCE, not drops: \
         counting them would make every default-configured room look \
         permanently saturated"
    );
}
