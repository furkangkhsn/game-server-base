//! A sealed session's record path: every refused datagram dropped and
//! counted under its own name, nothing of it reaching the actor; the
//! integrity limit ends the session.

use super::*;
use crate::seal::INTEGRITY_LIMIT;

/// Replayed, tampered (each region: kind, CID kept, counter, ciphertext,
/// tag), truncated, unknown-CID and unsealed datagrams: each counted
/// under its name, none delivered; the genuine record before them was.
#[tokio::test]
async fn forged_replayed_and_tampered_records_are_counted_and_dropped() {
    let mut r = rig().await;
    let at = addr(&r.a);
    let rec = r.raw(b"one");
    feed(&mut r.d, at, &rec);
    assert_eq!(r.frame().as_deref(), Some(&b"one"[..]));

    feed(&mut r.d, at, &rec); // the same record again
    let next = r.raw(b"two");
    let mut flipped = next.clone();
    flipped[20] ^= 0x01; // ciphertext
    feed(&mut r.d, at, &flipped);
    let mut tag = next.clone();
    *tag.last_mut().unwrap() ^= 0x80; // tag
    feed(&mut r.d, at, &tag);
    let mut counter = next.clone();
    counter[9] ^= 0x02; // the counter (the AAD): authentication fails
    feed(&mut r.d, at, &counter);
    let mut phase = next.clone();
    phase[0] |= crate::seal::wire::KIND_PHASE_BIT; // the phase bit
    feed(&mut r.d, at, &phase);
    feed(&mut r.d, at, &next[..20]); // shorter than header + tag
    let mut cid = next.clone();
    cid[1] ^= 0xFF; // another CID: no session
    feed(&mut r.d, at, &cid);
    // Plaintext at a sealed door: a RAW, a CID-tagged RAW.
    let plain = encode_raw(&FrameBody::new(1000, Bytes::from_static(b"x")));
    feed(&mut r.d, at, &plain);
    feed(&mut r.d, at, &tag_cid(r.cid, &plain));

    assert_eq!(r.frame(), None, "nothing refused reaches the actor");
    let c = r.counts();
    // IntegrityLimit, Malformed, TooOld, Replayed, WrongPhase, Forged.
    assert_eq!(c.refused, [0, 1, 0, 1, 0, 4], "{c:?}");
    assert_eq!(c.datagrams_unsealed, 2);
    assert_eq!(r.d.mig.cid_unknown, 1);
    // The genuine record that was tampered with still opens: the window
    // only moves on an authenticated datagram.
    feed(&mut r.d, at, &next);
    assert_eq!(r.frame().as_deref(), Some(&b"two"[..]));
    let totals = r.d.seal_totals();
    assert_eq!(
        (
            totals.seal_replayed,
            totals.seal_forged,
            totals.seal_malformed
        ),
        (1, 4, 1)
    );
}

/// `tag` without the import clash.
fn tag_cid(cid: u64, d: &[u8]) -> Vec<u8> {
    crate::udp::wire::tag(cid, d)
}

/// A record far below the window is `seal_too_old`, never opened.
#[tokio::test]
async fn a_record_older_than_the_window_is_too_old() {
    let mut r = rig().await;
    let at = addr(&r.a);
    let old = r.raw(b"old");
    for _ in 0..crate::seal::REPLAY_WINDOW + 1 {
        r.raw(b"skipped");
    }
    let newer = r.raw(b"new");
    feed(&mut r.d, at, &newer);
    feed(&mut r.d, at, &old);
    assert_eq!(r.frame().as_deref(), Some(&b"new"[..]));
    assert_eq!(r.frame(), None);
    assert_eq!(r.counts().refused[2], 1);
}

/// Past the integrity limit nothing opens, and the session ends: its
/// actor is told (the stream rejected), it is removed, counted.
#[tokio::test]
async fn the_integrity_limit_ends_the_session() {
    let mut r = rig().await;
    let at = addr(&r.a);
    let s = r.d.sessions.get_mut(r.key).unwrap();
    s.seal
        .as_mut()
        .unwrap()
        .opener
        .set_forged_for_test(INTEGRITY_LIMIT + 1);
    let rec = r.raw(b"late");
    feed(&mut r.d, at, &rec);
    let c = r.counts();
    assert_eq!((c.refused[0], c.sessions_ended_limit), (1, 1));
    assert!(r.d.sessions.get(r.key).is_none(), "the session is removed");
    match r.inbox.try_recv() {
        Ok(ConnIn::ServerClosed { cause, .. }) => {
            assert_eq!(cause, gsb_core::conn::ServerClose::StreamRejected)
        }
        other => panic!("expected the close, got {other:?}"),
    }
    let after = r.raw(b"after");
    feed(&mut r.d, at, &after);
    assert_eq!(r.d.mig.cid_unknown, 1, "its CID routes nowhere now");
}
