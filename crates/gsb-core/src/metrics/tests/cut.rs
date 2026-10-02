//! A periodic report waits, bounded, for a sharded room's round in
//! flight (BACKLOG F29): when the report falls due on the very tick the
//! shards sample — some shards' rows sent before the collector's drain,
//! the others just after — it goes out on the next tick, a consistent
//! cut; a shard that never sends holds it no longer than the grace.
//!
//! Paused clock, a hand-fed ticker: the tick a report falls due on, and
//! the grace, are exact. A report that goes out torn at the grace is
//! counted (BACKLOG F70: `reports_torn_at_cut_grace`).

use super::*;
use crate::shard::sample_id;

/// The tick period these tests feed: 40 ticks make exactly the report
/// period, so the collector's due tick IS the shards' sample tick.
const TICK: Duration = Duration::from_millis(25);
const SAMPLE_EVERY: u64 = 40;

struct Rig {
    ticks: broadcast::Sender<TickInfo>,
    events: mpsc::Sender<MetricsEvent>,
    reports: mpsc::UnboundedReceiver<MetricReport>,
    _collector: tokio::task::JoinHandle<bool>,
    tick: u64,
}

fn rig() -> Rig {
    let (ticks, tick_rx) = broadcast::channel::<TickInfo>(64);
    let (events, rx) = mpsc::channel::<MetricsEvent>(64);
    let (sink, reports) = mpsc::unbounded_channel::<MetricReport>();
    let c = MetricsCollector::new(
        tick_rx,
        rx,
        MetricSink::Channel(sink),
        Duration::from_secs(1),
    );
    Rig {
        ticks,
        events,
        reports,
        _collector: tokio::spawn(c.run()),
        tick: 0,
    }
}

impl Rig {
    /// Shard `index` of room 1 sends its sample at `steps` (and
    /// `lagged_ticks`).
    fn shard(&self, index: usize, steps: u64, lagged_ticks: u64) {
        let mut s = room_sample(sample_id(RoomId(1), index), Instant::now(), steps);
        s.lagged_ticks = lagged_ticks;
        self.events
            .try_send(MetricsEvent::Room(s))
            .expect("event queued");
    }

    /// Wait one tick period, then broadcast the next tick and let the
    /// collector take it (drain, maybe emit) before returning.
    async fn tick(&mut self) {
        tokio::time::sleep(TICK).await;
        self.tick += 1;
        self.ticks
            .send(TickInfo {
                tick: self.tick,
                at: crate::ticker::now(),
            })
            .expect("the collector subscribes");
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
    }

    /// Feed ticks up to tick `n` (the report falls due on tick 40).
    async fn tick_to(&mut self, n: u64) {
        while self.tick < n {
            self.tick().await;
        }
    }

    fn reports(&mut self) -> Vec<MetricReport> {
        std::iter::from_fn(|| self.reports.try_recv().ok()).collect()
    }
}

/// Room 1's rows of `report` as `(steps, lagged_ticks)`, in shard order.
fn rows(report: &MetricReport) -> Vec<(u64, u64)> {
    report
        .rooms
        .iter()
        .map(|r| (r.steps, r.lagged_ticks))
        .collect()
}

/// A report is a cut when every row agrees on `(steps, lagged_ticks)`.
fn is_cut(report: &MetricReport) -> bool {
    rows(report).windows(2).all(|w| w[0] == w[1])
}

/// The shards sample on schedule, on the tick the report falls due,
/// every period: shards 0 and 1 send before the collector's drain, 2
/// and 3 right after it. Every report is a consistent cut of four rows,
/// and the reports keep their once-a-second cadence.
#[tokio::test(start_paused = true)]
async fn reports_are_cuts_when_the_shards_sample_on_the_due_tick() {
    let mut rig = rig();
    for i in 0..4 {
        rig.shard(i, 0, 0);
    }
    for _ in 0..(10 * SAMPLE_EVERY) {
        let next = rig.tick + 1;
        let sampling = next.is_multiple_of(SAMPLE_EVERY);
        if sampling {
            rig.shard(0, next, 0);
            rig.shard(1, next, 0);
        }
        rig.tick().await;
        if sampling {
            rig.shard(2, next, 0);
            rig.shard(3, next, 0);
        }
    }
    let reports = rig.reports();
    assert!(reports.len() >= 9, "one a second: {}", reports.len());
    for r in &reports {
        assert_eq!(r.rooms.len(), 4);
        assert!(is_cut(r), "torn report: {:?}", rows(r));
    }
}

/// The round in flight at the due tick lands on the next tick: the
/// report waits one tick and carries the whole round.
#[tokio::test(start_paused = true)]
async fn a_round_split_by_the_due_tick_goes_out_whole_on_the_next() {
    let mut rig = rig();
    for i in 0..4 {
        rig.shard(i, 30, 0);
    }
    rig.tick_to(SAMPLE_EVERY - 1).await;
    assert!(rig.reports().is_empty(), "not due yet");
    rig.shard(0, 60, 0);
    rig.shard(1, 60, 0);
    rig.tick().await;
    assert!(rig.reports().is_empty(), "due, but a round is in flight");
    rig.shard(2, 60, 0);
    rig.shard(3, 60, 0);
    rig.tick().await;
    let reports = rig.reports();
    assert_eq!(reports.len(), 1, "out on the next tick");
    assert_eq!(rows(&reports[0]), vec![(60, 0); 4]);
    assert_eq!(reports[0].reports_torn_at_cut_grace, 0, "a cut is not torn");
}

