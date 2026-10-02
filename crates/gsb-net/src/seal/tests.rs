//! Tests of the seal core. Helpers shared by the child modules live here.

use super::wire::{Direction, Header};
use super::*;

mod cost;
mod handshake;
mod model;
mod record;
mod rekey;
mod replay;
mod reset;
mod vectors;

/// A completed handshake: (client sealer, client opener, server sealer,
/// server opener), plus the accept the client got.
pub(super) fn pair(cid: u64) -> (Sealer, Opener, Sealer, Opener, Accept) {
    let server = StaticKey::generate().unwrap();
    let accept = Accept {
        cid,
        reset_token: ResetKey::from_bytes([7; 32]).token(cid),
    };
    let mut init = Initiator::new(&server.public(), b"ctx", b"").unwrap();
    let resp = Msg1::parse(init.msg1())
        .unwrap()
        .cookie_verified(&server, b"ctx", &accept)
        .unwrap();
    let Ok((got, client)) = init.finish(&resp.msg2) else {
        panic!("msg2 must authenticate")
    };
    let (cs, co) = client.into_halves();
    let (ss, so) = resp.session.into_halves();
    (cs, co, ss, so, got)
}

/// Seals `n` empty-ish datagrams, returning them in order.
pub(super) fn seal_n(s: &mut Sealer, n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| {
            let mut d = Vec::new();
            s.seal(&(i as u32).to_le_bytes(), &mut d).unwrap();
            d
        })
        .collect()
}

/// Hex string to bytes (test vectors).
pub(super) fn hex(s: &str) -> Vec<u8> {
    let s: String = s.split_whitespace().collect();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// SplitMix64: a tiny seeded PRNG so randomized tests replay exactly.
pub(super) struct Rng(pub(super) u64);

impl Rng {
    pub(super) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform-enough in `0..n` for tests.
    pub(super) fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}
