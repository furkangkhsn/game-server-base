//! The handshake times its steps (BACKLOG B2): a step answered without
//! a re-send is an RTT sample that seeds the reliable band's estimate,
//! and a step that had to be re-sent gives none (Karn's rule). The steps'
//! backoff stays in the handshake (BACKLOG B86): the band starts from
//! the estimate, or from the initial timer. Child of `handshake`.

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
/// band does NOT inherit the steps' backoff (BACKLOG B86): it starts from
/// the initial timer, so the AUTH/JOIN behind a lossy handshake is not
/// re-sent late.
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
    assert_eq!(
        c.rto(),
        crate::udp::rel::INITIAL_RTO,
        "the band starts without the handshake's backoff"
    );
}

/// A clean step seeds the estimate, and only the estimate: the proof's
/// re-sends (its accept lost twice) leave the band at the timer the
/// challenge's sample gave it, not doubled.
#[tokio::test]
async fn a_clean_challenge_seeds_the_band_without_the_proofs_backoff() {
    let (_listener, addr, _eps, _accept) = bound_transport(UdpTransportConfig::default()).await;
    let lose_two_accepts = first(2, |d| d.len() == 5 && d[0] == KIND_ACK);
    let via = relay(addr, keep(), lose_two_accepts).await;
    let c = healed(via).await;
    assert!(c.stats.proof_retries >= 2, "{:?}", c.stats);
    if c.stats.challenge_retries == 0 {
        // The challenge was answered cleanly (loopback, unless a busy
        // machine delayed it past its timer): its sample is the estimate,
        // and the timer is the estimate's — the floor on loopback.
        assert!(c.srtt().is_some(), "the clean challenge is a sample");
        assert_eq!(c.rto(), crate::udp::rel::MIN_RTO, "no backoff carried");
    } else {
        assert_eq!(c.rto(), crate::udp::rel::INITIAL_RTO, "no backoff carried");
    }
}
