//! The pacing queue on a synthetic clock: the bucket's rate and burst,
//! the oldest dropped past the budget (never the newest, never a message
//! on the wire, a fragmented one whole), the control band's charge, and
//! the ledger — every queued frame sent, dropped or unsent.

use super::*;
use crate::udp::congestion::queue::Released;

const RATE: f64 = 100_000.0; // bytes per second: 1000 B per 10 ms

fn dg(tag: u8, len: usize) -> Vec<u8> {
    vec![tag; len]
}

fn q(t: Instant) -> PaceQueue {
    let mut q = PaceQueue::new(BUDGET, t);
    q.start(t, RATE);
    q
}

/// Everything the queue releases at `now`.
fn drain(q: &mut PaceQueue, now: Instant) -> Vec<Released> {
    std::iter::from_fn(|| q.pop(now, RATE)).collect()
}

/// A full bucket holds one burst (rate × PACE_BURST, at least a
/// datagram); after it, a datagram per its own size's worth of time, and
/// `wait` says exactly when.
#[test]
fn the_bucket_releases_at_the_rate() {
    let t = Instant::now();
    let mut q = q(t);
    for tag in 0..8 {
        q.push(vec![dg(tag, 500)], false, usize::MAX);
    }
    let out = drain(&mut q, t);
    assert_eq!(out.len(), 2, "a 1000-byte burst: two 500-byte datagrams");
    assert_eq!(q.wait(t, RATE), Some(ms(5)));
    assert!(drain(&mut q, t + ms(4)).is_empty());
    assert_eq!(drain(&mut q, t + ms(5))[0].datagram, dg(2, 500));
    assert_eq!(
        drain(&mut q, t + ms(100)).len(),
        2,
        "no burst past the depth"
    );
    assert_eq!(drain(&mut q, t + ms(100) + ms(10)).len(), 2);
    assert_eq!(
        q.wait(t + ms(200), RATE),
        Some(Duration::ZERO),
        "one left, affordable"
    );
}

/// Past the budget the oldest go first, each counted once — and the
/// newest is kept even when it alone is over the budget.
#[test]
fn the_oldest_are_dropped_past_the_budget() {
    let t = Instant::now();
    let mut q = q(t);
    for tag in 0..4 {
        q.push(vec![dg(tag, 400)], false, 1000);
    }
    assert_eq!(q.counts.dropped, 2, "1600 B over a 1000 B budget");
    q.push(vec![dg(9, 1000)], false, 500);
    assert_eq!(q.counts.dropped, 4, "only the newest left, over the budget");
    let out = drain(&mut q, t + ms(1000));
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].datagram, dg(9, 1000));
    assert_eq!(q.counts.queued, 5);
}

/// A fragmented message is one unit: dropped whole, and never once its
/// first fragment is on the wire — the next oldest goes instead.
#[test]
fn a_fragmented_message_is_all_or_nothing() {
    let t = Instant::now();
    let mut q = q(t);
    let frags = |tag| vec![dg(tag, 1000), dg(tag, 1000), dg(tag, 1000)];
    q.push(frags(1), true, usize::MAX);
    let first = drain(&mut q, t);
    assert_eq!(
        first,
        [Released {
            datagram: dg(1, 1000),
            frag: true,
            last: false
        }]
    );
    q.push(frags(2), true, usize::MAX);
    q.push(vec![dg(3, 100)], false, 2500);
    assert_eq!(q.counts.dropped, 1, "message 2, whole");
    let rest: Vec<_> = (1..=4)
        .flat_map(|k| drain(&mut q, t + ms(10 * k)))
        .collect();
    let tags: Vec<_> = rest.iter().map(|r| (r.datagram[0], r.last)).collect();
    assert_eq!(tags, [(1, false), (1, true), (3, true)]);
}

/// The control band's bytes are charged: the game band waits for them.
#[test]
fn control_bytes_delay_the_game_band() {
    let t = Instant::now();
    let mut q = q(t);
    q.charge(t, RATE, 3000);
    q.push(vec![dg(1, 1000)], false, usize::MAX);
    assert!(drain(&mut q, t + ms(29)).is_empty());
    assert_eq!(drain(&mut q, t + ms(30)).len(), 1);
}

/// At the session's end what is queued is unsent — a message cut after
/// its first fragment included — and the ledger closes: queued = sent +
/// dropped + unsent.
#[test]
fn every_queued_frame_is_sent_dropped_or_unsent() {
    let t = Instant::now();
    let mut q = q(t);
    q.push(vec![dg(1, 600)], false, usize::MAX);
    q.push(vec![dg(2, 400), dg(2, 400)], true, usize::MAX);
    q.push(vec![dg(3, 900)], false, usize::MAX);
    let sent = drain(&mut q, t).iter().filter(|r| r.last).count() as u64;
    assert_eq!(sent, 1, "message 1; message 2 has one fragment out");
    q.abandon();
    let c = q.counts;
    assert_eq!((c.queued, c.dropped, c.unsent), (3, 0, 2));
    assert_eq!(c.queued, sent + c.dropped + c.unsent);
    assert!(q.is_empty() && q.pop_any().is_none());
}
