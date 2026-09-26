//! Merging per-actor samples into ONE reported number — and the rule
//! that says how, written next to the fold that applies it.
//!
//! A sharded room is N shard actors; each is its own metrics producer
//! with its own sample id (`room << 16 | index`), so the collector
//! reports N rows and every single-room-shaped consumer downstream needs
//! them folded into one. Three fields were fixed here one at a time over
//! three rounds (`step_min_us`, then `late_min_us`, then this audit)
//! because the fold was a *mutation of a copy of the first row*: a field
//! nobody remembered to touch silently reported shard 0's value, and
//! nothing in the type system asked.
//!
//! The shape below is the answer to that. The loop **destructures
//! [`RoomReport`] exhaustively**, so a field added to the report stops
//! compiling here until someone writes its rule down. There is no
//! `..` rest pattern and no field is skipped; `room` is bound to `_`
//! with the reason spelled out, which is itself a decision.
//!
//! ## The rule per field
//!
//! | Rule | Fields | Why |
//! |---|---|---|
//! | NOT MERGED | `room` | An identity, not a measurement. The folded row keeps the first shard's id; no consumer prints it. |
//! | MAX | `steps` | The shards tick in lockstep off ONE global ticker, so the room's step count is a shard's, not their sum. Also what `report_steps` uses for recency. |
//! | MAX | `step_max_us`, `late_max_us`, `snap_bytes_max`, `max_group` | Worst case over the room: the bottleneck shard. |
//! | MIN | `step_min_us`, `late_min_us` | Best case over the room. The fold for a minimum is `min`, never "whatever the first shard said". |
//! | MIN (positive) | `hz` | The shards step together, so a lagging shard drags the room. `hz = 0.0` means "no sample in this window" (see [`RoomReport::hz`]), not "stopped", so a zero is skipped rather than min'd. |
//! | MIN (config) | `budget_us` | CONFIGURATION, not a measurement — the shards share one `RoomConfig` and always agree. If they ever do not, the smaller budget is the honest answer: it is the denominator of the overflow fraction and of the histogram edges, and it reads overflow *earlier*. |
//! | MEAN, steps-weighted | `step_mean_us`, `late_mean_us` | A mean of means is not a mean. Each shard's mean is `sum / steps`, so weighting by `steps` and dividing by the total reconstructs `Σsum / Σsteps` exactly. |
//! | SUM, element-wise | `step_hist`, `step_fine_hist` | The union of the shards' step distributions, so percentiles and over-budget % are room-wide. See [`folded_steps`] for the population this union covers. |
//! | SUM | `lagged_events`, `lagged_ticks`, `dropped`, `keepalive_resends`, `snapshots`, `snap_overflows`, `snap_records`, `shipped_bytes`, `shipped_frames`, `private_frames`, `joins`, `leaves`, `resumes`, `resume_rejected_stale`, `detach_expired_despawn`, `detach_expired_ai`, `detach_forced`, the `effects_*`, `migrations_*` and `team_*` families, the whole `requests_*` family, `metrics_dropped` | Cumulative counters over disjoint work. (A migration is counted once as `migrations_out` by its source and once as `migrations_in` by its destination, so the folded pair should agree — they are not added together.) |
//! | SUM | `dropped_s`, `snap_bytes_s`, `shipped_s` | A RATE computed per shard cannot be averaged: the shards' counters are disjoint over the same wall clock, so the room's rate is their sum. (Averaging would report a quarter of the room's loss on a 4-shard room.) |
//! | SUM | `groups`, `members`, `detached`, `pending_requests` | Gauges, but PARTITIONED ones — the shards partition the room's connections, groups, parked sessions and in-flight requests, so the room's value is the total. (`max_group` and `snap_bytes_max` are the counter-example: an extremum over a population, not a population.) |
//! | PER COUNTER | `logic` | The logic's own counters (F9) carry their rule with them: name by name, a `LogicFold::Sum` counter adds (disjoint work, like the SUM row above), a `LogicFold::Max` one takes the larger (a high-water mark, like the MAX row); a name only some shards report is kept; the overflow counts add (`LogicCounters::merge`). |
//!
//! This fold runs once, in the load generator's end-of-run
//! `print_report` — never on a tick path.

use gsb_core::metrics::{FINE_HIST_CAP_US, MetricReport, RoomReport, fine_hist_percentile_us};

#[cfg(test)]
mod tests;

/// Total room membership in a report: the SUM over all rooms. For a
/// single room (every non-sharded strategy) this is that room's member
/// count; for a sharded room it is the room's total population (the
/// shards partition the room's connections).
pub(crate) fn report_members(report: &MetricReport) -> u32 {
    report.rooms.iter().map(|r| r.members).sum()
}

