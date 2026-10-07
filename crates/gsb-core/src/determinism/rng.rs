//! The room's seeded generator (SplitMix64, `docs/SERVICES.md` §10.1):
//! the only randomness a game's logic may draw. Its seed is the room's
//! (recorded in the input log), so a replay draws the same values.

use std::cell::Cell;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::calls::mix;

/// SplitMix64's increment (the golden ratio's 64-bit fraction).
const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;

/// A room's generator state. The actor owns one and lends it to every
/// hook through [`TickCtx::rng`](crate::room::TickCtx::rng) (a `Cell`, so
/// the `&TickCtx` hooks can draw). A test that builds its own context
/// owns one too ([`Self::view`]).
#[derive(Debug, Default)]
pub struct RoomRng {
    state: Cell<u64>,
}

impl RoomRng {
    /// A generator seeded with `seed`.
    pub fn new(seed: u64) -> Self {
        Self {
            state: Cell::new(seed),
        }
    }

    /// The handle a tick context carries.
    pub fn view(&self) -> Rng<'_> {
        Rng {
            state: Some(&self.state),
        }
    }

    /// The current state (the engine folds it into the state hash: two
    /// runs that drew differently differ here).
    pub fn state(&self) -> u64 {
        self.state.get()
    }
}

thread_local! {
    /// The stream an INERT handle draws from (a hand-built context):
    /// deterministic per thread, never recorded.
    static INERT: Cell<u64> = const { Cell::new(0) };
}

/// The room's generator as the tick context lends it. `Default` is the
/// INERT handle a hand-built context carries: it draws from a
/// thread-local stream seeded with 0 (deterministic per thread — a test
/// runs on its own — but no room's, and never recorded).
#[derive(Debug, Clone, Copy, Default)]
pub struct Rng<'a> {
    state: Option<&'a Cell<u64>>,
}

impl Rng<'_> {
    /// The next 64 random bits.
    pub fn next_u64(&self) -> u64 {
        let step = |c: &Cell<u64>| {
            let s = c.get().wrapping_add(GOLDEN);
            c.set(s);
            mix(s)
        };
        match self.state {
            Some(c) => step(c),
            None => INERT.with(step),
        }
    }

    /// Uniform in `0..n` (`0` when `n == 0`): the high half of a 128-bit
    /// product (Lemire), no modulo bias worth the name.
    pub fn below(&self, n: u64) -> u64 {
        ((u128::from(self.next_u64()) * u128::from(n)) >> 64) as u64
    }

    /// Uniform in `[0, 1)`, 53 bits.
    pub fn unit(&self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// `true` with probability `p` (clamped to `[0, 1]`).
    pub fn chance(&self, p: f64) -> bool {
        self.unit() < p
    }
}

/// A fresh seed for a room that was given none (live mode): unique per
/// incarnation — the process's random hasher keys (std, OS-seeded), a
/// process-wide counter and `salt` (the room id). Recorded with the
/// room's log, so a replay never needs it again.
pub fn fresh_seed(salt: u64) -> u64 {
    static N: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(salt);
    h.write_u64(N.fetch_add(1, Ordering::Relaxed));
    mix(h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_seed_one_stream_and_a_view_advances_the_room_state() {
        let a = RoomRng::new(7);
        let b = RoomRng::new(7);
        let xs: Vec<u64> = (0..4).map(|_| a.view().next_u64()).collect();
        let ys: Vec<u64> = (0..4).map(|_| b.view().next_u64()).collect();
        assert_eq!(xs, ys);
        assert_ne!(xs[0], xs[1]);
        assert_eq!(a.state(), 7u64.wrapping_add(GOLDEN.wrapping_mul(4)));
        let c = RoomRng::new(8);
        assert_ne!(c.view().next_u64(), xs[0], "another seed, another stream");
    }

    #[test]
    fn below_stays_in_range_and_fresh_seeds_differ() {
        let r = RoomRng::new(1);
        for _ in 0..1000 {
            assert!(r.view().below(6) < 6);
            let u = r.view().unit();
            assert!((0.0..1.0).contains(&u));
        }
        assert_eq!(r.view().below(0), 0);
        assert_ne!(fresh_seed(1), fresh_seed(1));
    }
}
