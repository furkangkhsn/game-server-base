//! The server's feedback state on synthetic clocks and counts: when a
//! session is probed, how two answered probes become loss and an RTT,
//! and what a report that claims the impossible is allowed to change
//! (nothing but its own counter).

use super::*;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// A session that announced at `t0`, its first probe sent at once.
fn announced(t0: Instant) -> Feedback {
    let mut f = Feedback::new(t0);
    assert_eq!(f.on_report(0, 0, t0), Report::Announce);
    assert!(f.probe_due(t0), "the first probe goes at once");
    f.probe_sent(t0);
    f
}

fn send(f: &mut Feedback, n: u64) {
    for _ in 0..n {
        f.game_sent();
    }
}

/// A client that never announced is never probed, and a report it sends
/// anyway names a probe that does not exist.
#[test]
fn an_unannounced_session_is_never_probed() {
    let t0 = Instant::now();
    let mut f = Feedback::new(t0);
    send(&mut f, 50);
    assert!(!f.probe_due(t0 + ms(10_000)));
    assert_eq!(f.on_report(1, 50, t0), Report::Invalid);
    assert_eq!(f.estimate(), None);
    assert_eq!(f.counts.invalid, 1);
    assert_eq!(f.counts.probes_sent, 0);
}

/// After the first probe, the next is due an interval later; a probe the
/// socket refused is counted and waits an interval too.
#[test]
fn probes_go_once_per_interval() {
    let t0 = Instant::now();
    let mut f = announced(t0);
    assert!(!f.probe_due(t0 + PROBE_INTERVAL - ms(1)));
    assert!(f.probe_due(t0 + PROBE_INTERVAL));
    assert_eq!(f.next_probe().0, 2, "ids run up from 1");
    f.probe_failed(t0 + PROBE_INTERVAL);
    assert_eq!(f.counts.probes_send_failed, 1);
    assert!(!f.probe_due(t0 + PROBE_INTERVAL + ms(10)), "no retry storm");
    assert_eq!(f.next_probe().0, 2, "a refused probe spends no id");
    assert_eq!(f.counts.probes_sent, 1);
}

/// Two answered probes: the interval's datagrams sent, the client's
/// count of them, the RTT sample each — and the echo the next probe
/// carries.
#[test]
fn two_answered_probes_measure_loss_and_rtt() {
    let t0 = Instant::now();
    let mut f = announced(t0);
    send(&mut f, 10);
    assert_eq!(f.on_report(1, 0, t0 + ms(40)), Report::Applied(ms(40)));
    let e = f.estimate().expect("estimated");
    assert_eq!(
        (e.interval_sent, e.interval_lost),
        (0, 0),
        "nothing before probe 1"
    );
    assert_eq!(f.next_probe().1, 40_000, "the echo: the newest sample, µs");
    let t1 = t0 + PROBE_INTERVAL;
    f.probe_sent(t1);
    assert_eq!(f.next_probe().1, 0, "echoed once");
    send(&mut f, 10);
    // Probe 2 left after 10 game datagrams; the client saw 8 of them.
    assert_eq!(f.on_report(2, 8, t1 + ms(60)), Report::Applied(ms(60)));
    let e = f.estimate().expect("estimated");
    assert_eq!((e.interval_sent, e.interval_lost), (10, 2));
    assert_eq!(e.interval, PROBE_INTERVAL);
    assert_eq!((e.latest_rtt, e.min_rtt), (ms(60), ms(40)));
    assert!(
        (e.loss - 0.2).abs() < 1e-9,
        "the first interval that sent sets it"
    );
    assert_eq!(e.reports, 2);
    let c = f.counts;
    assert_eq!((c.reports, c.reported_sent, c.reported_lost), (2, 10, 2));
    assert_eq!((c.rtt_samples, c.rtt_sum_us), (2, 100_000));
}

/// The smoothed loss moves a quarter of the way per interval.
#[test]
fn the_loss_fraction_is_smoothed() {
    let t0 = Instant::now();
    let mut f = announced(t0);
    send(&mut f, 100);
    let (mut at, mut received) = (t0, 0u32);
    for (id, got) in [(1u32, 0u32), (2, 50), (3, 100)] {
        if id > 1 {
            at += PROBE_INTERVAL;
            f.probe_sent(at);
            send(&mut f, 100);
        }
        received += got;
        f.on_report(id, received, at + ms(1));
    }
    // Probe 2 covered the first 100 (50 lost), probe 3 the next 100 (none).
    let e = f.estimate().unwrap();
    assert!((e.loss - (0.5 - 0.5 / 4.0)).abs() < 1e-9, "{}", e.loss);
}

