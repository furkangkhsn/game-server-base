//! The pending validation's rules, sans-IO (synthetic clock).

use super::*;

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 1], port))
}

/// The first challenge is due at once; the next only a
/// [`CHALLENGE_RESEND`] later — however many datagrams arrive between.
#[test]
fn a_challenge_is_due_at_once_then_once_per_interval() {
    let t0 = Instant::now();
    let mut p = PathProbe::new(addr(1), 7, 20, t0);
    assert!(p.challenge_due(t0));
    p.challenged(CHALLENGE_LEN, true, t0);
    assert!(!p.challenge_due(t0 + CHALLENGE_RESEND / 2));
    assert!(p.challenge_due(t0 + CHALLENGE_RESEND));
}

/// The amplification budget: never more than 3× the bytes received
/// from the candidate. A 2-byte datagram affords 6 bytes — not a 9-byte
/// challenge; one more 1-byte datagram makes 9 affordable exactly.
#[test]
fn the_budget_is_three_times_what_the_candidate_sent() {
    let t0 = Instant::now();
    let mut p = PathProbe::new(addr(1), 7, 2, t0);
    assert!(!p.affordable(CHALLENGE_LEN), "6 B of budget, 9 B challenge");
    p.heard(1);
    assert!(p.affordable(CHALLENGE_LEN), "9 B of budget: exactly");
    p.challenged(CHALLENGE_LEN, true, t0);
    assert!(!p.affordable(1), "spent");
    p.heard(3);
    assert!(p.affordable(CHALLENGE_LEN), "18 B more of budget");
    assert!(!p.affordable(CHALLENGE_LEN + 1));
}

/// A challenge the socket refused costs no budget, but waits out the
/// interval like a sent one (no spinning on a refusing socket).
#[test]
fn a_refused_challenge_costs_no_budget() {
    let t0 = Instant::now();
    let mut p = PathProbe::new(addr(1), 7, 3, t0);
    p.challenged(CHALLENGE_LEN, false, t0);
    assert!(p.affordable(CHALLENGE_LEN));
    assert!(!p.challenge_due(t0));
}

/// The response must come from the candidate and echo its nonce.
#[test]
fn only_the_candidate_with_the_nonce_answers() {
    let p = PathProbe::new(addr(1), 0xABCD, 9, Instant::now());
    assert!(p.answered_by(addr(1), 0xABCD));
    assert!(!p.answered_by(addr(2), 0xABCD), "another address");
    assert!(!p.answered_by(addr(1), 0xABCE), "another nonce");
}

/// A validation runs out exactly at [`VALIDATION_TIMEOUT`].
#[test]
fn a_validation_times_out() {
    let t0 = Instant::now();
    let p = PathProbe::new(addr(1), 7, 9, t0);
    assert!(!p.timed_out(t0 + VALIDATION_TIMEOUT - Duration::from_millis(1)));
    assert!(p.timed_out(t0 + VALIDATION_TIMEOUT));
}

/// The writer's `UDP_PATH` payload round-trips both families; anything
/// else is refused.
#[test]
fn the_path_change_payload_round_trips() {
    for a in [
        "192.0.2.7:4000".parse::<SocketAddr>().unwrap(),
        "[2001:db8::9]:65535".parse().unwrap(),
    ] {
        assert_eq!(decode_addr(&encode_addr(a)), Some(a));
    }
    assert_eq!(decode_addr(&[]), None);
    assert_eq!(decode_addr(&[5, 1, 2, 3, 4, 0, 0]), None);
    assert_eq!(decode_addr(&[4, 1, 2, 3, 4, 0]), None, "short port");
}
