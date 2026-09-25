//! What a pair's two clients are held to when the second side's codec
//! has a SEND RATE (A10) and the first side's does not: after every tick
//!
//! - the same records in both views (entering and leaving a view never
//!   wait), and the same `removed` ids and `cell_exits` in the tick's
//!   frames;
//! - every record's value on the rated side is a value the default side
//!   showed (to any player — a fresh pair has no past of its own), or
//!   the record had (a stepped twin also feeds its worlds' truth), at
//!   most `bound` ticks ago (the staleness bound: the largest class's
//!   period, less one — plus the team exchange's one-tick relay for an
//!   imported record);
//! - a stepped twin (its step is its tick) with every record typed
//!   (`on_due`) also holds the rated side's deltas to the schedule: a
//!   change to a record the client held, inside its cell, is upserted
//!   only on a due step of its class — never early;
//! - after a tick in which the rated side applied a FULL (a fresh
//!   group's, the keep-alive, a one-shot), the two views are equal — a
//!   full carries every current value (`exact_on_full`; not for the
//!   sharded team room, whose imports are the owner's paced bodies).

use std::collections::{BTreeMap, HashMap};

use super::layout::Parts;
use crate::client::{ClientDecoder, ClientView};
use crate::testing::{Dec, WirePos, band_class};

type View = BTreeMap<u64, (i32, i32)>;

/// The relation's state: what the default side showed lately.
pub(super) struct Lag {
    bound: u64,
    exact_on_full: bool,
    /// The last tick each `(record, value)` was in a default-side view
    /// (kept for `bound` ticks).
    shown: HashMap<(u64, (i32, i32)), u64>,
    /// The tick `shown` was last pruned on.
    pruned: u64,
    /// The largest staleness seen (ticks).
    pub(super) max_seen: u64,
    /// Record-ticks the rated side showed an older value.
    pub(super) lagging: u64,
    /// Ticks a rated full was checked equal to the default view.
    pub(super) exact_fulls: u64,
    /// Whether [`Self::on_time`] checks the schedule.
    on_due: bool,
    /// Upserts [`Self::on_time`] found on their due step.
    pub(super) on_time: u64,
}

impl Lag {
    pub(super) fn new(bound: u64, exact_on_full: bool) -> Self {
        Self {
            bound,
            exact_on_full,
            shown: HashMap::new(),
            pruned: 0,
            max_seen: 0,
            lagging: 0,
            exact_fulls: 0,
            on_due: false,
            on_time: 0,
        }
    }

    /// Also hold the rated deltas to the schedule ([`Self::on_time`]).
    pub(super) fn on_due(mut self) -> Self {
        self.on_due = true;
        self
    }

    /// The rated side's frames of tick `tick` (the room's step), given
    /// the view the client held before them and the records that
    /// migrated between shards this step: an upsert CHANGING a record it
    /// held, still in the same cell, neither removed in the frame nor
    /// migrating, is due on this step in the class of the value it
    /// carries.
    pub(super) fn on_time(
        &mut self,
        tick: u64,
        before: &View,
        frames: &[(bool, Parts)],
        migrated: &[u64],
    ) {
        if !self.on_due {
            return;
        }
        let grid = Dec::<true>::new(super::CELL);
        let cell = |&(x, y): &(i32, i32)| grid.cell_of(&(x, y));
        for (_, p) in frames.iter().filter(|(private, p)| !private && p.delta) {
            for &(id, x, y) in &p.records {
                let Some(old) = before.get(&id) else { continue };
                let moved = cell(old) != cell(&(x, y));
                if *old == (x, y) || moved || p.removed.contains(&id) || migrated.contains(&id) {
                    // Not a change, or a crossing, an exit and re-entry,
                    // a migration (its arrival appears on the receiving
                    // shard — an entry — replacing the lent copy).
                    continue;
                }
                let class = band_class(&WirePos { x, y });
                assert!(
                    class.due(tick, id),
                    "tick {tick}: {id} upserted early ({class:?})"
                );
                self.on_time += 1;
            }
        }
    }

    /// Every record's true value on tick `tick` (a stepped twin reads
    /// its default room's worlds: a record no player sees still has a
    /// past — an import's body may come from it).
    pub(super) fn observe(&mut self, tick: u64, truth: impl Iterator<Item = (u64, (i32, i32))>) {
        for (id, v) in truth {
            self.shown.insert((id, v), tick);
        }
    }

    /// A pair after tick `tick`: `a` the default side, `b` the rated.
    pub(super) fn check(
        &mut self,
        tick: u64,
        (a, fa): (&ClientView<Dec<false>>, &[(bool, Parts)]),
        (b, fb): (&ClientView<Dec<true>>, &[(bool, Parts)]),
    ) {
        let (va, vb) = (view(a), view(b));
        if self.pruned != tick {
            let bound = self.bound;
            self.shown.retain(|_, at| tick - *at <= bound);
            self.pruned = tick;
        }
        for (&id, &v) in &va {
            self.shown.insert((id, v), tick);
        }
        assert!(
            va.keys().eq(vb.keys()),
            "tick {tick}: the same records\n{va:?}\n{vb:?}"
        );
        assert_eq!(gone(fa), gone(fb), "tick {tick}: the same exits");
        for (&id, &v) in &vb {
            let Some(at) = self.shown.get(&(id, v)) else {
                panic!(
                    "tick {tick}: record {id} at {v:?} is older than {} ticks",
                    self.bound
                );
            };
            let k = tick - at;
            self.max_seen = self.max_seen.max(k);
            self.lagging += u64::from(k > 0);
        }
        if self.exact_on_full && fb.iter().any(|(_, p)| !p.delta) {
            assert_eq!(va, vb, "tick {tick}: a full carries the current values");
            self.exact_fulls += 1;
        }
        let (ca, cb) = (a.counters(), b.counters());
        assert_eq!((cb.errors, cb.stale), (0, 0), "tick {tick}");
        assert_eq!(
            (ca.fulls, ca.private_fulls),
            (cb.fulls, cb.private_fulls),
            "tick {tick}: the fulls land alike"
        );
    }
}

fn view<const RUN: bool>(v: &ClientView<Dec<RUN>>) -> View {
    v.iter().map(|(id, &at)| (id, at)).collect()
}

/// The tick's `removed` ids and cell exits, over all its frames.
fn gone(frames: &[(bool, Parts)]) -> (Vec<u64>, Vec<Vec<u8>>) {
    let mut removed: Vec<u64> = frames.iter().flat_map(|(_, p)| p.removed.clone()).collect();
    let mut exits: Vec<Vec<u8>> = frames.iter().flat_map(|(_, p)| p.exits.clone()).collect();
    removed.sort_unstable();
    exits.sort_unstable();
    (removed, exits)
}