/// Datagrams that overtook a probe count for the interval before it:
/// the surplus is carried into the next one, not lost from it.
#[test]
fn a_reordering_surplus_is_carried_not_lost() {
    let t0 = Instant::now();
    let mut f = announced(t0);
    send(&mut f, 10);
    let t1 = t0 + PROBE_INTERVAL;
    f.probe_sent(t1); // probe 2 left after 10
    send(&mut f, 2); // two more, which overtake it
    f.on_report(1, 0, t0 + ms(5));
    assert_eq!(f.on_report(2, 12, t1 + ms(5)), Report::Applied(ms(5)));
    assert_eq!(f.estimate().unwrap().interval_lost, 0);
    assert_eq!(f.counts.clamped, 0, "12 had been sent by then");
    let t2 = t1 + PROBE_INTERVAL;
    send(&mut f, 8);
    f.probe_sent(t2); // probe 3 left after 20; the client has all 20
    f.on_report(3, 20, t2 + ms(5));
    let e = f.estimate().unwrap();
    assert_eq!(
        (e.interval_sent, e.interval_lost),
        (10, 0),
        "the two were not lost"
    );
    assert_eq!(f.counts.reported_lost, 0);
}

/// A count over what the server has sent by now is clamped to it (a
/// duplicated datagram, or a false claim) and counted — and a lie cannot
/// buy a later interval a lower loss.
#[test]
fn a_claim_beyond_what_was_sent_is_clamped() {
    let t0 = Instant::now();
    let mut f = announced(t0);
    send(&mut f, 10);
    let t1 = t0 + PROBE_INTERVAL;
    f.probe_sent(t1);
    f.on_report(1, 0, t0 + ms(1));
    assert_eq!(f.on_report(2, 1_000, t1 + ms(1)), Report::Applied(ms(1)));
    assert_eq!(f.counts.clamped, 1);
    assert_eq!(f.estimate().unwrap().interval_lost, 0);
    let t2 = t1 + PROBE_INTERVAL;
    send(&mut f, 10);
    f.probe_sent(t2);
    // The client "received" 4 of the next 10: no credit from the lie.
    f.on_report(3, 1_004, t2 + ms(1));
    let e = f.estimate().unwrap();
    assert_eq!((e.interval_sent, e.interval_lost), (10, 6));
}

/// A count that runs backwards is refused: nothing of it is applied, and
/// the probe it named can still be answered properly.
#[test]
fn a_count_that_runs_backwards_is_refused() {
    let t0 = Instant::now();
    let mut f = announced(t0);
    send(&mut f, 10);
    f.on_report(1, 0, t0 + ms(1));
    let t1 = t0 + PROBE_INTERVAL;
    f.probe_sent(t1);
    f.on_report(2, 0, t1); // nothing received; baseline stays 0
    let before = f.estimate();
    let t2 = t1 + PROBE_INTERVAL;
    f.probe_sent(t2);
    assert_eq!(f.on_report(3, u32::MAX, t2 + ms(1)), Report::Invalid);
    assert_eq!(f.estimate(), before, "nothing applied");
    assert_eq!(f.counts.invalid, 1);
    assert_eq!(f.on_report(3, 10, t2 + ms(2)), Report::Applied(ms(2)));
}

/// An id never sent is refused; an answered or superseded one is late —
/// and a superseded probe counts as unanswered.
#[test]
fn unknown_ids_are_invalid_and_answered_ones_late() {
    let t0 = Instant::now();
    let mut f = announced(t0);
    assert_eq!(f.on_report(7, 0, t0), Report::Invalid, "never sent");
    f.probe_sent(t0 + PROBE_INTERVAL); // probe 2
    assert_eq!(
        f.on_report(2, 0, t0 + PROBE_INTERVAL),
        Report::Applied(ms(0))
    );
    assert_eq!(f.counts.probes_unanswered, 1, "probe 1 was superseded");
    assert_eq!(f.on_report(1, 0, t0 + PROBE_INTERVAL), Report::Late);
    assert_eq!(
        f.on_report(2, 0, t0 + PROBE_INTERVAL),
        Report::Late,
        "a duplicate"
    );
    assert_eq!(
        (f.counts.late, f.counts.invalid, f.counts.reports),
        (2, 1, 1)
    );
}

/// The ring bounds the probes awaiting a report, and a session's end
/// counts the rest: every probe sent is answered or unanswered.
#[test]
fn every_probe_is_answered_or_counted_unanswered() {
    let t0 = Instant::now();
    let mut f = announced(t0);
    for k in 1..=PROBE_RING as u32 {
        f.probe_sent(t0 + PROBE_INTERVAL * k);
    }
    assert_eq!(
        f.counts.probes_unanswered, 1,
        "the oldest fell off the ring"
    );
    f.on_report(3, 0, t0 + PROBE_INTERVAL * 3);
    f.end();
    let c = f.counts;
    assert_eq!(c.probes_sent, PROBE_RING as u64 + 1);
    assert_eq!(c.probes_sent, c.reports + c.probes_unanswered);
}
