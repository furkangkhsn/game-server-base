//! Stateless reset, demux side (B5b): a record whose CID no session
//! holds — here, a "restarted" demux under the same static key that
//! never knew the session — is answered with that CID's token, in a
//! datagram shorter than the trigger and only while the door's reset
//! budget allows; a door with resets off sends none.

use super::*;
use crate::seal::{RESET_LEN_MAX, RESET_LEN_MIN, Refusal, reset_tail};
use crate::udp::sealed::DhBudget;

/// A sealed session on demux `d` (client `a`), then a second demux under
/// the same static key that never saw it: the restarted server.
async fn restarted() -> (Demux, Client, Accept, Session) {
    let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    sock.writable().await.expect("writable");
    let (mut d, _end_rx, server) = sealed_demux(sock, None);
    let a = client();
    let (accept, session, _, _) = handshake(&mut d, &a, &server).await;
    let key = d.seal.as_ref().unwrap().key.clone();
    let sock2 = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    sock2.writable().await.expect("writable");
    let (mut d2, _) = demux_bare(sock2);
    d2.seal = Some(DoorSeal::new(key, None));
    (d2, a, accept, session)
}

fn record(tx: &mut crate::seal::Sealer, len: usize) -> Vec<u8> {
    let mut out = Vec::new();
    tx.seal(&vec![0; len], &mut out).unwrap();
    out
}

/// The restarted demux answers the lost session's next record with a
/// reset carrying the token message 2 gave the client: shorter than the
/// record, refused by the client's opener, its tail the token.
#[tokio::test]
async fn a_lost_sessions_record_gets_a_shorter_reset_with_its_token() {
    let (mut d2, a, accept, session) = restarted().await;
    let (mut tx, mut rx) = session.into_halves();
    let rec = record(&mut tx, 20);
    feed(&mut d2, addr(&a), &rec);
    let reset = recv(&a).await.expect("a reset");
    assert!(reset.len() < rec.len(), "{} < {}", reset.len(), rec.len());
    assert_eq!(rx.open(&reset), Err(Refusal::Forged));
    assert!(accept.reset_token.matches(reset_tail(&reset).unwrap()));
    let c = d2.seal.as_ref().unwrap().counts;
    assert_eq!((c.resets_sent, d2.mig.cid_unknown), (1, 1));
    assert_eq!(d2.seal_totals().udp_stateless_resets_sent, 1);
}

/// Never an amplifier: every trigger size gets a reset strictly
/// shorter, at most RESET_LEN_MAX; a datagram too short to name a CID
/// gets nothing (it is malformed).
#[tokio::test]
async fn the_reset_is_always_shorter_than_its_trigger() {
    let (mut d2, a, _accept, session) = restarted().await;
    let (mut tx, _) = session.into_halves();
    for inner in [0, 1, 8, 9, 30, 200, 1400] {
        let rec = record(&mut tx, inner);
        feed(&mut d2, addr(&a), &rec);
        let reset = recv(&a).await.expect("a reset");
        assert_eq!(
            reset.len(),
            (rec.len() - 1).min(RESET_LEN_MAX),
            "{}",
            rec.len()
        );
        assert!(reset.len() >= RESET_LEN_MIN);
    }
    let short = record(&mut tx, 0);
    feed(&mut d2, addr(&a), &short[..32]);
    assert!(recv(&a).await.is_none(), "32 bytes name no CID");
    assert_eq!(d2.seal_counts().refused[1], 1, "seal_malformed");
}

/// The budget: over it a trigger is dropped and counted; `None` (the
/// server's `udp_stateless_resets_per_sec = 0`) sends no reset at all.
#[tokio::test]
async fn resets_are_rate_limited_and_can_be_off() {
    let (mut d2, a, _accept, session) = restarted().await;
    let (mut tx, _) = session.into_halves();
    d2.seal.as_mut().unwrap().resets = Some(DhBudget::new(Some(1)));
    for _ in 0..3 {
        let rec = record(&mut tx, 4);
        feed(&mut d2, addr(&a), &rec);
    }
    assert!(recv(&a).await.is_some(), "the bucket's one token");
    assert!(recv(&a).await.is_none(), "the rest rate-limited");
    let t = d2.seal_totals();
    assert_eq!(
        (
            t.udp_stateless_resets_sent,
            t.udp_stateless_resets_rate_limited
        ),
        (1, 2)
    );
    d2.seal.as_mut().unwrap().resets = None;
    let rec = record(&mut tx, 4);
    feed(&mut d2, addr(&a), &rec);
    assert!(recv(&a).await.is_none(), "resets off");
    assert_eq!(d2.mig.cid_unknown, 4, "every trigger is still counted");
}
