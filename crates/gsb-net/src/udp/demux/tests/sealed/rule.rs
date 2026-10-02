//! The migration rule on a sealed door (RUDP-SECURITY §7): a new path is
//! considered only for a record that OPENED and is the NEWEST, and is
//! then validated with a sealed challenge. A sniffed CID is worth
//! nothing.

use super::*;

/// An on-path sniffer knows the CID (it rides every c→s header in the
/// clear) and forges a record with it from its own address: the AEAD
/// refuses it — counted — and no validation starts: no challenge goes
/// anywhere, nothing is queued for the writer.
#[tokio::test]
async fn a_sniffed_cid_in_a_forged_record_starts_no_validation() {
    let mut r = rig().await;
    let genuine = r.raw(b"seen on the wire");
    feed(&mut r.d, addr(&r.a), &genuine);
    let mut forged = genuine.clone();
    forged[9..17].copy_from_slice(&5u64.to_le_bytes()); // a fresh counter
    for b in &mut forged[17..] {
        *b ^= 0x5A;
    }
    feed(&mut r.d, addr(&r.b), &forged);
    assert_eq!(r.counts().refused[5], 1, "seal_forged");
    assert_eq!(r.d.mig.validations_started, 0);
    assert!(r.d.sessions.get(r.key).unwrap().path.is_none());
    assert_eq!(r.send_request(), None, "no challenge queued");
    assert!(recv(&r.b).await.is_none());
}

/// A genuine record captured and replayed from another address: the
/// replay window refuses it — no validation.
#[tokio::test]
async fn a_genuine_record_replayed_from_elsewhere_starts_no_validation() {
    let mut r = rig().await;
    let rec = r.raw(b"captured");
    feed(&mut r.d, addr(&r.a), &rec);
    feed(&mut r.d, addr(&r.b), &rec);
    assert_eq!(r.counts().refused[3], 1, "seal_replayed");
    assert_eq!(r.d.mig.validations_started, 0);
    assert_eq!(r.send_request(), None);
}

/// An authenticated record from a new address that is NOT the newest (a
/// lower counter than one already opened — reordered, or a held copy
/// released elsewhere): delivered, it is the client's — but it moves
/// nothing (condition 2), counted.
#[tokio::test]
async fn an_authenticated_older_record_from_elsewhere_moves_nothing() {
    let mut r = rig().await;
    let older = r.raw(b"older");
    let newer = r.raw(b"newer");
    feed(&mut r.d, addr(&r.a), &newer);
    feed(&mut r.d, addr(&r.b), &older);
    assert_eq!(r.frame().as_deref(), Some(&b"newer"[..]));
    assert_eq!(r.frame().as_deref(), Some(&b"older"[..]));
    assert_eq!(r.counts().candidates_not_newest, 1);
    assert_eq!(r.d.mig.validations_started, 0);
    assert_eq!(r.send_request(), None);
}

/// The newest authenticated record from a new address starts the
/// validation: a SEALED challenge, queued for the writer toward the new
/// address (never sent by the demux in the clear); the client's sealed
/// response from there migrates the session — writer and actor told.
#[tokio::test]
async fn the_newest_authenticated_record_validates_and_migrates() {
    let mut r = rig().await;
    let rec = r.raw(b"from b");
    feed(&mut r.d, addr(&r.b), &rec);
    assert_eq!(r.frame().as_deref(), Some(&b"from b"[..]));
    assert_eq!(r.d.mig.validations_started, 1);
    let (to, inner) = r.send_request().expect("a challenge for the writer");
    assert_eq!(to, Some(addr(&r.b)));
    assert_eq!((inner.len(), inner[0]), (9, KIND_PATH_CHALLENGE));
    assert!(recv(&r.b).await.is_none(), "no plaintext challenge");

    let mut wrong = vec![KIND_PATH_RESPONSE];
    wrong.extend_from_slice(&(u64_at(&inner, 1).unwrap() ^ 1).to_le_bytes());
    let wrong = r.seal(&wrong);
    feed(&mut r.d, addr(&r.b), &wrong);
    assert_eq!((r.d.mig.responses_unmatched, r.d.mig.migrations), (1, 0));

    let mut resp = vec![KIND_PATH_RESPONSE];
    resp.extend_from_slice(&inner[1..9]);
    let resp = r.seal(&resp);
    feed(&mut r.d, addr(&r.b), &resp);
    assert_eq!(r.d.mig.migrations, 1);
    assert_eq!(r.d.sessions.get(r.key).unwrap().addr, addr(&r.b));
    assert!(matches!(
        r.inbox.try_recv(),
        Ok(ConnIn::PeerChanged { peer }) if peer == addr(&r.b)
    ));
    // From here the ACKs follow the session (the writer's peer moved).
    let rel = r.seal(&encode_rel(1, &FrameBody::new(7, Bytes::new())));
    feed(&mut r.d, addr(&r.b), &rel);
    assert_eq!(r.send_request(), Some((None, encode_ack(2))));
}

/// With migration off on a sealed door, a record from another address is
/// not read at all (as on the plaintext door: an address change is a new
/// session): counted as from no session, nothing opened.
#[tokio::test]
async fn migration_off_reads_nothing_from_another_address() {
    let mut r = rig().await;
    r.d.migration = false;
    let rec = r.raw(b"elsewhere");
    feed(&mut r.d, addr(&r.b), &rec);
    assert_eq!((r.d.no_session, r.frame()), (1, None));
    feed(&mut r.d, addr(&r.a), &rec);
    assert_eq!(r.frame().as_deref(), Some(&b"elsewhere"[..]), "unopened");
}
