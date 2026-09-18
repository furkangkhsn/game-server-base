//! The room actor's duration counters: the `*_min_us` fields must be
//! minima, not first observations.

use super::*;
use crate::room::actor::RoomActor;

/// A room actor the test STEPS directly (no spawn, no ticker task), so
/// the counters can be read off the sample between steps — together with
/// the peers that must outlive it: closing the control mailbox or the
/// logic's receivers would change what a step does.
struct BareRoom {
    actor: RoomActor<(), (), ()>,
    _control: Mailbox<RoomControl>,
    _dts: mpsc::Receiver<Duration>,
    _ops: mpsc::Receiver<u16>,
}

fn bare_room() -> BareRoom {
    let (_tick_tx, tick_rx) = broadcast::channel(16);
    let (control, control_rx) = channel(16);
    let (dts, dt_rx) = mpsc::channel(64);
    let (ops, op_rx) = mpsc::channel(64);
    BareRoom {
        actor: RoomActor::new(
            RoomConfig {
                id: RoomId(11),
                keepalive_hz: 0.0,
                metrics_cadence_hz: 0.0,
                ..Default::default()
            },
            (),
            Box::new(RecLogic { dts, ops }),
            tick_rx,
            control_rx,
            1,
            null_metrics_tx(),
            None,
        ),
        _control: control,
        _dts: dt_rx,
        _ops: op_rx,
    }
}

/// A tick stamped `late_by` in the PAST: the actor's measured tick
/// latency is `step start − t.at`, so the test sets the latency it wants
/// instead of racing the scheduler for it.
fn tick_late_by(tick: u64, late_by: Duration) -> TickInfo {
    TickInfo {
        tick,
        at: Instant::now()
            .checked_sub(late_by)
            .expect("the monotonic clock is further from its epoch than the test's offset"),
    }
}

/// `late_min_us` must be a MINIMUM — a later, smaller tick latency has to
/// pull it down.
///
/// It did not. The room actor seeded the min/max pair on `steps == 1` and
/// then only ever raised the maximum; nothing lowered the minimum, so the
/// field carried the FIRST step's latency for the process's whole life.
/// The first step is the coldest one, so the field reported a number that
/// was not merely stale but systematically the WRONG END of the
/// distribution — under a name (`min`) that sits in the same log line and
/// the same Prometheus family as a `max` that really is a maximum.
///
/// Deterministic, not timing-dependent: the test owns `t.at`. A tick
/// stamped 50 ms in the past is observed ≥ 50 ms late; the next, stamped
/// 2 ms in the past, ≥ 2 ms — a gap no scheduling jitter in a bare,
/// empty-world step can close.
#[tokio::test]
async fn a_faster_tick_lowers_the_rooms_late_minimum() {
    let mut r = bare_room();
    let a = &mut r.actor;

    assert!(a.step(&tick_late_by(1, Duration::from_millis(50))));
    let seeded = a.sample().late_min_us;
    assert!(
        seeded >= 50_000,
        "the first step must observe the 50 ms the tick was stamped late: {seeded} µs"
    );

    assert!(a.step(&tick_late_by(2, Duration::from_millis(2))));

    let s = a.sample();
    assert_eq!(s.steps, 2, "two steps were run");
    assert!(
        s.late_min_us < seeded,
        "a later, smaller tick latency must lower the reported minimum: \
         late_min_us {} still equals the first step's {seeded} µs",
        s.late_min_us
    );
    assert!(
        s.late_min_us < 20_000,
        "late_min_us {} µs must track the ~2 ms observation, not the 50 ms one",
        s.late_min_us
    );
    // The seeding half of the contract: a minimum initialised to 0 could
    // never be lowered and would report 0 forever, so it must start from
    // a real observation — and here, with both ticks stamped milliseconds
    // in the past, stay well above zero.
    assert!(
        s.late_min_us > 0,
        "late_min_us must be seeded from the first observation, never left at 0"
    );
    assert!(
        s.late_max_us >= seeded,
        "the maximum must still hold the 50 ms observation: {} µs",
        s.late_max_us
    );
    assert!(
        s.late_min_us <= s.late_max_us,
        "min {} must not exceed max {}",
        s.late_min_us,
        s.late_max_us
    );
}

