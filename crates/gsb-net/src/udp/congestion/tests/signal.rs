//! The delay signal's inputs (module `signal`, round 4): the recent
//! minimum, and the jitter estimate seen through the threshold it sets —
//! a steady or accelerating queue adds nothing, normal jitter is its σ,
//! the slow cadence does not move it, a corner counts at most the clip,
//! the clamps.

use super::*;
use crate::udp::congestion::signal::{JITTER_MULT, JITTER_SAMPLES, RttTrack};

/// Samples `rtts` (ms), `every` ms apart from `t`, at the fast cadence
/// or not.
fn feed(r: &mut RttTrack, t: &mut Instant, every: u64, fast: bool, rtts: &[u64]) {
    for &rtt in rtts {
        *t += ms(every);
        r.sample(ms(rtt), *t, fast);
    }
}

/// Alternating `a`, `b` ms at the fast cadence, `n` samples.
fn alternate(r: &mut RttTrack, t: &mut Instant, a: u64, b: u64, n: usize) {
    for i in 0..n {
        feed(r, t, 250, true, &[if i % 2 == 0 { a } else { b }]);
    }
}

fn thr_ms(r: &RttTrack) -> f64 {
    r.threshold().as_secs_f64() * 1000.0
}

/// The smallest of the newest `n`, once there are `n`.
#[test]
fn the_recent_minimum_needs_its_samples() {
    let mut t = Instant::now();
    let mut r = RttTrack::default();
    assert_eq!((r.recent_min(1), r.floor()), (None, None));
    feed(&mut r, &mut t, 250, true, &[50, 30]);
    assert_eq!(r.recent_min(2), Some(ms(30)));
    assert_eq!(r.recent_min(4), None, "two of four");
    feed(&mut r, &mut t, 250, true, &[60, 70, 80]);
    assert_eq!(r.recent_min(4), Some(ms(30)));
    assert_eq!(r.recent_min(2), Some(ms(70)));
    assert_eq!(r.floor(), Some(ms(30)));
    feed(&mut r, &mut t, 250, true, &[90]);
    assert_eq!(r.recent_min(4), Some(ms(60)), "30 is out");
}

/// A queue growing steadily — 400 ms a second, even where the cadence
/// changes under it — or at a steadily growing pace (a paced session's
/// own additive increase) is no jitter: the threshold stays the limit.
#[test]
fn a_steady_or_accelerating_queue_is_no_jitter() {
    let mut t = Instant::now();
    let mut r = RttTrack::default();
    feed(&mut r, &mut t, 1000, false, &[20, 420]);
    let mut rtt = 420;
    for every in [250, 250, 500, 250, 250, 250] {
        rtt += 400 * every / 1000;
        feed(&mut r, &mut t, every, true, &[rtt]);
    }
    assert_eq!(r.threshold(), QUEUE_DELAY_LIMIT, "steady");
    assert_eq!(r.deviations(), 4, "a slow sample may open a fit");
    let mut r = RttTrack::default();
    for k in 0..12u64 {
        feed(&mut r, &mut t, 250, true, &[20 + 3 * k * k]);
    }
    assert_eq!(r.deviations(), 9);
    assert_eq!(r.threshold(), QUEUE_DELAY_LIMIT, "accelerating");
}

/// Normal jitter of σ is an estimate of σ: the threshold settles near
/// JITTER_MULT × σ (seeded draws, two hundred fast samples).
#[test]
fn normal_jitter_is_its_sigma() {
    let mut t = Instant::now();
    let mut r = RttTrack::default();
    let mut rng = Rng(11);
    for _ in 0..200 {
        let rtt = rng.normal(100.0, 20.0).max(0.0).round() as u64;
        feed(&mut r, &mut t, 250, true, &[rtt]);
    }
    let want = JITTER_MULT * 20.0;
    assert!((thr_ms(&r) - want).abs() < 0.15 * want, "{}", thr_ms(&r));
}

/// The slow cadence teaches it nothing: a wider swing at one sample a
/// second neither raises the estimate nor, calm, lowers it (a queue other
/// sessions raise and lower every few seconds looks like jitter there).
#[test]
fn the_slow_cadence_does_not_move_it() {
    let mut t = Instant::now();
    let mut r = RttTrack::default();
    alternate(&mut r, &mut t, 40, 80, 2 * JITTER_SAMPLES as usize);
    let learned = r.threshold();
    for i in 0..20 {
        feed(
            &mut r,
            &mut t,
            1000,
            false,
            &[if i % 2 == 0 { 40 } else { 200 }],
        );
    }
    assert_eq!(r.threshold(), learned, "a wide swing");
    feed(&mut r, &mut t, 1000, false, &[60; 20]);
    assert_eq!(r.threshold(), learned, "a calm path");
    assert_eq!(r.deviations(), 2 * JITTER_SAMPLES - 3);
}

/// A spike — one sample 450 ms over a calm path — breaks the parabola
/// four times (as the newest, and in each of the three fits after it);
/// each break counts at most the clip (three times the estimate), not
/// hundreds of milliseconds.
#[test]
fn a_corner_counts_at_most_the_clip() {
    let mut t = Instant::now();
    let mut r = RttTrack::default();
    let mut rng = Rng(5);
    for _ in 0..64 {
        let rtt = rng.normal(50.0, 10.0).round() as u64;
        feed(&mut r, &mut t, 250, true, &[rtt]);
    }
    let calm = thr_ms(&r);
    assert!((30.0..75.0).contains(&calm), "jitter 10: {calm}");
    feed(&mut r, &mut t, 250, true, &[50, 500, 50, 50, 50]);
    assert!(thr_ms(&r) < 1.6 * calm, "{calm} → {}", thr_ms(&r));
}

/// The threshold is the limit on a calm path and QUEUE_DELAY_MAX on the
/// wildest: no jitter hides a queue that long.
#[test]
fn the_threshold_is_clamped() {
    let mut t = Instant::now();
    let mut r = RttTrack::default();
    assert_eq!(r.threshold(), QUEUE_DELAY_LIMIT);
    alternate(&mut r, &mut t, 40, 42, 2 * JITTER_SAMPLES as usize);
    assert_eq!(r.threshold(), QUEUE_DELAY_LIMIT, "jitter 2 ms");
    alternate(&mut r, &mut t, 0, 400, 4 * JITTER_SAMPLES as usize);
    assert_eq!(r.threshold(), QUEUE_DELAY_MAX, "jitter 400 ms");
}
