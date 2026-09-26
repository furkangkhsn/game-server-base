//! The metrics path: actor-local counters, carried out over a channel.
//!
//! Nothing in this module — or anywhere in the architecture — is shared
//! mutable state. Counters live in the producing actor's own local state
//! (the room, the registry, each connection actor); an actor hands a
//! compact **sample** of them to the collector over a *bounded* `mpsc`
//! channel with a synchronous `try_send` — a mailbox exactly like every
//! other actor channel in this codebase (a transport, not shared state),
//! with bounded capacity as the backpressure mechanism and a counted drop
//! instead of a park when it saturates (the next paragraph has the full
//! why). The collector accumulates the samples into its own task-local
//! [`MetricAccumulator`] and emits a [`MetricReport`] at a fixed cadence
//! through a [`MetricSink`] (tracing log lines, or a channel to a
//! programmatic consumer such as the load generator), after handing it
//! to every [`Exporter`] — the export seam (`export`: the Prometheus
//! exposition, the OTLP push).
//!
//! **Bounded channel, synchronous `try_send`, drops counted.** DESIGN §2
//! makes *bounded capacity* the backpressure mechanism, so the metrics path
//! uses a bounded `mpsc` channel — not an unbounded one. A bounded
//! `Sender::send` is a *future* (it parks when full), and awaiting it would
//! add an `await` to the tick body, which the tick-architecture constraint
//! forbids (the room's only `await` must stay `tick_rx.recv()`). The third
//! option is the one used: **bounded + `try_send`** — a plain synchronous
//! call that *drops* on overflow instead of parking. This is the project's
//! existing pattern (`OutSink::flush` → `dropped_frames`; the per-connection
//! action channel → `ConnSample::actions_dropped`).
//!
//! A drop here is *harmless*: every sample's counters are cumulative, so a
//! lost sample carries nothing the next sample does not already carry —
//! the same self-contained-snapshot logic the broadcast phase relies on.
//! Each producer counts its own drops (cumulative for the room and
//! registry, delta for a connection actor) and the total is surfaced in the
//! report (`MetricReport::metrics_dropped`); in normal operation it stays
//! 0 because each room sends at most one sample per report period (see
//! `RoomConfig::metrics_cadence_hz`) and the collector drains the whole
//! channel on every tick.
//!
//! **Why not atomics:** `std::sync::AtomicU64` in a global registry would
//! work, but it is shared mutable state — the principle this architecture
//! enforces (DESIGN §2) is that every value is *moved* into exactly one
//! owner, and cross-actor data travels as channel messages. The channel
//! path keeps the counters in the actor that owns them (they can be
//! derived from the same local state the actor already inspects for its
//! own logic, with no second copy to keep consistent), costs one small
//! allocation per step per producer, and makes "what the collector saw"
//! a pure function of the message stream (deterministic, testable,
//! reorder-free per source).
//!
//! **The collector's clock:** the collector task owns one awaited source —
//! a subscription to the global ticker's broadcast (the same channel the
//! rooms subscribe to) — and drains the event channel non-blockingly on
//! each tick, mirroring the room actor's discipline (one `recv`,
//! synchronous body, no `select!`). Reports go out at most once per
//! report period; the ticker's cadence (30–60 Hz) is far finer than a
//! report period, so no event is ever older than one tick before it is
//! processed. Ticker `Closed` (shutdown) triggers one final report and a
//! clean exit.
//!
//! **What is measured** (chosen so every P0 load question is answerable
//! from the report alone):
//! - *tick health per room*: measured step rate (Δsteps/s), missed
//!   broadcast ticks (`lagged_*`, the `Lagged` catch-up path), tick
//!   processing latency (`late_*`: step start − ticker timestamp), step
//!   body duration min/mean/max + a budget-relative log-2 histogram (the
//!   tick budget is the overflow boundary — see [`HIST_EDGES`]);
//! - *drops*: batches dropped at the fan-out (`dropped_frames`, slow
//!   client). Input drops are NOT a room-scope counter: the room's READ
//!   phase is a bounded pull that defers rather than drops, so the only
//!   input-loss point is a connection's own full action channel — counted
//!   at the net scope (`ConnSample::actions_dropped` →
//!   `NetReport::actions_dropped`) and attributed to its sender by
//!   `MetricReport::actions_dropped_top`;
//! - *broadcast*: snapshots encoded, encoded bytes, largest payload seen,
//!   keep-alive re-sends, payloads shipped to clients (bytes/frames),
//!   private frames;
//! - *groups*: current group count, member count, largest group;
//! - *control plane*: rooms/connections (current), rooms
//!   created/destroyed, joins/leaves/opens/closes (cumulative);
//! - *wire bytes*: bytes in (every connection actor counts its inbound
//!   frames; it flushes deltas on inbound traffic and a final flush at
//!   close) and bytes out (room fan-out bytes + the connection actors'
//!   own control frames).
//!
//! Rates (`hz`, `*_s`) are computed by the collector over the **sample
//! interval** (each room sample carries its `emit_at`; the room sends one
//! sample per report period, so a report window — not phase-locked to the
//! sample cadence — would mis-time the rate) — the actors send cumulative
//! samples and current gauges, nothing else.

