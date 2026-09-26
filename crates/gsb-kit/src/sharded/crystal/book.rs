//! The fight table: cross-seam contacts per unordered pair of wire ids,
//! with expiry — its size is the number of pairs in contact within the
//! last `window` ticks (capped), never the history.

use std::collections::HashMap;

/// The most pairs the table tracks at once. A contact that would open a
/// pair beyond it is not tracked (counted in `untracked`): that pair
/// simply does not crystallize — it keeps fighting through remote
/// effects, which is correct, only not moved.
pub(in crate::sharded) const FIGHT_CAP: usize = 1024;

/// One pair's streak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::sharded) struct Fight {
    /// The first contact of the current streak.
    pub(in crate::sharded) since: u64,
    /// The latest contact, either direction.
    pub(in crate::sharded) last: u64,
    /// The latest contact from the lower wire id to the higher one.
    pub(in crate::sharded) up: Option<u64>,
    /// The latest contact from the higher wire id to the lower one.
    pub(in crate::sharded) down: Option<u64>,
}

impl Fight {
    fn open(tick: u64) -> Self {
        Self {
            since: tick,
            last: tick,
            up: None,
            down: None,
        }
    }
}

/// Pair `(lo, hi)` → its streak.
#[derive(Debug, Default)]
pub(in crate::sharded) struct FightBook {
    pub(in crate::sharded) fights: HashMap<(u64, u64), Fight>,
    /// Contacts refused at the cap.
    pub(in crate::sharded) untracked: u64,
    /// The most pairs tracked at once.
    pub(in crate::sharded) peak: usize,
}

impl FightBook {
    /// Record `source` acting on `target` at `tick`. A pair silent for
    /// more than `window` ticks starts a new streak.
    pub(in crate::sharded) fn touch(&mut self, source: u64, target: u64, tick: u64, window: u64) {
        let key = (source.min(target), source.max(target));
        if !self.fights.contains_key(&key) {
            if self.fights.len() >= FIGHT_CAP {
                self.untracked += 1;
                return;
            }
            self.fights.insert(key, Fight::open(tick));
            self.peak = self.peak.max(self.fights.len());
        }
        let Some(fight) = self.fights.get_mut(&key) else {
            return;
        };
        if tick > fight.last.saturating_add(window) {
            *fight = Fight::open(tick);
        }
        fight.last = fight.last.max(tick);
        let dir = if source < target {
            &mut fight.up
        } else {
            &mut fight.down
        };
        *dir = Some(dir.map_or(tick, |t| t.max(tick)));
    }

    /// Forget every pair silent for more than `window` ticks.
    pub(in crate::sharded) fn expire(&mut self, tick: u64, window: u64) {
        if !self.fights.is_empty() {
            self.fights
                .retain(|_, f| f.last.saturating_add(window) >= tick);
        }
    }

    /// The pairs whose streak has spanned at least `after` ticks (first
    /// to latest contact) with both directions live (a contact within
    /// `window` ticks), unordered.
    pub(in crate::sharded) fn ripe(
        &self,
        tick: u64,
        after: u64,
        window: u64,
    ) -> impl Iterator<Item = (u64, u64)> + '_ {
        let live = move |t: Option<u64>| t.is_some_and(|t| t.saturating_add(window) >= tick);
        self.fights
            .iter()
            .filter(move |(_, f)| {
                f.last >= f.since.saturating_add(after) && live(f.up) && live(f.down)
            })
            .map(|(k, _)| *k)
    }
}
