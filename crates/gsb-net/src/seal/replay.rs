//! Anti-replay sliding window over the record counter (RFC 6479 shape:
//! a bitmap ring indexed by `counter % REPLAY_WINDOW`).

/// Window width in counters: a counter `top - REPLAY_WINDOW + 1` or newer
/// that was not seen yet is accepted; anything older is refused as too
/// old. 1024 is chosen over WireGuard's ~2048 because a game session sends
/// tens to a few hundred datagrams per second, so 1024 counters is several
/// seconds of reordering — anything later is stale for a game and the REL
/// band resends it anyway — while the bitmap stays 128 B per session
/// (12.8 MB at 100k sessions instead of 25.6 MB).
pub const REPLAY_WINDOW: u64 = 1024;
const WORDS: usize = (REPLAY_WINDOW / 64) as usize;

/// What the window says about a counter before authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Check {
    Fresh,
    Replayed,
    TooOld,
}

pub(super) struct ReplayWindow {
    /// Highest counter marked so far; `None` before the first.
    top: Option<u64>,
    bits: [u64; WORDS],
}

impl ReplayWindow {
    pub(super) fn new() -> Self {
        ReplayWindow {
            top: None,
            bits: [0; WORDS],
        }
    }

    pub(super) fn top(&self) -> Option<u64> {
        self.top
    }

    pub(super) fn check(&self, counter: u64) -> Check {
        let Some(top) = self.top else {
            return Check::Fresh;
        };
        if counter > top {
            Check::Fresh
        } else if top - counter >= REPLAY_WINDOW {
            Check::TooOld
        } else if self.get(counter) {
            Check::Replayed
        } else {
            Check::Fresh
        }
    }

    /// Records an AUTHENTICATED counter that [`check`](Self::check) called
    /// fresh. Marking before authentication would let a forger burn the
    /// slots of datagrams that have not arrived yet.
    pub(super) fn mark(&mut self, counter: u64) {
        if let Some(top) = self.top {
            if counter > top {
                if counter - top >= REPLAY_WINDOW {
                    self.bits = [0; WORDS];
                } else {
                    // Slots top+1..counter held counters that just fell
                    // out of the window; they must read as unseen.
                    for c in top + 1..counter {
                        self.put(c, false);
                    }
                }
                self.top = Some(counter);
            }
        } else {
            self.top = Some(counter);
        }
        self.put(counter, true);
    }

    fn get(&self, counter: u64) -> bool {
        let i = counter % REPLAY_WINDOW;
        self.bits[(i / 64) as usize] & (1 << (i % 64)) != 0
    }

    fn put(&mut self, counter: u64, seen: bool) {
        let i = counter % REPLAY_WINDOW;
        let word = &mut self.bits[(i / 64) as usize];
        if seen {
            *word |= 1 << (i % 64);
        } else {
            *word &= !(1 << (i % 64));
        }
    }
}