/// A shard that never sends its round (dead, or stalled) holds the
/// report no longer than the grace from the due time: it goes out torn,
/// on the first tick at or past the grace.
#[tokio::test(start_paused = true)]
async fn a_shard_that_never_sends_holds_the_report_only_for_the_grace() {
    let mut rig = rig();
    for i in 0..4 {
        rig.shard(i, 30, 0);
    }
    rig.tick_to(SAMPLE_EVERY - 1).await;
    for i in 0..3 {
        rig.shard(i, 60, 0);
    }
    rig.tick().await;
    let due = tokio::time::Instant::now();
    let reports = loop {
        rig.tick().await;
        let reports = rig.reports();
        if !reports.is_empty() {
            break reports;
        }
        assert!(due.elapsed() < CUT_GRACE, "held past the grace");
    };
    let waited = due.elapsed();
    assert!(
        waited >= CUT_GRACE && waited < CUT_GRACE + TICK,
        "{waited:?}"
    );
    assert_eq!(reports.len(), 1);
    assert_eq!(rows(&reports[0]), vec![(60, 0), (60, 0), (60, 0), (30, 0)]);
    assert_eq!(
        reports[0].reports_torn_at_cut_grace, 1,
        "the torn report counts itself (F70)"
    );
}

/// F70: the count is cumulative — the torn report's 1 stays on the
/// reports after it, and a report that lines up adds nothing.
#[tokio::test(start_paused = true)]
async fn a_report_torn_at_the_grace_is_counted_once() {
    let mut rig = rig();
    for i in 0..4 {
        rig.shard(i, 30, 0);
    }
    rig.tick_to(SAMPLE_EVERY - 1).await;
    for i in 0..3 {
        rig.shard(i, 60, 0);
    }
    rig.tick_to(SAMPLE_EVERY * 3 / 2).await;
    let torn = rig.reports();
    assert_eq!(torn.len(), 1, "out at the grace");
    assert_eq!(torn[0].reports_torn_at_cut_grace, 1);
    rig.shard(3, 60, 0);
    rig.tick_to(SAMPLE_EVERY * 3).await;
    let after = rig.reports();
    assert!(!after.is_empty(), "the next period's report");
    for r in &after {
        assert!(is_cut(r), "lined up: {:?}", rows(r));
        assert_eq!(r.reports_torn_at_cut_grace, 1, "nothing added");
    }
}

/// Disagreements waiting cannot cure do not hold a report: rows apart on
/// `lagged_ticks` (a shard that missed ticks the others did not samples
/// on other ticks from then on), single rooms' rows, and a stopped
/// sharded room's lingering rows (each shard froze at its own step).
#[tokio::test(start_paused = true)]
async fn rows_waiting_cannot_line_up_do_not_hold_the_report() {
    let mut rig = rig();
    rig.shard(0, 60, 0);
    rig.shard(1, 60, 0);
    rig.shard(2, 58, 2);
    rig.shard(3, 57, 3);
    for (room, steps) in [(1, 60), (2, 30)] {
        let s = room_sample(RoomId(room), Instant::now(), steps);
        rig.events.try_send(MetricsEvent::Room(s)).expect("queued");
    }
    for (index, steps) in [(0, 61), (1, 59)] {
        let s = room_sample(sample_id(RoomId(2), index), Instant::now(), steps);
        rig.events
            .try_send(MetricsEvent::RoomFinal(s))
            .expect("queued");
    }
    rig.tick_to(SAMPLE_EVERY).await;
    let reports = rig.reports();
    assert_eq!(reports.len(), 1, "out on the due tick");
    assert_eq!(reports[0].rooms.len(), 8);
    assert_eq!(
        reports[0].reports_torn_at_cut_grace, 0,
        "never held, never counted"
    );
}

/// `with_cut_grace(Duration::ZERO)` never waits: the report goes out on
/// the due tick, torn, as before the wait existed.
#[tokio::test(start_paused = true)]
async fn a_zero_grace_never_waits() {
    let (ticks, tick_rx) = broadcast::channel::<TickInfo>(64);
    let (events, rx) = mpsc::channel::<MetricsEvent>(64);
    let (sink, reports) = mpsc::unbounded_channel::<MetricReport>();
    let c = MetricsCollector::new(
        tick_rx,
        rx,
        MetricSink::Channel(sink),
        Duration::from_secs(1),
    )
    .with_cut_grace(Duration::ZERO);
    let mut rig = Rig {
        ticks,
        events,
        reports,
        _collector: tokio::spawn(c.run()),
        tick: 0,
    };
    rig.shard(0, 30, 0);
    rig.shard(1, 60, 0);
    rig.tick_to(SAMPLE_EVERY).await;
    let reports = rig.reports();
    assert_eq!(reports.len(), 1, "out on the due tick");
    assert_eq!(rows(&reports[0]), vec![(30, 0), (60, 0)]);
    assert_eq!(
        reports[0].reports_torn_at_cut_grace, 1,
        "torn at a grace of zero"
    );
}
