//! The per-origin duplicate filter: a sliding window of sequence
//! numbers, fixed size. The anti-replay window of IPsec/DTLS in shape;
//! its size is derived in [`EFFECT_WINDOW`]'s docs.

use crate::shard::effect::EFFECT_WINDOW;

/// 64-bit words backing the window.
const WORDS: usize = EFFECT_WINDOW.div_ceil(64) as usize;

/// What the window says about one sequence number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Seen {
    /// Never seen: admitted (and now remembered).
    Fresh,
    /// Already admitted once.
    Duplicate,
    /// Older than the window — by the window's derivation such an effect
    /// is also older than the age envelope, so it can only be a replay.
    TooOld,
}

/// The sequence numbers seen from ONE origin: the highest (`hwm`) and a
/// bitmap over `(hwm - EFFECT_WINDOW, hwm]`, bit `seq % EFFECT_WINDOW`.
/// Constant size whatever the traffic — that is the whole bound.
#[derive(Debug, Clone)]
pub(crate) struct Window {
    hwm: u64,
    bits: [u64; WORDS],
}

impl Default for Window {
    fn default() -> Self {
        Self {
            hwm: 0,
            bits: [0; WORDS],
        }
    }
}

impl Window {
    fn slot(seq: u64) -> (usize, u64) {
        let i = (seq % EFFECT_WINDOW) as usize;
        (i / 64, 1 << (i % 64))
    }

    /// Check `seq` (≥ 1) against the window and remember it if fresh.
    pub(crate) fn admit(&mut self, seq: u64) -> Seen {
        if seq > self.hwm {
            // Advance: forget the slots the new top reuses — every seq
            // in (hwm, seq] (all of them once the jump spans the window).
            if seq - self.hwm >= EFFECT_WINDOW {
                self.bits = [0; WORDS];
            } else {
                for s in self.hwm + 1..=seq {
                    let (w, b) = Self::slot(s);
                    self.bits[w] &= !b;
                }
            }
            self.hwm = seq;
        } else if self.hwm - seq >= EFFECT_WINDOW {
            return Seen::TooOld;
        }
        let (w, b) = Self::slot(seq);
        if self.bits[w] & b != 0 {
            return Seen::Duplicate;
        }
        self.bits[w] |= b;
        Seen::Fresh
    }
}