/// The report's cumulative step count as a "latest report" proxy: the MAX
/// over all rooms. For a single room that room's steps; for a sharded room
/// the shards step in lockstep (one global ticker) so any shard's count
/// marks the report's recency.
pub(crate) fn report_steps(report: &MetricReport) -> u64 {
    report.rooms.iter().map(|r| r.steps).max().unwrap_or(0)
}

/// The number of steps a (possibly folded) report's step histograms
/// cover — which is NOT [`RoomReport::steps`] once shards are folded.
///
/// `steps` folds with MAX (the room's tick count); the two histograms
/// fold with SUM (the union of the shards' distributions), so their
/// population is Σsteps. A percentile taken over a folded histogram must
/// be taken against THIS number: handing
/// `gsb_core::metrics::fine_hist_percentile_us` the tick count instead
/// asks for a rank a shard-count fraction of the way into the union, and
/// the answer comes back far too low (on a 4-shard room the "p50" is
/// really the p12.5).
///
/// The log2 histogram is the exact population, not an estimate of it:
/// `RoomCounters::observe_step_us` bins every step it observes and its
/// top bin is unbounded, so `Σ step_hist == Σ steps` for every actor.
/// The FINE histogram cannot be used for this — steps at or above its
/// cap are deliberately absent from it, which is exactly why
/// `fine_hist_percentile_us` takes the population as an argument.
pub(crate) fn folded_steps(r: &RoomReport) -> u64 {
    r.step_hist.iter().sum()
}

/// The sub-budget percentile pair the report lines print
/// (`step_p50_fine_us` / `step_p90_fine_us`), taken against
/// [`folded_steps`] — the one place that pairing is written down, so a
/// second consumer cannot reach for `steps` again.
///
/// [`FINE_HIST_CAP_US`] is the "the rank sits at or above the fine
/// histogram's cap" answer: unambiguous, since no fine-bin lower edge
/// equals the cap (they top out at 4088).
pub(crate) fn fine_percentiles_us(r: &RoomReport) -> (u64, u64) {
    let pop = folded_steps(r);
    let at = |q| fine_hist_percentile_us(&r.step_fine_hist, pop, q).unwrap_or(FINE_HIST_CAP_US);
    (at(50), at(90))
}

