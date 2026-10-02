//! The challenge's nonce is the whole of path validation's security
//! before crypto (RFC 9000 §8.2): an off-path attacker who knows a CID
//! could spoof a victim's address as the candidate, and only an
//! unpredictable nonce stops it answering blind and steering the
//! server → client stream at the victim. Every challenge draws a fresh
//! one; a guessed or stale one moves nothing. Child of `migrate`.

use super::*;

/// Fresh nonces everywhere: a re-challenge of the same address after a
/// timeout, a superseding candidate, another session — all distinct, and
/// none is the guessable 0 (each check fails by chance with p ≈ 2^-64).
#[tokio::test]
async fn every_challenge_draws_a_fresh_nonce() {
    let mut r = rig(4).await;
    let (b, c) = (addr(&r.b), addr(&r.c));
    feed(&mut r.d, b, &raw(b"1"));
    let first = nonce_of(&got(&r.b).await.unwrap());
    r.d.path_expiry(r.key, Instant::now() + VALIDATION_TIMEOUT);
    feed(&mut r.d, b, &raw(b"2"));
    let again = nonce_of(&got(&r.b).await.unwrap());
    feed(&mut r.d, c, &raw(b"3"));
    let other = nonce_of(&got(&r.c).await.unwrap());
    let mut s = rig(4).await;
    let sb = addr(&s.b);
    feed(&mut s.d, sb, &raw(b"4"));
    let elsewhere = nonce_of(&got(&s.b).await.unwrap());
    let all = [first, again, other, elsewhere];
    for (i, x) in all.iter().enumerate() {
        assert_ne!(*x, 0, "{all:x?}");
        assert!(all[i + 1..].iter().all(|y| y != x), "{all:x?}");
    }
}

/// A response from the candidate itself, with a guessed nonce (0) or the
/// nonce of a challenge that is over (a timed-out round's), is refused —
/// counted, no move — and only the live nonce moves the session.
#[tokio::test]
async fn a_guessed_or_stale_nonce_moves_nothing() {
    let mut r = rig(4).await;
    let (a, b) = (addr(&r.a), addr(&r.b));
    feed(&mut r.d, b, &raw(b"x"));
    let stale = nonce_of(&got(&r.b).await.unwrap());
    r.d.path_expiry(r.key, Instant::now() + VALIDATION_TIMEOUT);
    feed(&mut r.d, b, &raw(b"y"));
    let live = nonce_of(&got(&r.b).await.unwrap());
    for guess in [0, stale, live ^ 1] {
        feed(&mut r.d, b, &encode_path_response(CID, guess));
    }
    assert_eq!(r.d.mig.responses_unmatched, 3);
    assert_eq!(r.d.mig.migrations, 0);
    assert_eq!(r.d.sessions.key_at(&a), Some(r.key), "still at A");
    assert!(r.outbox.try_recv().is_err(), "the writer heard nothing");
    feed(&mut r.d, b, &encode_path_response(CID, live));
    assert_eq!(r.d.sessions.key_at(&b), Some(r.key));
}
