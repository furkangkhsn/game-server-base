//! The per-tick state hash (`docs/SERVICES.md` §10.1): what a game folds
//! its simulation state into ([`GameLogic::state_hash`](crate::room::GameLogic::state_hash)),
//! and how the engine adds the room's own (the generator's state and the
//! room clock).

use std::time::Duration;

use crate::calls::mix;

/// FNV-1a 64: stable across runs and platforms, simple, no keys (unlike
/// std's `DefaultHasher`, whose output is not promised to be stable). A
/// [`std::hash::Hasher`], so `#[derive(Hash)]` values feed it directly
/// (`value.hash(&mut h)`).
#[derive(Debug, Clone, Copy)]
pub struct StateHasher(u64);

impl Default for StateHasher {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl StateHasher {
    /// A fresh hasher.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold a string (length-prefixed: `"ab","c"` ≠ `"a","bc"`).
    pub fn str(&mut self, s: &str) {
        std::hash::Hasher::write_u64(self, s.len() as u64);
        std::hash::Hasher::write(self, s.as_bytes());
    }
}

impl std::hash::Hasher for StateHasher {
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

/// The tick's recorded hash: the game's, folded with the room's
/// generator state and clock (so two runs that drew differently, or whose
/// clocks differ, differ even when the game hashes the same state).
/// `None` when the game hashes nothing.
pub(crate) fn fold(game: Option<u64>, rng: u64, clock: Duration) -> Option<u64> {
    game.map(|g| mix(g ^ mix(rng ^ mix(clock.as_nanos() as u64))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::{Hash, Hasher};

    #[test]
    fn the_hash_is_fnv1a_and_the_fold_sees_every_part() {
        let mut h = StateHasher::new();
        h.write(b"a");
        assert_eq!(h.finish(), 0xaf63_dc4c_8601_ec8c, "FNV-1a 64 of \"a\"");
        let mut a = StateHasher::new();
        (1u32, "x").hash(&mut a);
        let mut b = StateHasher::new();
        (1u32, "x").hash(&mut b);
        assert_eq!(a.finish(), b.finish());
        let base = fold(Some(1), 2, Duration::from_millis(3));
        assert_ne!(base, fold(Some(9), 2, Duration::from_millis(3)));
        assert_ne!(base, fold(Some(1), 9, Duration::from_millis(3)));
        assert_ne!(base, fold(Some(1), 2, Duration::from_millis(9)));
        assert_eq!(fold(None, 2, Duration::ZERO), None);
    }
}
