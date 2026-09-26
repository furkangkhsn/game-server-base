//! The input gate's arithmetic (docs/SECURITY.md, "post-auth input
//! volume"): a token bucket of `burst` actions refilled at `per_sec`,
//! carried across the rooms a connection moves through. Synthetic
//! instants — the bucket reads no clock of its own.

use std::time::{Duration, Instant};

use super::{InputBucket, InputGate};
use crate::room::InputRate;

fn rate(per_sec: u32, burst: u32) -> InputRate {
    InputRate::new(per_sec, burst).expect("non-zero")
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// How many of `n` arrivals spaced `every` apart, from `t0`, pass.
fn passed(b: &mut InputBucket, t0: Instant, every: Duration, n: u32) -> u32 {
    (0..n).filter(|i| b.admit(t0 + every * *i)).count() as u32
}

/// A fresh bucket holds exactly `burst`: that many pass at one instant,
/// the next is refused.
#[test]
fn a_full_bucket_passes_its_burst_then_refuses() {
    let t0 = Instant::now();
    for burst in [1, 5, 30] {
        let mut b = InputBucket::full(rate(10, burst), t0);
        assert_eq!(passed(&mut b, t0, Duration::ZERO, burst + 3), burst);
    }
}

/// Refill is `per_sec` tokens a second, continuously: one token after
/// 1/rate, none before it.
#[test]
fn the_bucket_refills_at_its_rate() {
    let t0 = Instant::now();
    let mut b = InputBucket::full(rate(10, 1), t0);
    assert!(b.admit(t0));
    assert!(!b.admit(t0 + ms(99)), "a token takes 100 ms at 10/s");
    assert!(b.admit(t0 + ms(100)), "the refused arrival cost nothing");
    assert!(!b.admit(t0 + ms(150)));
    assert!(b.admit(t0 + ms(200)));
}

/// Idle time never buys more than `burst`.
#[test]
fn an_idle_bucket_holds_no_more_than_its_burst() {
    let t0 = Instant::now();
    let mut b = InputBucket::full(rate(10, 4), t0);
    let later = t0 + Duration::from_secs(3600);
    assert_eq!(passed(&mut b, later, Duration::ZERO, 10), 4);
}

/// A sender AT the rate is never refused, burst 1 included: the honest
/// client the limit must not touch.
#[test]
fn a_sender_at_the_rate_is_never_refused() {
    let t0 = Instant::now();
    for burst in [1, 10] {
        let mut b = InputBucket::full(rate(10, burst), t0);
        assert_eq!(passed(&mut b, t0, ms(100), 500), 500, "burst {burst}");
    }
}

/// A flood at 1000/s for one second against 10/s + burst 5: the burst,
/// plus one second of refill — never more.
#[test]
fn a_flood_gets_the_burst_plus_the_refill() {
    let t0 = Instant::now();
    let mut b = InputBucket::full(rate(10, 5), t0);
    let got = passed(&mut b, t0, ms(1), 1000);
    assert_eq!(got, 5 + 9, "999 ms of refill at 10/s is 9 tokens");
}

/// Extremes neither panic nor overflow: the largest rate after the
/// longest idle is still just the burst.
#[test]
fn extremes_saturate_at_the_burst() {
    let t0 = Instant::now();
    let mut b = InputBucket::full(rate(u32::MAX, u32::MAX), t0);
    assert!(b.admit(t0 + Duration::from_secs(u32::MAX as u64)));
    let mut b = InputBucket::full(rate(u32::MAX, 2), t0);
    let later = t0 + Duration::from_secs(1 << 40);
    assert_eq!(passed(&mut b, later, Duration::ZERO, 5), 2);
    // An instant before the last one (never produced by a monotonic
    // clock) refills nothing and does not rewind the bucket.
    let mut b = InputBucket::full(rate(10, 1), t0 + ms(500));
    assert!(b.admit(t0 + ms(500)));
    assert!(!b.admit(t0));
    assert!(!b.admit(t0 + ms(599)));
    assert!(b.admit(t0 + ms(600)));
}

/// Off (no limited room yet, or an unlimited room): everything passes.
#[test]
fn an_off_gate_passes_everything() {
    let t0 = Instant::now();
    let mut g = InputGate::default();
    assert!(!g.is_on());
    assert!((0..10_000).all(|_| g.admit(t0)));
    g.enter(None, t0);
    assert!(!g.is_on());
    assert!((0..10_000).all(|_| g.admit(t0)));
}

/// The first limited room starts the bucket full; an unlimited room
/// turns the gate off; coming back does NOT refill it — tokens accrue
/// with time, never with room hops (a leave/join loop through any room
/// cannot buy a fresh burst).
#[test]
fn room_hops_carry_the_bucket() {
    let t0 = Instant::now();
    let mut g = InputGate::default();
    g.enter(Some(rate(10, 3)), t0);
    assert!(g.is_on());
    assert_eq!((0..5).filter(|_| g.admit(t0)).count(), 3);
    // Through an unlimited room and back, 50 ms later: still empty.
    g.enter(None, t0 + ms(10));
    assert!(g.admit(t0 + ms(10)), "the unlimited room passes");
    g.enter(Some(rate(10, 3)), t0 + ms(50));
    assert!(!g.admit(t0 + ms(50)), "the hop bought nothing");
    assert!(g.admit(t0 + ms(100)), "time did");
}

/// A room with a different limit re-tunes the carried bucket: the level
/// is clamped to the new burst, a larger burst grants nothing, and the
/// new rate refills from then on.
#[test]
fn a_new_limit_retunes_the_carried_bucket() {
    let t0 = Instant::now();
    let mut g = InputGate::default();
    g.enter(Some(rate(10, 8)), t0);
    g.enter(Some(rate(100, 2)), t0);
    assert_eq!((0..5).filter(|_| g.admit(t0)).count(), 2, "clamped to 2");
    g.enter(Some(rate(100, 50)), t0);
    assert!(!g.admit(t0), "a larger burst grants no tokens");
    assert!(g.admit(t0 + ms(10)), "the new rate refills: 1 per 10 ms");
}

/// Time spent under one room's limit earns THAT room's rate: a second
/// in a 1/s room is one token on arrival in a 100/s room, not a hundred.
#[test]
fn time_earns_the_rate_of_the_room_it_was_spent_in() {
    let t0 = Instant::now();
    let mut g = InputGate::default();
    g.enter(Some(rate(1, 10)), t0);
    assert_eq!((0..10).filter(|_| g.admit(t0)).count(), 10, "emptied");
    let t1 = t0 + Duration::from_secs(1);
    g.enter(Some(rate(100, 10)), t1);
    assert_eq!((0..10).filter(|_| g.admit(t1)).count(), 1);
}
