//! Round 3's additions to the feedback state: the writer's cadence, the
//! silent client's backoff (BACKLOG B91), the bytes an interval sent and
//! the windowed minimum RTT (B93).

use super::*;

/// Send one probe at `at`; whether it pushed an unanswered one off the
/// ring.
fn probe(f: &mut Feedback, at: Instant) -> bool {
    assert!(f.probe_due(at), "due at {at:?}");
    f.probe_sent(at)
}

/// A client that stopped answering: once a whole ring is unanswered,
/// every further eviction doubles the interval, up to 8×; one answer
/// brings the writer's cadence back.
#[test]
fn a_silent_client_is_probed_ever_less_often() {
    let t0 = Instant::now();
    let mut f = announced(t0);
    let mut at = t0;
    for _ in 1..PROBE_RING {
        at += PROBE_INTERVAL;
        assert!(!probe(&mut f, at), "the ring is not full yet");
    }
    let mut gaps = Vec::new();
    for _ in 0..5 {
        let mut next = at + PROBE_INTERVAL;
        while !f.probe_due(next) {
            next += ms(100);
        }
        gaps.push((next - at).as_secs());
        at = next;
        assert!(probe(&mut f, at), "one evicted unanswered");
    }
    assert_eq!(gaps, [1, 2, 4, 8, 8]);
    assert_eq!(f.counts.probes_unanswered, 5);
    let id = f.next_probe().0 - 1;
    assert!(matches!(f.on_report(id, 0, at + ms(5)), Report::Applied(_)));
    assert!(!f.probe_due(at + PROBE_INTERVAL - ms(1)));
    assert!(f.probe_due(at + PROBE_INTERVAL), "the cadence is back");
}

/// The writer's interval is the cadence (a suspected or paced session is
/// probed faster).
#[test]
fn the_writer_sets_the_cadence() {
    let t0 = Instant::now();
    let mut f = announced(t0);
    f.set_interval(ms(250));
    assert!(!f.probe_due(t0 + ms(249)));
    assert!(f.probe_due(t0 + ms(250)));
}

/// An interval carries the bytes its game datagrams took, as well as
/// their count.
#[test]
fn an_interval_counts_its_bytes() {
    let t0 = Instant::now();
    let mut f = announced(t0);
    f.on_report(1, 0, t0 + ms(10));
    for len in [300, 700, 1472] {
        f.game_sent(len);
    }
    let t1 = t0 + PROBE_INTERVAL;
    f.probe_sent(t1); // probe 2 left after the three
    f.game_sent(1200); // the next interval's
    f.on_report(2, 3, t1 + ms(10));
    let e = f.estimate().unwrap();
    assert_eq!((e.interval_sent, e.interval_sent_bytes), (3, 2472));
}

/// Round trips sampled at `at` seconds after `t0` (the first, 40 ms, at
/// once): the windowed minimum after each.
fn windowed(samples: &[(u32, u64)]) -> Vec<(u128, u128)> {
    let t0 = Instant::now();
    let mut f = announced(t0);
    f.on_report(1, 0, t0 + ms(40));
    samples
        .iter()
        .map(|&(k, rtt)| {
            let at = t0 + PROBE_INTERVAL * k;
            f.probe_sent(at);
            let id = f.next_probe().0 - 1;
            f.on_report(id, 0, at + ms(rtt));
            let e = f.estimate().unwrap();
            (e.min_rtt.as_millis(), e.window_min_rtt.as_millis())
        })
        .collect()
}

/// The window slides by halves: each half-window the older half is
/// forgotten and the newer one kept — the lifetime minimum stays.
#[test]
fn the_minimum_rtt_is_windowed() {
    assert_eq!(
        windowed(&[(1, 90), (5, 60), (10, 90)]),
        [(40, 40), (40, 40), (40, 60)]
    );
}

/// A whole window of silence forgets both halves.
#[test]
fn a_silent_window_forgets_its_minimum() {
    assert_eq!(windowed(&[(11, 90)]), [(40, 90)]);
}
