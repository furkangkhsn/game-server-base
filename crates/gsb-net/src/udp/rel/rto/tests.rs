//! The retransmit timer's arithmetic (BACKLOG B2), on synthetic samples:
//! RFC 6298's estimator, the bounds, the backoff and its reset.

use super::*;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// Before any sample the timer is the initial one, which is the floor.
#[test]
fn before_a_sample_the_timer_is_the_initial_one() {
    let rto = Rto::default();
    assert_eq!(rto.srtt(), None);
    assert_eq!(rto.current(), INITIAL_RTO);
    assert_eq!(INITIAL_RTO, MIN_RTO);
}

/// The first sample sets SRTT = R and RTTVAR = R/2 (RFC 6298 §2.2); the
/// second follows §2.3 exactly (RTTVAR from the OLD SRTT, then SRTT).
#[test]
fn the_first_two_samples_follow_the_rfc() {
    let mut rto = Rto::default();
    rto.sample(ms(100));
    assert_eq!(rto.srtt(), Some(ms(100)));
    assert_eq!(rto.rttvar(), ms(50));
    assert_eq!(rto.current(), ms(100 + 4 * 50));
    rto.sample(ms(180));
    // RTTVAR = 3/4·50 + 1/4·|100 − 180| = 37.5 + 20; SRTT = 7/8·100 + 1/8·180.
    assert_eq!(rto.rttvar(), Duration::from_micros(57_500));
    assert_eq!(rto.srtt(), Some(ms(110)));
    assert_eq!(rto.current(), ms(110 + 230));
}

/// A steady path: SRTT converges to its RTT and the variation decays, so
/// the timer settles just above the RTT — from above and from below.
#[test]
fn srtt_converges_to_a_steady_rtt() {
    for (first, rtt) in [(ms(20), ms(300)), (ms(900), ms(120))] {
        let mut rto = Rto::default();
        rto.sample(first);
        for _ in 0..200 {
            rto.sample(rtt);
        }
        let srtt = rto.srtt().expect("sampled");
        assert!(
            srtt.abs_diff(rtt) <= ms(1),
            "{first:?}→{rtt:?}: SRTT {srtt:?}"
        );
        assert!(
            rto.rttvar() <= ms(1),
            "variation decays: {:?}",
            rto.rttvar()
        );
        let t = rto.current();
        assert!(t >= rtt && t <= rtt + ms(5), "{rtt:?}: timer {t:?}");
    }
}

/// A fast path never takes the timer below the floor, and a slow one
/// never above the ceiling.
#[test]
fn the_timer_stays_inside_its_bounds() {
    let mut fast = Rto::default();
    for _ in 0..50 {
        fast.sample(Duration::from_micros(80));
    }
    assert_eq!(fast.current(), MIN_RTO);
    let mut slow = Rto::default();
    slow.sample(ms(3000));
    assert_eq!(slow.current(), MAX_RTO);
}

/// Each timeout doubles the timer, up to the ceiling and no further.
#[test]
fn each_timeout_doubles_the_timer_up_to_the_ceiling() {
    let mut rto = Rto::default();
    let mut seen = vec![rto.current()];
    for _ in 0..8 {
        rto.timed_out();
        seen.push(rto.current());
    }
    let want: Vec<Duration> = [50, 100, 200, 400, 800, 1000, 1000, 1000, 1000]
        .into_iter()
        .map(ms)
        .collect();
    assert_eq!(seen, want);
}

/// The backoff doubles the ESTIMATED timer, and a valid sample ends it.
#[test]
fn a_valid_sample_ends_the_backoff() {
    let mut rto = Rto::default();
    rto.sample(ms(60));
    let settled = rto.current();
    assert_eq!(settled, ms(60 + 4 * 30));
    rto.timed_out();
    rto.timed_out();
    assert_eq!(rto.current(), settled * 4, "two timeouts: ×4");
    rto.sample(ms(60));
    assert!(
        rto.current() < settled,
        "the backoff is gone (and the variation shrank): {:?}",
        rto.current()
    );
}
