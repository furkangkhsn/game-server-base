//! The sealed door's sans-IO parts: the handshake's byte layout, the
//! demux → writer send request, the DH budget on a paused clock, and the
//! counters' names.

use std::time::Duration;

use super::*;
use crate::seal::{Initiator, MSG1_LEN_MIN, Refusal, StaticKey};

/// The sealed proof is the plaintext proof (HELLO + the caps byte, now
/// always present) with Noise message 1 at byte 19: 67 bytes with the
/// empty payload rUDP sends; the accept is `ACK{1}` + message 2, 77.
#[test]
fn the_sealed_proof_and_accept_have_their_documented_layout() {
    let server = StaticKey::generate().unwrap();
    let (nonce, cookie) = (0x0102_0304_0506_0708_u64, 0x1112_1314_1516_1718_u64);
    let ini = Initiator::new(&server.public(), &context(nonce, cookie), &[]).unwrap();
    assert_eq!(ini.msg1().len(), MSG1_LEN_MIN);
    let p = encode_sealed_proof(nonce, cookie, CAP_CID, ini.msg1());
    assert_eq!(p.len(), 67);
    assert_eq!(p.len(), SEALED_PROOF_MIN);
    assert_eq!(p[0], KIND_HELLO);
    assert_eq!(p[1..9], nonce.to_le_bytes());
    assert_eq!(p[9..17], cookie.to_le_bytes());
    assert_eq!((p[17], p[18]), (0, CAP_CID), "the HELLO's pad, then caps");
    assert_eq!(&p[PROOF_MSG1_AT..], ini.msg1());
    // No caps asked: the byte is still there (msg1's offset is fixed).
    assert_eq!(encode_sealed_proof(nonce, cookie, 0, ini.msg1())[18], 0);
    // The context is the nonce and the cookie, little-endian.
    let c = context(nonce, cookie);
    assert_eq!(
        (&c[..8], &c[8..]),
        (&nonce.to_le_bytes()[..], &cookie.to_le_bytes()[..])
    );

    let msg2 = [0xAB; MSG2_LEN];
    let a = encode_sealed_accept(&msg2);
    assert_eq!(a.len(), 77);
    assert_eq!(a.len(), SEALED_ACCEPT_LEN);
    assert_eq!(&a[..5], &[KIND_ACK, 1, 0, 0, 0], "ACK{{1}} first");
    assert_eq!(&a[5..], &msg2);
}

/// A send request names its destination (none = the session's address,
/// or an address of either family) and carries the inner datagram whole;
/// a request without one, or with an unknown tag, decodes to nothing.
#[test]
fn a_send_request_round_trips() {
    let inner = encode_path_challenge(42);
    for to in [
        None,
        Some("127.0.0.1:4000".parse().unwrap()),
        Some("[::1]:4000".parse().unwrap()),
    ] {
        let b = encode_send(to, &inner);
        assert_eq!(decode_send(&b), Some((to, &inner[..])), "{to:?}");
    }
    assert_eq!(decode_send(&encode_send(None, &[])), None);
    assert_eq!(decode_send(&[9, 1, 2]), None);
    assert_eq!(decode_send(&[]), None);
}

/// Each record refusal has a transport counter of its own, named exactly
/// as the refusal (the stable `seal_*` names), in `Refusal::ALL` order.
#[test]
fn every_refusal_has_its_counter_under_its_own_name() {
    let names: Vec<&str> = gsb_core::metrics::TransportCounters::default()
        .fields()
        .iter()
        .map(|(n, _)| *n)
        .collect();
    let at: Vec<usize> = Refusal::ALL
        .iter()
        .map(|r| {
            names
                .iter()
                .position(|n| *n == r.name())
                .unwrap_or_else(|| panic!("{} has no counter", r.name()))
        })
        .collect();
    assert!(at.windows(2).all(|w| w[1] == w[0] + 1), "{at:?}");
    let mut c = Counts::default();
    for (i, r) in Refusal::ALL.iter().enumerate() {
        for _ in 0..=i {
            c.refusal(*r);
        }
    }
    assert_eq!(c.refused, [1, 2, 3, 4, 5, 6]);
}

/// The DH budget on a paused clock: the full bucket admits its burst (50
/// ms of the rate) back to back and refuses the next proof, counted; the
/// clock refills one token per `1 s / rate`; the bucket never holds more
/// than its burst however long it idles.
#[tokio::test(start_paused = true)]
async fn the_dh_budget_admits_its_burst_then_its_rate() {
    let mut b = DhBudget::new(Some(1000));
    assert_eq!(b.burst(), 50);
    let t0 = tokio::time::Instant::now();
    for i in 0..50 {
        assert!(b.admits(t0), "token {i}");
    }
    assert!(!b.admits(t0), "the 51st at once is refused");
    assert_eq!(b.refused, 1);
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(b.admits(tokio::time::Instant::now()), "1 ms buys one");
    assert!(!b.admits(tokio::time::Instant::now()));
    tokio::time::advance(Duration::from_secs(3600)).await;
    let now = tokio::time::Instant::now();
    let admitted = (0..1000).filter(|_| b.admits(now)).count();
    assert_eq!(admitted, 50, "an idle hour refills the burst, no more");
    assert_eq!(b.refused, 2 + 950);
}

/// A slow rate's bucket holds one token (never zero); `None` and `0`
/// mean no budget at all.
#[tokio::test(start_paused = true)]
async fn a_slow_budget_holds_one_token_and_zero_means_none() {
    let now = tokio::time::Instant::now();
    let mut slow = DhBudget::new(Some(10));
    assert_eq!(slow.burst(), 1);
    assert!(slow.admits(now));
    assert!(!slow.admits(now));
    tokio::time::advance(Duration::from_millis(99)).await;
    assert!(
        !slow.admits(tokio::time::Instant::now()),
        "100 ms per token"
    );
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(slow.admits(tokio::time::Instant::now()));
    for off in [None, Some(0)] {
        let mut b = DhBudget::new(off);
        assert!((0..10_000).all(|_| b.admits(now)), "{off:?}");
        assert_eq!(b.refused, 0);
    }
}

/// The security mode never prints a key; the static key's `Debug` shows
/// its public half only.
#[test]
fn no_debug_prints_the_private_key() {
    let k = StaticKey::generate().unwrap();
    let private = k.private_bytes();
    let hex: String = private.iter().map(|b| format!("{b:02x}")).collect();
    let dec = format!("{:?}", &private[..]);
    let key_shown = format!("{k:?}");
    let public = format!("{:?}", k.public());
    assert!(
        key_shown.contains(&public[1..public.len() - 1]),
        "{key_shown}"
    );
    let shown = format!("{key_shown} {:?}", UdpSecurity::Sealed(Arc::new(k)));
    assert!(
        !shown.contains(&hex) && !shown.contains(&dec[1..dec.len() - 1]),
        "{shown}"
    );
}

mod door;
mod rekey;
