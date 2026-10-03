//! The delay signal's inputs (module `signal`, round 4): the recent
//! minimum, and the jitter estimate seen through the threshold it sets —
//! a steady ramp adds nothing, alternating samples do, the slow cadence
//! does not move it, a corner counts at most the clip, the clamps.

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
    assert_eq!(r.recent_min(QUEUE_SAMPLES), None, "two of four");
    feed(&mut r, &mut t, 250, true, &[60, 70, 80]);
    assert_eq!(r.recent_min(QUEUE_SAMPLES), Some(ms(30)));
    assert_eq!(r.recent_min(PACED_QUEUE_SAMPLES), Some(ms(70)));
    assert_eq!(r.floor(), Some(ms(30)));
    feed(&mut r, &mut t, 250, true, &[90]);
    assert_eq!(r.recent_min(QUEUE_SAMPLES), Some(ms(60)), "30 is out");
}

/// A queue growing steadily — 400 ms a second, even where the cadence
/// changes under it — is no jitter: the threshold stays the limit.
#[test]
fn a_steady_ramp_is_no_jitter() {
    let mut t = Instant::now();
    let mut r = RttTrack::default();
    feed(&mut r, &mut t, 1000, false, &[20, 420]);
    let mut rtt = 420;
    for every in [250, 250, 500, 250, 250, 250] {
        rtt += 400 * every / 1000;
        feed(&mut r, &mut t, every, true, &[rtt]);
    }
    assert_eq!(r.threshold(), QUEUE_DELAY_LIMIT);
}

/// Samples alternating 40 ms apart at the fast cadence are jitter of 40:
/// the threshold settles at JITTER_MULT × 40 ms.
#[test]
fn alternating_samples_are_jitter() {
    let mut t = Instant::now();
    let mut r = RttTrack::default();
    alternate(&mut r, &mut t, 40, 80, 2 * JITTER_SAMPLES as usize);
    let want = JITTER_MULT * 40.0;
    assert!((thr_ms(&r) - want).abs() < 0.05 * want, "{}", thr_ms(&r));
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
    assert_eq!(r.deviations(), 2 * JITTER_SAMPLES - 2);
}

/// A spike — one sample 450 ms over a calm path — breaks the line three
/// times (as the middle, and as each neighbour); each break counts at
/// most the clip (three times the estimate, at least the limit), not
/// hundreds of milliseconds.
#[test]
fn a_corner_counts_at_most_the_clip() {
    let mut t = Instant::now();
    let mut r = RttTrack::default();
    alternate(&mut r, &mut t, 50, 60, 2 * JITTER_SAMPLES as usize);
    assert!((thr_ms(&r) - 40.0).abs() < 1.0, "jitter 10: {}", thr_ms(&r));
    feed(&mut r, &mut t, 250, true, &[50, 500, 50, 50]);
    assert!(thr_ms(&r) < 60.0, "{}", thr_ms(&r));
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
