//! The collector-side hand-off: a report is handed on only when due, a
//! full slot drops the report and counts it, and the count travels with
//! the next report that gets through.

use super::*;

/// An empty report emitted `secs` after `t0`.
fn report_at(t0: Instant, secs: u64) -> MetricReport {
    let mut r = MetricReport::initial_stale(Duration::from_secs(1));
    r.emitted_at = t0 + Duration::from_secs(secs);
    r
}

fn exporter(interval_secs: u64) -> (OtlpExporter, OtlpPusher) {
    otlp(&OtlpConfig {
        endpoint: "http://127.0.0.1:9/v1/metrics".to_owned(),
        interval: Duration::from_secs(interval_secs),
        service_name: "gsb".to_owned(),
    })
    .expect("valid config")
}

/// One report per second for a 10 s interval: the first is handed on,
/// the next nine are not due (neither handed on nor counted as drops).
#[test]
fn only_a_due_report_is_handed_on() {
    let (mut e, mut p) = exporter(10);
    let t0 = Instant::now();
    for s in 0..10 {
        e.export(&report_at(t0, s));
    }
    let first = p.rx.try_recv().expect("the first report is due");
    assert_eq!(first.report.emitted_at, t0);
    assert!(p.rx.try_recv().is_err(), "nothing else was due");
    assert_eq!(e.dropped(), 0, "a report that is not due is not a drop");
    e.export(&report_at(t0, 10));
    let second = p.rx.try_recv().expect("one interval later it is due");
    assert_eq!(second.report.emitted_at, t0 + Duration::from_secs(10));
}

/// The push task is busy (nothing drains the one slot): the next due
/// reports are dropped and counted, never queued behind it; once the
/// slot frees, the next due report gets through carrying the count.
#[test]
fn a_full_slot_drops_and_counts_and_the_count_travels_on() {
    let (mut e, mut p) = exporter(10);
    let t0 = Instant::now();
    e.export(&report_at(t0, 0));
    e.export(&report_at(t0, 10));
    e.export(&report_at(t0, 20));
    assert_eq!(e.dropped(), 2);
    let first =
        p.rx.try_recv()
            .expect("the first report waited in the slot");
    assert_eq!(first.report.emitted_at, t0);
    assert_eq!(first.dropped, 0);
    assert!(
        p.rx.try_recv().is_err(),
        "the dropped ones were never queued"
    );
    e.export(&report_at(t0, 30));
    let next =
        p.rx.try_recv()
            .expect("the freed slot takes the next due one");
    assert_eq!(next.report.emitted_at, t0 + Duration::from_secs(30));
    assert_eq!(next.dropped, 2, "the drops ride the next hand-off");
    assert_eq!(
        next.time_unix_nano - first.time_unix_nano,
        30_000_000_000,
        "the stamps keep the reports' own spacing"
    );
}

/// A push task that is gone (its receiver dropped) is a drop per due
/// report, not a panic or a stall.
#[test]
fn a_gone_push_task_is_counted_as_drops() {
    let (mut e, p) = exporter(1);
    drop(p);
    let t0 = Instant::now();
    for s in 0..3 {
        e.export(&report_at(t0, s));
    }
    assert_eq!(e.dropped(), 3);
}

/// A zero interval refuses to build (it would hand on every report and
/// make the due grid meaningless).
#[test]
fn a_zero_interval_refuses() {
    let r = otlp(&OtlpConfig {
        endpoint: "http://127.0.0.1:4318".to_owned(),
        interval: Duration::ZERO,
        service_name: "gsb".to_owned(),
    });
    assert_eq!(r.err(), Some(OtlpError::ZeroInterval));
}
