//! Cookie-key tests: the entropy-source property the handshake rests on.


use super::*;

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
    let now_ns =
        || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock").as_nanos();
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
/// is deterministic for a fixed (nonce, peer) — an operator-supplied
/// key makes the handshake reproducible (and auditable) by
/// construction.
#[test]
fn cookie_key_from_bytes_is_deterministic() {
    let peer: SocketAddr = "127.0.0.1:1234".parse().unwrap();
    let other: SocketAddr = "127.0.0.1:1235".parse().unwrap();
    let a = CookieKey::from_bytes([1u8; 16]);
    let b = CookieKey::from_bytes([1u8; 16]);
    assert_eq!(a, b, "the config path must be a pure function of the bytes");
    assert_eq!(a.compute(42, peer), b.compute(42, peer));
    assert_ne!(a.compute(42, peer), a.compute(43, peer));
    assert_ne!(a.compute(42, peer), a.compute(42, other));
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
