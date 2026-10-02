//! The QUIC path machine over synthetic statistics: what each quinn
//! counter becomes, the phase's way in and out, and the windowed floor.

use std::time::Duration;

use gsb_core::path::PathPhase;
use tokio::time::Instant;

use super::*;

const MS: Duration = Duration::from_millis(1);

/// A sample `ms` after `t0` with the given cumulative counters.
fn at(t0: Instant, ms: u64, rtt_ms: u64, cwnd: u64, events: u64) -> Sample {
    Sample {
        at: t0 + MS * ms as u32,
        rtt: MS * rtt_ms as u32,
        cwnd,
        congestion_events: events,
        lost_packets: 0,
        sent_packets: 0,
        tx_bytes: 0,
    }
}

#[test]
fn the_first_sample_is_an_open_path_with_its_round_trip() {
    let t0 = Instant::now();
    let mut p = QuicPath::default();
    let s = p.on_sample(at(t0, 0, 40, 12_000, 0));
    assert_eq!(s.phase, PathPhase::Open);
    assert_eq!(s.rtt, Some(40 * MS));
    assert_eq!(s.queue_delay, Some(Duration::ZERO));
    assert_eq!((s.rate, s.demand, s.loss_permille), (None, None, None));
}

#[test]
fn an_interval_gives_demand_and_loss() {
    let t0 = Instant::now();
    let mut p = QuicPath::default();
    p.on_sample(Sample {
        tx_bytes: 1_000,
        sent_packets: 10,
        lost_packets: 1,
        ..at(t0, 0, 40, 12_000, 0)
    });
    let s = p.on_sample(Sample {
        tx_bytes: 26_000,
        sent_packets: 210,
        lost_packets: 11,
        ..at(t0, 250, 40, 12_000, 0)
    });
    assert_eq!(s.demand, Some(100_000), "25 000 B in a quarter second");
    assert_eq!(s.loss_permille, Some(50), "10 of 200");
    // An interval that sent no packet has no loss to tell.
    let s = p.on_sample(Sample {
        tx_bytes: 26_000,
        sent_packets: 210,
        lost_packets: 11,
        ..at(t0, 500, 40, 12_000, 0)
    });
    assert_eq!((s.demand, s.loss_permille), (Some(0), None));
}

#[test]
fn one_congestion_event_is_suspect_and_two_pace_at_the_window() {
    let t0 = Instant::now();
    let mut p = QuicPath::default();
    p.on_sample(at(t0, 0, 50, 20_000, 0));
    let s = p.on_sample(at(t0, 250, 50, 20_000, 1));
    assert_eq!((s.phase, s.rate), (PathPhase::Suspect, None));
    let s = p.on_sample(at(t0, 500, 50, 20_000, 1));
    assert_eq!(s.phase, PathPhase::Open, "a clean interval clears it");
    p.on_sample(at(t0, 750, 50, 20_000, 2));
    let s = p.on_sample(at(t0, 1_000, 50, 10_000, 3));
    assert_eq!(s.phase, PathPhase::Paced);
    assert_eq!(s.rate, Some(200_000), "10 000 B per 50 ms round trip");
}

#[test]
fn a_paced_path_opens_only_when_the_window_outruns_the_demand() {
    let t0 = Instant::now();
    let mut p = QuicPath::default();
    p.on_sample(at(t0, 0, 100, 10_000, 0));
    p.on_sample(at(t0, 250, 100, 10_000, 1));
    let s = p.on_sample(at(t0, 500, 100, 10_000, 2));
    assert_eq!(s.rate, Some(100_000));
    // Clean, but the connection offers 90 000 B/s: 100 000 < 1.25 × that.
    let s = p.on_sample(Sample {
        tx_bytes: 22_500,
        ..at(t0, 750, 100, 10_000, 2)
    });
    assert_eq!((s.phase, s.rate), (PathPhase::Paced, Some(100_000)));
    // Clean, and offering 80 000 B/s: 100 000 = 1.25 × that — open.
    let s = p.on_sample(Sample {
        tx_bytes: 42_500,
        ..at(t0, 1_000, 100, 10_000, 2)
    });
    assert_eq!((s.phase, s.rate), (PathPhase::Open, None));
}

#[test]
fn the_queue_is_the_round_trip_over_a_windowed_floor() {
    let t0 = Instant::now();
    let mut p = QuicPath::default();
    p.on_sample(at(t0, 0, 20, 10_000, 0));
    let s = p.on_sample(at(t0, 1_000, 80, 10_000, 0));
    assert_eq!(s.queue_delay, Some(60 * MS));
    // A longer route held for a whole window becomes the floor: the old
    // 20 ms bucket is gone after two buckets.
    for k in 2..=11 {
        p.on_sample(at(t0, 1_000 * k, 80, 10_000, 0));
    }
    let s = p.on_sample(at(t0, 12_000, 80, 10_000, 0));
    assert_eq!(s.queue_delay, Some(Duration::ZERO));
}

#[test]
fn the_window_rate_is_bounded_below_a_millisecond_and_saturates() {
    assert_eq!(capacity(1_000, Duration::from_micros(10)), 1_000_000);
    assert_eq!(capacity(u64::MAX, MS), u32::MAX);
}
