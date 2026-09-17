//! The stateless handshake's cookie key (see the module docs, "The key").

use std::net::SocketAddr;
use std::time::Instant;

use crate::udp::*;

/// The per-process cookie key: 16 bytes (two u64 words), either the
/// operator's config (`cookie_key`) or a draw from the **OS entropy
/// source** at bind time (see the module docs, "The key").
///
/// **Why OS entropy and not the wall clock:** an off-path attacker who
/// can narrow the server's *start time* to a small window enumerates a
/// clock-derived key offline and forges proofs without ever receiving
/// the challenge. OS entropy is uniform and independent of anything an
/// attacker can observe; 64 bits of it would already be enough, and 128
/// bits is the same draw.
///
/// **No silent degradation:** if the entropy source cannot be read and
/// the operator did not supply a key, the bind fails (the server
/// refuses to start). The key is the entire basis of the
/// anti-amplification property: a predictable key does not weaken it, it
/// *inverts* it (an attacker who knows the key forges proofs for
/// spoofed addresses and allocates full sessions — channels, actor,
/// registry entry — per fake peer), and "the server started with a
/// warning" is not a posture an operator can be trusted to notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CookieKey(pub(super) u64, pub(super) u64);

impl CookieKey {
    /// Build the key from operator-supplied bytes (the composition root
    /// parses the config's 32-hex-char string into these). Pure and
    /// deterministic: the config path is a function of the operator's
    /// input alone.
    pub(super) fn from_bytes(b: [u8; 16]) -> Self {
        Self(
            u64::from_le_bytes(b[0..8].try_into().unwrap()),
            u64::from_le_bytes(b[8..16].try_into().unwrap()),
        )
    }

    /// Draw the key from the OS entropy source. The `Err` arm is a
    /// deliberate, loud choice (see the struct docs): [`Self::generate`]
    /// is called from `bind`, which maps the failure to an `io::Error`
    /// and the server refuses to start.
    pub(super) fn generate() -> Result<Self, std::io::Error> {
        let mut b = [0u8; 16];
        getrandom::fill(&mut b).map_err(|e| {
            std::io::Error::other(format!(
                "cannot read OS entropy for the rUDP cookie key: {e}"
            ))
        })?;
        Ok(Self::from_bytes(b))
    }

    /// F(nonce, peer, key, slot) — the stateless cookie.
    ///
    /// `slot` is the TIME TERM (see [`CookieClock`]): it is what makes a
    /// captured proof expire. It is a *public* counter — anyone can read
    /// a clock — so it is folded WITH the key rather than mixed in on its
    /// own; the secrecy of the whole function still rests entirely on the
    /// entropy-derived key, exactly as before.
    pub(super) fn compute(&self, nonce: u64, peer: SocketAddr, slot: u64) -> u64 {
        let (ip, port) = match peer {
            SocketAddr::V4(v4) => (v4.ip().to_bits() as u64, v4.port()),
            SocketAddr::V6(v6) => {
                let w = v6.ip().octets();
                let hi = u64::from_be_bytes(w[0..8].try_into().unwrap());
                let mut lo = u64::from_be_bytes(w[8..16].try_into().unwrap());
                sm64(&mut lo);
                (hi ^ lo, v6.port())
            }
        };
        let mut x = self.0 ^ nonce;
        sm64(&mut x);
        let mut t = ip ^ self.1;
        sm64(&mut t);
        x ^= t;
        x ^= (port as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mut s = self.1.rotate_left(17) ^ slot.wrapping_mul(0xD6E8_FEB8_6659_FD93);
        sm64(&mut s);
        x ^= s;
        sm64(&mut x);
        x
    }

    /// Verify a proof against the CURRENT slot and the previous one.
    ///
    /// The previous slot is the in-flight grace: a challenge issued at
    /// the last millisecond of slot N is answered during slot N+1, and
    /// rejecting it would fail an honest handshake for no gain. Two slots
    /// is also the exact size of the replay window this leaves open —
    /// between one and two [`COOKIE_SLOT`] periods, depending on where in
    /// its slot the proof was issued.
    ///
    /// Not a constant-time comparison, deliberately: the cookie is not a
    /// secret the server holds and compares against an attacker-supplied
    /// guess over many attempts — it is derived per (nonce, peer, slot),
    /// so there is no fixed value for a timing oracle to converge on, and
    /// v1 declares no crypto layer (module docs, "Handshake").
    pub(super) fn verify(&self, nonce: u64, peer: SocketAddr, cookie: u64, slot: u64) -> bool {
        if cookie == self.compute(nonce, peer, slot) {
            return true;
        }
        slot.checked_sub(1)
            .is_some_and(|prev| cookie == self.compute(nonce, peer, prev))
    }
}

/// The handshake's **time term**: a monotonic slot counter, derived from
/// the clock at verification time.
///
/// Keep the distinction from [`CookieKey`] in view — they answer two
/// different questions and must not be conflated:
///
/// - the **key** is the SECRET. It must be unpredictable, so it is drawn
///   from OS entropy and never from a clock (see [`CookieKey`]);
/// - the **slot** is the EXPIRY. It must be *shared* between the server's
///   two computations of `F` (issue and verify), so it is derived from
///   the clock and is deliberately public — an attacker who knows the
///   slot still cannot produce a cookie without the key.
///
/// The base is an [`Instant`] captured at bind time, so the counter is
/// monotonic and immune to wall-clock jumps (NTP steps, DST, an operator
/// setting the date). Rotation needs **no timer task and no shared
/// state**: every call recomputes the slot from the elapsed time, which
/// is the only way a single-awaited actor could have it at all.
#[derive(Debug, Clone, Copy)]
pub(super) struct CookieClock {
    base: Instant,
}

impl CookieClock {
    /// Start the counter now (bind time).
    pub(super) fn new() -> Self {
        Self::started_at(Instant::now())
    }

    /// Start the counter at an explicit base — the seam the rotation
    /// tests drive (a base in the past *is* a server that has been up
    /// that long).
    pub(super) fn started_at(base: Instant) -> Self {
        Self { base }
    }

    /// The current slot.
    pub(super) fn slot(&self) -> u64 {
        self.slot_at(Instant::now())
    }

    /// The slot `now` falls in: elapsed time since the base, floored to
    /// [`COOKIE_SLOT`]. Saturating, so an instant before the base (which
    /// cannot happen with a monotonic clock, but costs nothing to state)
    /// is slot 0.
    pub(super) fn slot_at(&self, now: Instant) -> u64 {
        let elapsed = now.saturating_duration_since(self.base).as_millis();
        (elapsed / COOKIE_SLOT.as_millis()) as u64
    }
}

#[cfg(test)]
mod tests;