mod accumulator;
mod closes;
mod collector;
mod export;
mod logic;
mod render;
mod report;
mod sample;

#[cfg(test)]
mod tests;

pub use accumulator::MetricAccumulator;
pub use closes::ServerCloses;
pub use collector::{MetricSink, MetricsCollector};
pub use export::Exporter;
#[cfg(feature = "otlp")]
pub use export::otlp;
pub use logic::{
    LOGIC_COUNTERS_MAX, LOGIC_NAME_MAX, LogicCounter, LogicCounters, LogicFold, LogicSlot,
};
pub use report::{MetricReport, NetReport, RegistryReport, RoomReport};
pub use sample::{ConnSample, MetricsEvent, RegistrySample, RoomSample};

/// Histogram bin edges for the per-step body duration, each expressed as a
/// fraction of the room's **tick budget** (one period, in µs). Each edge is
/// a `(num, den)` pair so the concrete µs edge is `ceil(budget_us * num / den)`
/// (integer-only — no float on the binning path).
///
/// The `(1, 1)` edge is **exactly the tick budget**: every bin at or above
/// [`HIST_OVERFLOW_BIN`] is *budget overflow* — a step that took longer than
/// one period, i.e. the room cannot keep its rate. A log-2 ladder on *both*
/// sides of it: below, down to 1/128× so a healthy room (steps well under the
/// budget) still has a resolvable distribution and a meaningful median (at a
/// 30 Hz budget of 33 ms the low bins land at ~260/520/1040 µs — where real
/// step times live); above, up to 32× so the degree of overflow is readable
/// (a 1.5× step and a 150× step land in *different* bins instead of one
/// `[5000, ∞)` bucket).
///
/// Why budget *ratios* instead of absolute µs: the whole point of this
/// histogram is to read "are we inside the budget, and by how much do we
/// overshoot". An absolute-µs array can only encode the budget for the one
/// tick rate it was tuned to, and the architecture supports 15/30/60 Hz
/// rooms (DESIGN §10: a room rate divides the global rate). Ratios make the
/// budget a bin boundary at *any* rate, which is exactly the criterion the
/// spec fixes for this fix.
pub const HIST_EDGES: [(u64, u64); 13] = [
    (1, 128),
    (1, 64),
    (1, 32),
    (1, 16),
    (1, 8),
    (1, 4),
    (1, 2),
    (1, 1),
    (2, 1),
    (4, 1),
    (8, 1),
    (16, 1),
    (32, 1),
];
/// Number of histogram bins (`HIST_EDGES.len() + 1`).
pub const HIST_BINS: usize = HIST_EDGES.len() + 1;
/// The first overflow bin: a step duration at or above 1.0× the tick budget
/// (the `(1, 1)` edge, index 7) lands here or higher.
pub const HIST_OVERFLOW_BIN: usize = 8;

