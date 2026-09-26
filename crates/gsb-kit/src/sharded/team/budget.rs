//! The per-team export budget (`docs/CROSS-SHARD.md` §8b.1, A29): which
//! records of one team's visible set an over-budget export keeps.
//!
//! **Members first, always.** A team's members (its units on this shard)
//! are kept before any record they see; the budget reaches what they see
//! only once every member is in. The game's rank
//! ([`crate::sharded::ShardedTeamRoom::with_export_rank`]) REFINES within each of the two
//! tiers — it does not replace the tiers: membership is known only here
//! (an own entity's [`crate::team::TeamMember`]; a lent record has no
//! team), and the export's promise — a team's own units map-wide before
//! the enemies they spot — does not depend on the game's rank.
//!
//! **Within a tier: the higher rank first, ties by the smaller wire id** —
//! a total order (a wire is listed once per set), so the kept set is the
//! same whatever order the kit met the records in (own by wire id, then
//! the border strip). The kept records go out in the export's own order.
//! Without a rank, the prefix: exactly the cut before A29.
//!
//! **Cost.** Nothing unless the set is over budget (one comparison per
//! team per tick). Over it, with a rank: one rank call per record, one
//! linear-time selection of the cut tier's last kept key
//! (`select_nth_unstable_by`: O(n) in the worst case) and one comparison
//! per record — O(n) per team, no sort, no allocation once the two
//! scratch buffers have grown to the largest set.

use std::cmp::Ordering;
use std::ops::Range;

/// A record's place in a ranked cut: `(rank, wire id)`.
type Key = (u32, u64);

/// The ranked order, best first: the higher rank, then the smaller wire
/// id.
fn best_first(a: &Key, b: &Key) -> Ordering {
    b.0.cmp(&a.0).then(a.1.cmp(&b.1))
}

/// What one tier of a ranked cut keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::sharded) enum Tier {
    All,
    Nothing,
    /// The records at or before this key (the last one kept).
    UpTo(Key),
}

impl Tier {
    fn keeps(self, key: &Key) -> bool {
        match self {
            Self::All => true,
            Self::Nothing => false,
            Self::UpTo(last) => best_first(key, &last) != Ordering::Greater,
        }
    }
}

/// What a cut keeps of one team's visible set (read with
/// [`Budget::keeps`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::sharded) enum Kept {
    /// The first `n` records.
    Prefix(usize),
    /// Ranked: the members tier (the set's first `members` records),
    /// then the rest.
    Ranked { members: usize, tiers: [Tier; 2] },
}

/// The export budget of the team composite: the records per team per
/// tick, the game's rank, and the ranked cut's scratch.
pub(in crate::sharded) struct Budget<W> {
    /// Records per team per tick this shard exports at most.
    pub(in crate::sharded) records: usize,
    /// The game's rank of a record by its wire value (higher is kept
    /// first); `None`: the prefix.
    pub(in crate::sharded) rank: Option<fn(&W) -> u32>,
    /// The ranked set's keys, in the set's order.
    keys: Vec<Key>,
    /// The cut tier's keys, reordered by the selection.
    pick: Vec<Key>,
}

impl<W> Budget<W> {
    pub(in crate::sharded) fn new(records: usize) -> Self {
        Self {
            records,
            rank: None,
            keys: Vec::new(),
            pick: Vec::new(),
        }
    }

    /// Cut one team's visible set `set` — `(wire id, wire value)` in the
    /// export's order, its first `members` the team's members.
    pub(in crate::sharded) fn cut<'a>(
        &mut self,
        set: impl ExactSizeIterator<Item = (u64, &'a W)>,
        members: usize,
    ) -> Kept
    where
        W: 'a,
    {
        let n = set.len();
        let Some(rank) = self.rank.filter(|_| n > self.records) else {
            return Kept::Prefix(n.min(self.records));
        };
        self.keys.clear();
        self.keys
            .extend(set.map(|(wire, value)| (rank(value), wire)));
        let rest = self.records.saturating_sub(members);
        let tiers = [
            self.tier(0..members, self.records),
            self.tier(members..n, rest),
        ];
        Kept::Ranked { members, tiers }
    }

    /// What the tier `range` of the keys keeps with room for `room`.
    fn tier(&mut self, range: Range<usize>, room: usize) -> Tier {
        if range.len() <= room {
            return Tier::All;
        }
        if room == 0 {
            return Tier::Nothing;
        }
        self.pick.clear();
        self.pick.extend_from_slice(&self.keys[range]);
        let (_, last, _) = self.pick.select_nth_unstable_by(room - 1, best_first);
        Tier::UpTo(*last)
    }

    /// Whether the cut `kept` keeps the `i`-th record of its set.
    pub(in crate::sharded) fn keeps(&self, kept: Kept, i: usize) -> bool {
        match kept {
            Kept::Prefix(n) => i < n,
            Kept::Ranked { members, tiers } => {
                tiers[usize::from(i >= members)].keeps(&self.keys[i])
            }
        }
    }
}

#[cfg(test)]
mod tests;
