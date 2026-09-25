//! What the two clients of a player are held to after every tick: the
//! same frames (their content — the framing aside), the same view, the
//! same counters — or, when the second side's codec has a send rate
//! (A10), the rate's relation (`lag`); and what a run exercised.

use std::collections::BTreeMap;

use super::lag::Lag;
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
    /// A digest of every frame's content in arrival order (FNV-1a over
    /// the parts, the framing aside): the session's content pin.
    pub(super) digest: u64,
}

impl Stats {
    /// Count one tick's frames of one client.
    pub(super) fn see(&mut self, frames: &[(bool, Parts)]) {
        for (private, p) in frames {
            self.fulls += u64::from(!p.delta);
            self.deltas += u64::from(p.delta);
            self.removed += p.removed.len() as u64;
            self.exits += p.exits.len() as u64;
            self.records += p.records.len() as u64;
            self.long_runs += u64::from(p.run_len >= 0x80);
            self.max_run = self.max_run.max(p.run_len);
            self.fold(u64::from(*private));
            self.fold(p.sequence);
            self.fold(u64::from(p.delta));
            for &id in &p.removed {
                self.fold(id);
            }
            for exit in &p.exits {
                self.fold(exit.len() as u64);
                exit.iter().for_each(|&b| self.fold(u64::from(b)));
            }
            for &(id, x, y) in &p.records {
                self.fold(id);
                self.fold(u64::from(x as u32));
                self.fold(u64::from(y as u32));
            }
            self.fold(u64::MAX); // the frame's end
        }
    }

    /// Fold one value into [`Self::digest`] (FNV-1a, byte by byte).
    fn fold(&mut self, v: u64) {
        if self.digest == 0 {
            self.digest = 0xCBF2_9CE4_8422_2325;
        }
        for b in v.to_le_bytes() {
            self.digest = (self.digest ^ u64::from(b)).wrapping_mul(0x0100_0000_01B3);
        }
    }
}

/// How a twin holds its pairs to each other.
pub(super) enum Check {
    /// [`same`]: two framings of one content.
    Same,
    /// The send rate's relation (the second side is rated).
    Lag(Lag),
}

impl Check {
    /// A pair after tick `tick`.
    pub(super) fn pair(
        &mut self,
        tick: u64,
        a: (&ClientView<Dec<false>>, &[(bool, Parts)]),
        b: (&ClientView<Dec<true>>, &[(bool, Parts)]),
    ) {
        match self {
            Self::Same => same(tick, a, b),
            Self::Lag(lag) => lag.check(tick, a, b),
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