/// Concrete µs edge `i` for a tick budget of `budget_us` (µs).
#[inline]
pub fn hist_edge_us(budget_us: u64, i: usize) -> u64 {
    let (num, den) = HIST_EDGES[i];
    (budget_us * num).div_ceil(den)
}

/// Bin index for a step of `step_us` against the room's tick budget in µs.
/// Bins (fractions of `b = budget_us`):
/// `[0,1/128) [1/128,1/64) [1/64,1/32) [1/32,1/16) [1/16,1/8) [1/8,1/4)
/// [1/4,1/2) [1/2,1) [1,2) [2,4) [4,8) [8,16) [16,32) [32,∞)`; the `(1,1)`
/// edge (bin [`HIST_OVERFLOW_BIN`]) is the tick budget.
#[inline]
pub fn hist_index(budget_us: u64, step_us: u64) -> usize {
    let mut i = 0;
    while i < HIST_EDGES.len() && step_us >= hist_edge_us(budget_us, i) {
        i += 1;
    }
    i
}

// ── Fine step-duration histogram (A: sub-budget resolution) ──────────
//
// The budget-relative log2 histogram above answers "are we inside the
// budget, and by how much do we overshoot" (the `(1,1)` overflow edge).
// Its bins double, so a 10-20% change far below the budget is invisible
// (a 390 µs step and a 460 µs step are the SAME bin). This companion
// histogram answers the other question — "which micro-optimization
// worked" — with FIXED absolute bins: 8 µs wide, covering
// `[0, 4096 µs)`. Steps at or above the cap are NOT double-counted
// here: they remain visible in the log2 histogram only (the two
// histograms are complementary, and the overflow semantics of the log2
// one are untouched). Absolute bins (not budget fractions) on purpose:
// the resolution target is absolute µs in the region where real step
// times live, at any tick rate; a budget-fraction fine histogram would
// just re-scale the same 2×-apart problem.
pub const FINE_HIST_US_PER_BIN: u64 = 8;
/// Number of fine bins: covers `[0, FINE_HIST_BINS * FINE_HIST_US_PER_BIN)` µs.
pub const FINE_HIST_BINS: usize = 512;
/// The fine histogram's cap in µs: steps at or above it land only in the
/// log2 histogram.
pub const FINE_HIST_CAP_US: u64 = FINE_HIST_BINS as u64 * FINE_HIST_US_PER_BIN;

/// Fine-bin index for a step duration; `None` at/above the cap (the step
/// is then readable in the log2 histogram only).
#[inline]
pub fn fine_hist_index(step_us: u64) -> Option<usize> {
    let i = (step_us / FINE_HIST_US_PER_BIN) as usize;
    (i < FINE_HIST_BINS).then_some(i)
}

/// Exact percentile of a fine histogram, INTEGER arithmetic (the hot path
/// only counts bins; this runs at report time and deliberately uses no
/// float). `total` is the room's TOTAL step count (including steps at or
/// above the cap, which do not appear in `hist`); the percentile is taken
/// over all steps, and the answer is the bin's lower edge `L` — the
/// smallest `L` such that at least `p` percent of the steps are ≤
/// `L + FINE_HIST_US_PER_BIN - 1`. `None` when the histogram is empty,
/// `p` is out of `[1, 100]`, or the percentile's rank falls beyond the
/// cap (then the log2 histogram's coarse estimate applies).
pub fn fine_hist_percentile_us(hist: &[u64], total: u64, p: u32) -> Option<u64> {
    if total == 0 || p == 0 || p > 100 {
        return None;
    }
    let fine_total: u64 = hist.iter().copied().sum();
    if fine_total == 0 {
        return None;
    }
    let target = (u128::from(total) * u128::from(p)).div_ceil(100);
    if target > u128::from(fine_total) {
        return None; // the p-th step sits at/above the cap
    }
    let mut acc = 0u64;
    for (i, &n) in hist.iter().enumerate() {
        acc += n;
        if u128::from(acc) >= target {
            return Some(i as u64 * FINE_HIST_US_PER_BIN);
        }
    }
    None
}