/// The whole `step_*` / `late_*` family must stay mutually consistent
/// over a run: both minima at or below their means, both means at or
/// below their maxima, and neither minimum left at the zero it was
/// initialised to once steps have been observed.
///
/// `step_min_us` is the half that cannot be driven by the test clock (the
/// step body's duration is whatever it is), so it is pinned here by the
/// relation the buggy code violated: holding the FIRST step's duration
/// makes `min > mean` as soon as the cold first step is slower than the
/// warm rest — which is exactly what the loadgen kept printing
/// (`step_min_us=187` beside `step_p50_fine_us=32`). The exact-value
/// lock for this path is the deterministic unit test on
/// `RoomCounters::observe_step_us`.
#[tokio::test]
async fn duration_counters_stay_mutually_consistent() {
    let mut r = bare_room();
    let a = &mut r.actor;

    // Enough steps that a cold first one cannot pass for the typical
    // case, all stamped a fixed 3 ms late so `late_*` is bounded too.
    for t in 1..=64 {
        assert!(a.step(&tick_late_by(t, Duration::from_millis(3))));
    }

    let s = a.sample();
    assert_eq!(s.steps, 64, "sixty-four steps were run");

    let step_mean = s.step_sum_us as f64 / s.steps as f64;
    assert!(
        s.step_min_us as f64 <= step_mean,
        "step_min_us {} µs must not exceed the mean {step_mean:.1} µs — a \
         \"minimum\" above the average is the first-observation bug",
        s.step_min_us
    );
    assert!(
        s.step_min_us <= s.step_max_us,
        "step_min_us {} must not exceed step_max_us {}",
        s.step_min_us,
        s.step_max_us
    );

    let late_mean = s.late_sum_us as f64 / s.steps as f64;
    assert!(
        s.late_min_us as f64 <= late_mean,
        "late_min_us {} µs must not exceed the mean {late_mean:.1} µs",
        s.late_min_us
    );
    assert!(
        s.late_min_us > 0,
        "every tick was stamped 3 ms late, so the observed minimum latency \
         cannot be the zero the counter was initialised to"
    );
}

/// The ROOM actor fills the fine step-duration histogram, once per step.
///
/// The shard actor's twin of this is
/// `shard::tests::metrics::sharded_step_fills_the_fine_duration_histogram`,
/// and it exists because the shard's copy of this path was silently
/// never incrementing it (CHANGELOG "park sızıntısı + shard metrik
/// boşluğu turu"): the field was sent, always zero, and the percentile
/// helper's `None` on an empty histogram made the Prometheus p50/p99
/// lines for those rooms disappear entirely. The room side was correct
/// but untested at the ACTOR level — the accounting lives in
/// `RoomCounters::observe_step_us`, which both actors call and which has
/// its own unit test, so the only thing left unlocked was whether this
/// actor calls it. That is exactly what was broken on the other one.
#[tokio::test]
async fn the_rooms_step_fills_the_fine_duration_histogram() {
    let mut r = bare_room();
    let a = &mut r.actor;
    for t in 1..=5 {
        assert!(a.step(&tick_late_by(t, Duration::from_millis(1))));
    }

    let s = a.sample();
    assert_eq!(s.steps, 5, "five steps were run");
    assert_eq!(
        s.step_hist.iter().sum::<u64>(),
        s.steps,
        "the coarse histogram counts every step"
    );
    // Every step of a bare, empty-world room is orders of magnitude
    // under the fine cap (`FINE_HIST_CAP_US` = 4096 µs), so all five
    // must be present — a step at or above the cap is deliberately
    // absent from this histogram, and none of these can be.
    let fine: u64 = s.step_fine_hist.iter().copied().map(u64::from).sum();
    assert_eq!(
        fine, s.steps,
        "the room's step path must increment the fine histogram once per \
         step, the way the shard actor's does"
    );
    // And it must bin the duration the step actually MEASURED, not some
    // other number: the bin `step_max_us` falls in has to be occupied.
    let max_bin = crate::metrics::fine_hist_index(s.step_max_us)
        .expect("this rig's steps are far under the fine cap");
    assert!(
        s.step_fine_hist[max_bin] > 0,
        "the bin step_max_us ({} µs) lands in ({max_bin}) must be occupied",
        s.step_max_us
    );
}
