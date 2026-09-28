//! The handshake times its steps (BACKLOG B2): a step answered without
//! a re-send is an RTT sample that seeds the reliable band's estimate,
//! and a step that had to be re-sent gives none (Karn's rule) — its
//! backoff is what the band starts with. Child of `handshake`.

use super::*;

/// A clean loopback handshake: the challenge (or the proof), answered at
/// once, seeds the estimate — the band does not start from the floor
/// blind. (A step that loopback scheduling delayed past its timer is
/// re-sent and gives no sample, so the assertion is on the clean steps.)
#[tokio::test]
async fn a_clean_handshake_seeds_the_rtt_estimate() {
    let (_listener, addr, _eps, _accept) = bound_transport(UdpTransportConfig::default()).await;
    let c = healed(addr).await;
    if c.stats.challenge_retries == 0 || c.stats.proof_retries == 0 {
        let srtt = c.srtt().expect("a clean step is a sample");
        assert!(srtt < Duration::from_secs(1), "loopback: {srtt:?}");
    }
}

/// Every step re-sent (the first challenge and the first accept lost):
/// neither answer can say which copy it answers, so no sample — and the
/// band starts with the backed-off timer, not the floor.
#[tokio::test]
async fn a_handshake_whose_every_step_was_re_sent_takes_no_sample() {
    let (_listener, addr, _eps, _accept) = bound_transport(UdpTransportConfig::default()).await;
    let (mut hello, mut accept) = (0, 0);
    let lose_first_answers: Rule = Box::new(move |d| match d[0] {
        KIND_HELLO if hello == 0 => {
            hello += 1;
            true
        }
        KIND_ACK if accept == 0 => {
            accept += 1;
            true
        }
        _ => false,
    });
    let via = relay(addr, keep(), lose_first_answers).await;
    let c = healed(via).await;
    assert!(c.stats.challenge_retries >= 1 && c.stats.proof_retries >= 1);
    assert_eq!(c.srtt(), None, "Karn: no sample from a re-sent step");
    assert!(
        c.rto() >= crate::udp::rel::INITIAL_RTO * 4,
        "two re-sends, two doublings kept: {:?}",
        c.rto()
    );
}
