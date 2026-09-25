//! A seeded random session, replayed on both rooms of the twin: each
//! tick's operations are drawn as data — at most one join (a random
//! spot on the `[-95, 95]²` map, one of three teams), a leave now and
//! then, walks and teleports, runtime team changes.

/// SplitMix64: a fixed seed gives the same run everywhere.
pub(super) struct Rng(u64);

impl Rng {
    pub(super) fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// `true` with probability `p`.
    fn chance(&mut self, p: f64) -> bool {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64 <= p
    }

    /// Uniform in `0..n` (`n > 0`).
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// Uniform in `[-half, half)`.
    fn coord(&mut self, half: f32) -> f32 {
        (self.next() % 10_000) as f32 / 10_000.0 * 2.0 * half - half
    }
}

/// One operation; players are named by their index in the live list.
#[derive(Debug, Clone)]
pub(super) enum Op {
    /// A join as `x:y:team`.
    Join(String),
    Leave(usize),
    Move(usize, f32, f32),
    Switch(usize, u8),
}

/// The draw, and where every live player stands.
pub(super) struct Script {
    rng: Rng,
    players: Vec<(f32, f32)>,
    /// The most players alive at once.
    cap: usize,
}

impl Script {
    pub(super) fn new(seed: u64, cap: usize) -> Self {
        Self {
            rng: Rng::new(seed),
            players: Vec::new(),
            cap,
        }
    }

    /// This tick's operations.
    pub(super) fn tick(&mut self) -> Vec<Op> {
        let r = &mut self.rng;
        let mut ops = Vec::new();
        if self.players.len() < self.cap && r.chance(0.3) {
            let at = (r.coord(95.0), r.coord(95.0));
            let team = r.below(3);
            ops.push(Op::Join(format!("{}:{}:{team}", at.0, at.1)));
            self.players.push(at);
        } else if self.players.len() > 4 && r.chance(0.04) {
            let i = r.below(self.players.len());
            self.players.remove(i);
            ops.push(Op::Leave(i));
        }
        for (i, at) in self.players.iter_mut().enumerate() {
            if r.chance(0.02) {
                *at = (r.coord(95.0), r.coord(95.0)); // a teleport
            } else if r.chance(0.5) {
                at.0 = (at.0 + r.coord(3.0)).clamp(-95.0, 95.0);
                at.1 = (at.1 + r.coord(3.0)).clamp(-95.0, 95.0);
            } else {
                continue; // standing still this tick
            }
            ops.push(Op::Move(i, at.0, at.1));
        }
        if !self.players.is_empty() && r.chance(0.03) {
            ops.push(Op::Switch(r.below(self.players.len()), r.below(3) as u8));
        }
        ops
    }
}
