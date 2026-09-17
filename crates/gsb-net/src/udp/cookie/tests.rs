//! Cookie-key tests: the entropy-source property the handshake rests on.

use super::*;
use std::time::Duration;

/// Reference of the REMOVED v1 derivation (wall-clock nanoseconds),
/// kept only as a regression oracle: a key equal to `legacy(t)` for
/// some instant `t` is computable from the server start time — the
/// property this turn removes.
fn legacy_time_key(nanos_since_epoch: u64) -> CookieKey {
    let mut a = nanos_since_epoch;
    let mut b = a.rotate_left(13) ^ (a >> 21);
    sm64(&mut a);
    sm64(&mut b);
    CookieKey(a | 1, b | 2)
}

/// The key is no longer a function of the wall clock: capture the
/// clock, generate, capture again, and enumerate the legacy
/// derivation over the EXACT [t0, t1] window at every plausible time
/// granularity (ns, µs, ms). A time-derived key must land on an
/// enumerated value; an OS-entropy draw cannot (a collision with any
/// one candidate is 2^-128, the window holds ≤ ~10^5 of them).
#[test]
fn cookie_key_is_not_derived_from_the_wall_clock() {
    let now_ns = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    };
    let t0 = now_ns();
    let key = CookieKey::generate().expect("OS entropy in test");
    let t1 = now_ns();
    assert!(t1 >= t0, "the window must not be empty");
    for unit in [1u128, 1_000, 1_000_000] {
        for t in (t0 / unit)..=(t1 / unit) {
            assert_ne!(
                key,
                legacy_time_key(t as u64),
                "key equals the legacy wall-clock derivation at t={t}"
            );
        }
    }
}

/// The config path is pure: the same bytes give the same key, and F
/// is deterministic for a fixed (nonce, peer, slot) — an
/// operator-supplied key makes the handshake reproducible (and
/// auditable) by construction.
#[test]
fn cookie_key_from_bytes_is_deterministic() {
    let peer: SocketAddr = "127.0.0.1:1234".parse().unwrap();
    let other: SocketAddr = "127.0.0.1:1235".parse().unwrap();
    let a = CookieKey::from_bytes([1u8; 16]);
    let b = CookieKey::from_bytes([1u8; 16]);
    assert_eq!(a, b, "the config path must be a pure function of the bytes");
    assert_eq!(a.compute(42, peer, 7), b.compute(42, peer, 7));
    assert_ne!(a.compute(42, peer, 7), a.compute(43, peer, 7));
    assert_ne!(a.compute(42, peer, 7), a.compute(42, other, 7));
}

/// The OS-entropy source is non-degenerate: two consecutive draws
/// differ (a collision is 2^-128 — a match means the source is
/// constant or broken, not bad luck).
#[test]
fn cookie_key_generate_draws_distinct_keys() {
    let a = CookieKey::generate().expect("OS entropy in test");
    let b = CookieKey::generate().expect("OS entropy in test");
    assert_ne!(a, b, "two consecutive OS-entropy draws collided");
}

// ---------------------------------------------------------------------
// The TIME TERM. The two tests above pin the KEY (entropy-derived, never
// clock-derived); the ones below pin the SLOT (clock-derived on purpose).
// The distinction is the whole design: the key is the secret, the slot is
// the expiry — see `CookieClock`.
// ---------------------------------------------------------------------

/// The slot IS a term of F: the same (key, nonce, peer) in a different
/// slot is a different cookie. Without this the cookie would have no
/// expiry at all — which was the bug.
#[test]
fn cookie_slot_is_a_term_of_the_cookie() {
    let peer: SocketAddr = "127.0.0.1:1234".parse().unwrap();
    let key = CookieKey::from_bytes([9u8; 16]);
    let base = key.compute(42, peer, 100);
    assert_ne!(
        base,
        key.compute(42, peer, 101),
        "the next slot must differ"
    );
    assert_ne!(
        base,
        key.compute(42, peer, 99),
        "the previous slot must differ"
    );
    assert_eq!(base, key.compute(42, peer, 100), "same slot, same cookie");
}

/// A proof for a slot older than the previous one is REJECTED: a cookie
/// captured off the wire stops working once two rotations have passed.
/// This is the property the fix exists for.
#[test]
fn cookie_proof_from_an_expired_slot_is_rejected() {
    let peer: SocketAddr = "127.0.0.1:1234".parse().unwrap();
    let key = CookieKey::generate().expect("OS entropy in test");
    let captured = key.compute(0xABCD, peer, 100);
    assert!(
        !key.verify(0xABCD, peer, captured, 102),
        "a proof two slots old must not verify"
    );
    assert!(
        !key.verify(0xABCD, peer, captured, 1_000),
        "a long-ago proof must not verify"
    );
}

/// A handshake in flight ACROSS a rotation boundary still succeeds: the
/// challenge was minted in slot N, the proof arrives in slot N+1, and
/// verification accepts the previous slot for exactly this reason.
#[test]
fn cookie_proof_across_one_rotation_boundary_is_accepted() {
    let peer: SocketAddr = "127.0.0.1:1234".parse().unwrap();
    let key = CookieKey::generate().expect("OS entropy in test");
    let issued = key.compute(0xABCD, peer, 100);
    assert!(
        key.verify(0xABCD, peer, issued, 100),
        "the issuing slot must verify"
    );
    assert!(
        key.verify(0xABCD, peer, issued, 101),
        "one rotation of grace must verify"
    );
    // Slot 0 has no previous slot: the grace must not wrap around.
    let first = key.compute(0xABCD, peer, 0);
    assert!(key.verify(0xABCD, peer, first, 0));
    assert!(!key.verify(0xABCD, peer, u64::MAX, 0));
}

/// The address binding survives the time term: a proof minted for one
/// peer never verifies for another, in any slot (the anti-spoofing
/// property the handshake exists for).
#[test]
fn cookie_proof_for_a_different_peer_is_still_rejected() {
    let mine: SocketAddr = "127.0.0.1:1234".parse().unwrap();
    let theirs: SocketAddr = "127.0.0.1:1235".parse().unwrap();
    let v6: SocketAddr = "[::1]:1234".parse().unwrap();
    let key = CookieKey::generate().expect("OS entropy in test");
    let mine_proof = key.compute(7, mine, 100);
    for slot in [99u64, 100, 101] {
        assert!(
            !key.verify(7, theirs, mine_proof, slot),
            "another port must not accept my proof (slot {slot})"
        );
        assert!(
            !key.verify(7, v6, mine_proof, slot),
            "another address family must not accept my proof (slot {slot})"
        );
    }
}

/// The clock advances exactly one slot per [`COOKIE_SLOT`], from a base
/// captured at bind time — no timer, no shared state, just arithmetic on
/// a monotonic instant.
#[test]
fn cookie_clock_advances_one_slot_per_period() {
    let base = Instant::now();
    let clock = CookieClock::started_at(base);
    assert_eq!(clock.slot_at(base), 0);
    assert_eq!(
        clock.slot_at(base + COOKIE_SLOT - Duration::from_millis(1)),
        0
    );
    assert_eq!(clock.slot_at(base + COOKIE_SLOT), 1);
    assert_eq!(clock.slot_at(base + COOKIE_SLOT * 7), 7);
    // A clock started in the past is a server that has been up that long.
    let old = CookieClock::started_at(base - COOKIE_SLOT * 3);
    assert_eq!(old.slot_at(base), 3);
}
