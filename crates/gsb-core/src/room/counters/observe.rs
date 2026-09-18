//! Folding one step's two measured durations into the counters.
//!
//! The room actor and the shard actor measure the same pair per step —
//! tick latency (`step start − ticker timestamp`) and step body duration
//! — and keep the same extremes from them. That accounting used to be
//! written out twice, once in each actor's `lifecycle.rs`, with the
//! second copy carrying a comment saying it mirrored the first; the two
//! copies then shared a defect (see [`RoomCounters::observe_late_us`])
//! and had to be fixed twice. It lives here once instead, so a `min` is
//! a `min` on both actors by construction.

use super::RoomCounters;
use crate::metrics::{fine_hist_index, hist_index};

impl RoomCounters {
    /// Fold this step's tick latency (µs) into the `late_*` extremes and
    /// sum.
    ///
    /// `steps` is the number of the step being observed — 1 for the
    /// first. Both extremes are SEEDED from that first observation
    /// rather than moved from their zero-initialised values, because a
    /// minimum that starts at 0 can never be lowered and would report 0
    /// for the process's whole life. Every later observation then moves
    /// both ends.
    ///
    /// The `min` arm is the one this module exists for. Before it,
    /// `late_min_us` and `step_min_us` were assigned under the seeding
    /// branch and nowhere else, so each held the FIRST step's duration
    /// forever — and the first step is the coldest, so the field
    /// reported the wrong END of the distribution under a name that sits
    /// in the same log line and the same Prometheus family as a `max`
    /// that really is a maximum.
    pub(crate) fn observe_late_us(&mut self, steps: u64, late_us: u64) {
        if steps <= 1 {
            self.late_min_us = late_us;
            self.late_max_us = late_us;
        } else {
            self.late_min_us = self.late_min_us.min(late_us);
            self.late_max_us = self.late_max_us.max(late_us);
        }
        self.late_sum_us = self.late_sum_us.saturating_add(late_us);
    }

    /// Fold this step's body duration (µs) into the `step_*` extremes,
    /// sum and both histograms. Seeding and `min`/`max` rules exactly as
    /// in [`Self::observe_late_us`].
    pub(crate) fn observe_step_us(&mut self, steps: u64, budget_us: u64, step_us: u64) {
        if steps <= 1 {
            self.step_min_us = step_us;
            self.step_max_us = step_us;
        } else {
            self.step_min_us = self.step_min_us.min(step_us);
            self.step_max_us = self.step_max_us.max(step_us);
        }
        self.step_sum_us = self.step_sum_us.saturating_add(step_us);
        self.step_hist[hist_index(budget_us, step_us)] += 1;
        // The fine histogram runs ALONGSIDE the log2 one (sub-budget
        // resolution; the overflow semantics of `step_hist` are
        // untouched). One saturating increment, integer only — no float
        // on the hot path; steps at/above the cap are simply absent from
        // it.
        if let Some(fi) = fine_hist_index(step_us) {
            self.step_fine_hist[fi] = self.step_fine_hist[fi].saturating_add(1);
        }
    }
}
