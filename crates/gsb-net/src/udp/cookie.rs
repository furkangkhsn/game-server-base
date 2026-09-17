//! The stateless handshake's cookie key (see the module docs, "The key").

use std::net::SocketAddr;

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

    /// F(nonce, peer, key) — the stateless cookie.
    pub(super) fn compute(&self, nonce: u64, peer: SocketAddr) -> u64 {
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
        sm64(&mut x);
        x
    }
}

#[cfg(test)]
mod tests;
