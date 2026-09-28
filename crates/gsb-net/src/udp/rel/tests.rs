//! The reliable band's sending half on a synthetic clock (BACKLOG B2):
//! where the RTT samples come from, Karn's rule, the backoff schedule of
//! one frame, the owner's wait, and the liveness bound it still keeps.

use super::*;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn band(t0: Instant) -> RelSend {
    RelSend::new(t0, Rto::default())
}

fn dg(seq: u32) -> Bytes {
    Bytes::from(vec![1, seq as u8])
}

/// A clean frame's ACK is one sample: the time from its send to the ACK.
#[test]
fn a_clean_ack_is_a_sample() {
    let t0 = Instant::now();
    let mut b = band(t0);
    b.push(1, dg(1), t0);
    b.on_ack(2, t0 + ms(80));
    assert!(b.is_empty());
    assert_eq!(b.rto().srtt(), Some(ms(80)));
}

/// A cumulative ACK that releases several frames samples the NEWEST one
/// (its arrival sent the ACK); an ACK that moves nothing samples nothing.
#[test]
fn a_cumulative_ack_samples_its_newest_frame() {
    let t0 = Instant::now();
    let mut b = band(t0);
    b.push(1, dg(1), t0);
    b.push(2, dg(2), t0 + ms(30));
    b.on_ack(3, t0 + ms(100));
    assert_eq!(b.rto().srtt(), Some(ms(70)));
    b.on_ack(3, t0 + ms(900));
    b.on_ack(2, t0 + ms(900));
    assert_eq!(b.rto().srtt(), Some(ms(70)), "a stale ACK is no sample");
}

/// Karn's rule: a frame that was sent again gives no sample — neither on
/// its own ACK nor on a cumulative ACK that also releases clean frames
/// queued behind it — and the backed-off timer is KEPT until a clean
/// frame is answered.
#[test]
fn karns_rule_no_sample_from_a_re_sent_frame() {
    let t0 = Instant::now();
    let mut b = band(t0);
    b.push(1, dg(1), t0);
    b.push(2, dg(2), t0 + ms(1));
    assert_eq!(b.poll(t0 + ms(50)), Due::Resend(dg(1)));
    b.resent(t0 + ms(50));
    b.on_ack(3, t0 + ms(60));
    assert!(b.is_empty(), "both released");
    assert_eq!(b.rto().srtt(), None, "no sample from an ambiguous ACK");
    assert_eq!(b.rto().current(), ms(100), "the backoff stays");
    // The next clean frame is the first sample, and it ends the backoff.
    b.push(3, dg(3), t0 + ms(200));
    b.on_ack(4, t0 + ms(210));
    assert_eq!(b.rto().srtt(), Some(ms(10)));
    assert_eq!(b.rto().current(), MIN_RTO);
}

/// One frame the peer never answers: re-sent at 50, 150, 350, 750 ms…
/// (the timer doubling from each re-send), never before its time.
#[test]
fn an_unanswered_frame_is_re_sent_on_a_doubling_schedule() {
    let t0 = Instant::now();
    let mut b = band(t0);
    b.push(1, dg(1), t0);
    let mut at = t0;
    for gap in [50, 100, 200, 400, 800, 1000, 1000] {
        assert_eq!(b.poll(at + ms(gap - 1)), Due::Wait, "early at {gap}");
        assert_eq!(b.wait(at + ms(gap - 1), ms(50)), ms(1));
        assert_eq!(b.poll(at + ms(gap)), Due::Resend(dg(1)), "due at {gap}");
        at += ms(gap);
        b.resent(at);
    }
    assert_eq!(b.len(), 1, "never abandoned on its own");
}

/// The owner's wait: the timer's remainder, capped at its tick; a whole
/// tick when nothing is outstanding or the re-send was refused.
#[test]
fn the_wait_is_the_timers_remainder_capped_at_the_tick() {
    let t0 = Instant::now();
    let mut b = band(t0);
    assert_eq!(b.wait(t0, ms(50)), ms(50), "idle");
    b.push(1, dg(1), t0);
    assert_eq!(b.wait(t0 + ms(20), ms(50)), ms(30));
    assert_eq!(b.wait(t0 + ms(20), ms(10)), ms(10), "capped");
    assert_eq!(b.wait(t0 + ms(60), ms(50)), ms(50), "overdue: a tick");
}

/// The liveness bound is unchanged by the timer: the band dies after
/// `REL_NO_ACK_FATAL` without ACK progress, counted from when the work
/// became outstanding (not from a stale ACK), and an ACK restarts it.
#[test]
fn the_liveness_bound_counts_from_outstanding_work() {
    let t0 = Instant::now();
    let mut b = band(t0);
    let later = t0 + ms(60_000);
    assert_eq!(b.poll(later), Due::Idle);
    b.push(1, dg(1), later);
    b.push(2, dg(2), later);
    b.on_ack(2, later + ms(3000));
    let dead = later + ms(3000) + REL_NO_ACK_FATAL;
    assert!(matches!(b.poll(dead - ms(1)), Due::Resend(_)));
    assert_eq!(b.poll(dead), Due::Dead(REL_NO_ACK_FATAL));
    assert_eq!(b.abandon(), 1);
    assert!(b.is_empty());
}