/// Fold a report's rooms into ONE [`RoomReport`] so the
/// (single-room-shaped) print code works for both: a single room
/// (identity fold) and a sharded room (N shard reports). The rule per
/// field is the table in this module's docs; the loop below is the only
/// place that applies it.
pub(crate) fn fold_rooms(report: &MetricReport) -> Option<RoomReport> {
    let mut rest = report.rooms.iter().copied();
    // The seed is the first row and it is CONSUMED from the iterator:
    // the loop must not absorb it a second time. It used to
    // (`acc = *first` followed by a loop over the whole slice), which
    // double-counted shard 0 in every SUM — a 4-shard 50-client run
    // reported 61 members and a step histogram holding 5 shards' steps.
    let mut acc = rest.next()?;
    if report.rooms.len() == 1 {
        // Identity, exactly: no weighted mean round-trips through a
        // division, no float drift on a non-sharded run.
        return Some(acc);
    }
    // Weighted-mean and positive-rate accumulators, seeded with the row
    // the iterator already yielded.
    let mut weight = u128::from(acc.steps);
    let mut step_us_weighted = acc.step_mean_us * acc.steps as f64;
    let mut late_us_weighted = acc.late_mean_us * acc.steps as f64;
    let mut slowest_hz = (acc.hz > 0.0).then_some(acc.hz);

    for r in rest {
        // EXHAUSTIVE destructuring — the structural guard. A field added
        // to `RoomReport` fails to compile HERE until it is given a rule
        // below. Do not add `..`, and do not bind a field you then leave
        // unused: an unused binding is a rule nobody wrote.
        let RoomReport {
            // NOT MERGED: an identity. `acc` keeps the first shard's id.
            room: _,
            steps,
            hz,
            budget_us,
            step_min_us,
            step_mean_us,
            step_max_us,
            step_hist,
            step_fine_hist,
            late_min_us,
            late_mean_us,
            late_max_us,
            lagged_events,
            lagged_ticks,
            dropped,
            dropped_s,
            keepalive_resends,
            snapshots,
            snap_bytes_s,
            snap_bytes_max,
            snap_overflows,
            snap_records,
            shipped_bytes,
            shipped_s,
            shipped_frames,
            private_frames,
            groups,
            members,
            max_group,
            joins,
            leaves,
            detached,
            resumes,
            resume_rejected_stale,
            detach_expired_despawn,
            detach_expired_ai,
            detach_forced,
            effects_applied,
            effects_forwarded,
            effects_orphaned,
            effects_dropped,
            effects_refused,
            migrations_out,
            migrations_in,
            migrations_failed,
            team_exports,
            team_export_drops,
            team_export_records,
            team_over_cap,
            team_imports,
            team_import_records,
            team_expired,
            team_over_budget,
            requests_local,
            requests_external,
            requests_rejected_malformed,
            requests_rejected_dup,
            requests_rejected_no_handler,
            requests_rejected_logic,
            requests_rejected_conn_cap,
            requests_rejected_room_cap,
            requests_timed_out,
            requests_late,
            pending_requests,
            metrics_dropped,
            logic,
        } = r;

        // ── MAX: lockstep tick count, and the worst-case extremes ──
        acc.steps = acc.steps.max(steps);
        acc.step_max_us = acc.step_max_us.max(step_max_us);
        acc.late_max_us = acc.late_max_us.max(late_max_us);
        acc.snap_bytes_max = acc.snap_bytes_max.max(snap_bytes_max);
        acc.max_group = acc.max_group.max(max_group);

        // ── MIN: the best-case extremes, and the configured budget ──
        acc.step_min_us = acc.step_min_us.min(step_min_us);
        acc.late_min_us = acc.late_min_us.min(late_min_us);
        acc.budget_us = acc.budget_us.min(budget_us);

        // ── MIN over the shards that actually reported a rate ──
        if hz > 0.0 {
            slowest_hz = Some(slowest_hz.map_or(hz, |m: f64| m.min(hz)));
        }

        // ── MEAN, weighted by the step count each mean averages over ──
        weight += u128::from(steps);
        step_us_weighted += step_mean_us * steps as f64;
        late_us_weighted += late_mean_us * steps as f64;

        // ── SUM, element-wise: the union of the distributions ──
        for (a, b) in acc.step_hist.iter_mut().zip(step_hist) {
            *a += b;
        }
        for (a, b) in acc.step_fine_hist.iter_mut().zip(step_fine_hist) {
            *a += b;
        }

        // ── SUM: cumulative counters over disjoint work ──
        acc.lagged_events += lagged_events;
        acc.lagged_ticks += lagged_ticks;
        acc.dropped += dropped;
        acc.keepalive_resends += keepalive_resends;
        acc.snapshots += snapshots;
        acc.snap_overflows += snap_overflows;
        acc.snap_records += snap_records;
        acc.shipped_bytes += shipped_bytes;
        acc.shipped_frames += shipped_frames;
        acc.private_frames += private_frames;
        acc.joins += joins;
        acc.leaves += leaves;
        acc.resumes += resumes;
        acc.resume_rejected_stale += resume_rejected_stale;
        acc.detach_expired_despawn += detach_expired_despawn;
        acc.detach_expired_ai += detach_expired_ai;
        acc.detach_forced += detach_forced;
        acc.effects_applied += effects_applied;
        acc.effects_forwarded += effects_forwarded;
        acc.effects_orphaned += effects_orphaned;
        acc.effects_dropped += effects_dropped;
        acc.effects_refused += effects_refused;
        acc.migrations_out += migrations_out;
        acc.migrations_in += migrations_in;
        acc.migrations_failed += migrations_failed;
        acc.team_exports += team_exports;
        acc.team_export_drops += team_export_drops;
        acc.team_export_records += team_export_records;
        acc.team_over_cap += team_over_cap;
        acc.team_imports += team_imports;
        acc.team_import_records += team_import_records;
        acc.team_expired += team_expired;
        acc.team_over_budget += team_over_budget;
        acc.requests_local += requests_local;
        acc.requests_external += requests_external;
        acc.requests_rejected_malformed += requests_rejected_malformed;
        acc.requests_rejected_dup += requests_rejected_dup;
        acc.requests_rejected_no_handler += requests_rejected_no_handler;
        acc.requests_rejected_logic += requests_rejected_logic;
        acc.requests_rejected_conn_cap += requests_rejected_conn_cap;
        acc.requests_rejected_room_cap += requests_rejected_room_cap;
        acc.requests_timed_out += requests_timed_out;
        acc.requests_late += requests_late;
        acc.metrics_dropped += metrics_dropped;

        // ── SUM: per-shard RATES over the same wall clock ──
        acc.dropped_s += dropped_s;
        acc.snap_bytes_s += snap_bytes_s;
        acc.shipped_s += shipped_s;

        // ── SUM: gauges the shards PARTITION ──
        acc.groups += groups;
        acc.members += members;
        acc.detached += detached;
        acc.pending_requests += pending_requests;

        // ── PER COUNTER: the logic's own counters, each by its rule ──
        acc.logic.merge(&logic);
    }

    acc.hz = slowest_hz.unwrap_or(0.0);
    let (step_mean_us, late_mean_us) = if weight > 0 {
        let w = weight as f64;
        (step_us_weighted / w, late_us_weighted / w)
    } else {
        (0.0, 0.0)
    };
    acc.step_mean_us = step_mean_us;
    acc.late_mean_us = late_mean_us;
    Some(acc)
}
