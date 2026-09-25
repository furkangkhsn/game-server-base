//! What the two clients of a player are held to after every tick: the
//! same frames (their content — the framing aside), the same view, the
//! same counters; and what a run exercised.

use std::collections::BTreeMap;

use super::layout::Parts;
use crate::client::{ClientView, Counters};
use crate::testing::Dec;

/// What the run exercised (both sides count alike; the run side's).
#[derive(Debug, Default)]
pub(super) struct Stats {
    pub(super) fulls: u64,
    pub(super) deltas: u64,
    pub(super) removed: u64,
    pub(super) exits: u64,
    pub(super) records: u64,
    /// Runs longer than 127 bytes (a multi-byte length).
    pub(super) long_runs: u64,
    /// The longest run.
    pub(super) max_run: usize,
}

impl Stats {
    /// Count one tick's frames of one client.
    pub(super) fn see(&mut self, frames: &[(bool, Parts)]) {
        for (_, p) in frames {
            self.fulls += u64::from(!p.delta);
            self.deltas += u64::from(p.delta);
            self.removed += p.removed.len() as u64;
            self.exits += p.exits.len() as u64;
            self.records += p.records.len() as u64;
            self.long_runs += u64::from(p.run_len >= 0x80);
            self.max_run = self.max_run.max(p.run_len);
        }
    }
}

/// The pair's two clients after tick `tick`: `a` applied the
/// `entities` room's frames `fa`, `b` the run room's `fb`.
pub(super) fn same(
    tick: u64,
    (a, fa): (&ClientView<Dec<false>>, &[(bool, Parts)]),
    (b, fb): (&ClientView<Dec<true>>, &[(bool, Parts)]),
) {
    assert_eq!(content(fa), content(fb), "tick {tick}: the same frames");
    assert_eq!(view(a), view(b), "tick {tick}: the same view");
    assert_eq!(a.counters(), b.counters(), "tick {tick}: the same counters");
}

/// Add one client's counters to `sum`.
pub(super) fn add(sum: &mut Counters, c: &Counters) {
    sum.fulls += c.fulls;
    sum.private_fulls += c.private_fulls;
    sum.deltas += c.deltas;
    sum.gap_drops += c.gap_drops;
    sum.stale += c.stale;
    sum.errors += c.errors;
}

/// The frames without their framing (the run's length).
fn content(frames: &[(bool, Parts)]) -> Vec<(bool, Parts)> {
    let strip = |(private, p): &(bool, Parts)| {
        let p = Parts {
            run_len: 0,
            ..p.clone()
        };
        (*private, p)
    };
    frames.iter().map(strip).collect()
}

fn view<const RUN: bool>(v: &ClientView<Dec<RUN>>) -> BTreeMap<u64, (i32, i32)> {
    v.iter().map(|(id, &at)| (id, at)).collect()
}
