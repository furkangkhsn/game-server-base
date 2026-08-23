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
//! programmatic consumer such as the load generator).
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
//! action channel → `dropped_actions`).
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
//!   client) and input actions dropped on overflow (`dropped_actions`);
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

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc};

use crate::id::{ConnectionId, RoomId};
use crate::ticker::TickInfo;

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
    (1, 128), (1, 64), (1, 32), (1, 16), (1, 8), (1, 4), (1, 2),
    (1, 1), (2, 1), (4, 1), (8, 1), (16, 1), (32, 1),
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

/// A room's counter sample: cumulative counters + current gauges,
/// produced once per step (synchronous — see the module docs for why the
/// bounded `try_send` adds no await to the tick loop).
#[derive(Debug, Clone, Copy)]
pub struct RoomSample {
    pub room: RoomId,
    /// Wall-clock instant the room emitted this sample. The collector rates
    /// over the **sample interval** (this minus the previous sample's), not
    /// the report window: the room sends one sample per report period (A2)
    /// and the two cadences are not phase-locked, so a report window can span
    /// 0–2 samples and a report-window rate would be wrong.
    pub emit_at: Instant,
    /// Steps run (cumulative).
    pub steps: u64,
    /// Broadcast `Lagged` occurrences / total missed tick indices (the
    /// room fell behind the ticker's buffer; each one is caught up via
    /// the wall-clock `dt` of the next step).
    pub lagged_events: u64,
    pub lagged_ticks: u64,
    /// The room's tick budget in µs (one period, `1/tick_hz * 1e6`). The
    /// histogram's edges are fractions of this (see [`HIST_EDGES`]); consumers
    /// use it to map bins back to µs and to read the overflow boundary.
    pub budget_us: u64,
    /// Step body duration, µs: min / max / sum (mean = sum / steps).
    pub step_min_us: u64,
    pub step_max_us: u64,
    pub step_sum_us: u64,
    /// Step body duration histogram (cumulative; [`HIST_EDGES`] as fractions
    /// of [`Self::budget_us`]). Bins `>= HIST_OVERFLOW_BIN` are budget
    /// overflow.
    pub step_hist: [u64; HIST_BINS],
    /// Fine step-duration histogram (cumulative; fixed 8 µs bins covering
    /// `[0, FINE_HIST_CAP_US)` µs — sub-budget resolution; steps at/above
    /// the cap are only in [`Self::step_hist`], whose overflow semantics
    /// this does not touch).
    pub step_fine_hist: [u32; FINE_HIST_BINS],
    /// Tick processing latency (step start − the ticker's `at`
    /// timestamp), µs: min / max / sum.
    pub late_min_us: u64,
    pub late_max_us: u64,
    pub late_sum_us: u64,
    /// Outbound batches dropped at the fan-out (out channel full: slow
    /// client), cumulative.
    pub dropped_frames: u64,
    /// Input actions dropped on READ overflow, cumulative.
    pub dropped_actions: u64,
    /// Keep-alive re-sends (unchanged groups re-sending their cached
    /// snapshot), cumulative.
    pub keepalive_resends: u64,
    /// Group snapshots encoded (a group that reports "unchanged" encodes
    /// nothing), cumulative.
    pub snapshots: u64,
    /// Snapshot payload bytes encoded (once per group per emit),
    /// cumulative.
    pub snap_bytes: u64,
    /// Largest single group payload seen so far (bytes).
    pub snap_bytes_max: u32,
    /// Snapshots whose payload exceeded `max_snapshot_bytes`, cumulative.
    /// (The room warns once per group, but this counts every oversized emit
    /// — the AOI/MTU-signal the load test reports: a whole-world snapshot
    /// overflows every tick at scale, a per-cell snapshot does not.)
    pub snap_overflows: u64,
    /// Entity records encoded (summed over all groups, via
    /// `RoomLogic::encoded_records`), cumulative. Over the broadcastable
    /// entity count this is the *overlap multiplier* (how many group
    /// snapshots each entity landed in per tick — the "encode-per-unit"
    /// decision input, see `docs/ROADMAP.md`).
    pub snap_records: u64,
    /// Snapshot + private payload bytes shipped to the room's
    /// connections (per-connection fan-out copies), cumulative.
    pub shipped_bytes: u64,
    /// Frames shipped to the room's connections (snapshot + private),
    /// cumulative.
    pub shipped_frames: u64,
    /// Private frames shipped, cumulative.
    pub private_frames: u64,
    /// Joins / leaves processed on the control channel, cumulative.
    pub joins: u64,
    pub leaves: u64,
    /// RPC (see `crate::rpc`): requests answered room-local in the same
    /// tick, cumulative.
    pub requests_local: u64,
    /// RPC: requests delegated to a worker (registered pending),
    /// cumulative.
    pub requests_external: u64,
    /// RPC rejections, split by cause (the room's six terminal reject
    /// decisions — see `gsb_core::room`; each bucket answers a distinct
    /// operational question, which the cap-sizing measurement needs),
    /// cumulative: malformed envelope / id = 0, in-flight duplicate id,
    /// no handler for the op, the logic's own `Reject` decision,
    /// per-connection pending cap, room-wide pending cap.
    pub requests_rejected_malformed: u64,
    pub requests_rejected_dup: u64,
    pub requests_rejected_no_handler: u64,
    pub requests_rejected_logic: u64,
    pub requests_rejected_conn_cap: u64,
    pub requests_rejected_room_cap: u64,
    /// RPC: pending external requests swept as timed out (the
    /// client-visible timeout), cumulative.
    pub requests_timed_out: u64,
    /// RPC: worker reports for ids no longer pending (answered, timed
    /// out, or the connection left), dropped by the reconciliation,
    /// cumulative.
    pub requests_late: u64,
    /// RPC: external requests currently in flight (gauge).
    pub pending_requests: u32,
    /// Current gauges: snapshot groups, members (connections in the
    /// room), largest group.
    pub groups: u32,
    pub members: u32,
    pub max_group: u32,
    /// Metric samples this producer dropped because the (bounded) metrics
    /// channel was full, cumulative. Harmless (counters are cumulative — the
    /// next sample carries everything) but reported so an operator can see
    /// the channel saturating.
    pub metrics_dropped: u64,
}

/// The registry's table gauges + cumulative control-plane counters. The
/// registry emits one sample when a table changes (event-driven; the
/// registry has no timer and no new await — the send is synchronous).
#[derive(Debug, Clone, Copy)]
pub struct RegistrySample {
    /// Current room count.
    pub rooms: u32,
    /// Current registered connection count (the connection's whole
    /// lifetime, per the registry's table semantics).
    pub conns: u32,
    /// Rooms created / destroyed, cumulative.
    pub rooms_created: u64,
    pub rooms_destroyed: u64,
    /// Rooms that died UNEXPECTEDLY — a panicked room or shard task (one
    /// dead shard of a sharded room counts once; the whole logical room is
    /// reaped) — cumulative. Never incremented by a destroy. Non-zero means
    /// game logic panicked somewhere; the per-room detail is in the
    /// registry's `warn` at death time.
    pub rooms_died: u64,
    /// Joins (spawn done) / leaves (leave done), cumulative.
    pub joins: u64,
    pub leaves: u64,
    /// Connections opened / closed, cumulative.
    pub opens: u64,
    pub closes: u64,
    /// Metric samples this producer dropped on a full (bounded) metrics
    /// channel, cumulative.
    pub metrics_dropped: u64,
}

/// One connection actor's wire-byte sample. The fields are *deltas since
/// the actor's last flush*: the actor counts inbound frames as they
/// arrive (flushing at most once per flush interval, and a final sample
/// at close) and the control frames it sends itself. The dominant
/// outbound traffic (room fan-out) is counted by the *room* — see
/// [`RoomSample::shipped_bytes`] — so bytes out = room-shipped + these
/// control bytes.
#[derive(Debug, Clone, Copy)]
pub struct ConnSample {
    pub conn: ConnectionId,
    /// Wire bytes received (frame body: 2-byte op + payload), delta.
    pub bytes_in: u64,
    /// Wire bytes of control frames this actor sent (frame body), delta.
    pub bytes_out: u64,
    /// Frames received / control frames sent, delta.
    pub frames_in: u64,
    pub frames_out: u64,
    /// Input actions this actor dropped on a full (bounded) action
    /// channel, delta since its last flush. The only input-loss point in
    /// the architecture (the room's READ phase is a bounded *pull* — see
    /// [`RoomSample::dropped_actions`]) and always self-inflicted: a
    /// flooding connection drops its own input. The collector sums these
    /// per connection so a report can attribute the loss to its sender.
    pub actions_dropped: u64,
    /// Metric samples this actor dropped on a full (bounded) metrics
    /// channel, delta since its last flush.
    pub metrics_dropped: u64,
    /// Protocol-violation events counted by this actor's budget (module
    /// docs in `gsb_core::conn`), delta since its last flush. The net
    /// scope sums these: a rising total is the budget working (clients
    /// violating); the per-event detail (peer address, close reason) is in
    /// the tracing close signal, not here.
    pub violations: u64,
    /// True on the actor's final flush (connection closing).
    pub last: bool,
}

/// A metrics event: one actor's sample, in flight over the metrics
/// channel.
///
/// `RoomSample` dominates the size (it carries the 11-bin step histogram);
/// `RegistrySample`/`ConnSample` are smaller. Sizing the enum to the largest
/// variant (the default) is deliberate: boxing the large variant would heap-
/// allocate on every sample in the hot metrics path, which is the wrong
/// trade for a `Copy` struct passed by value over the channel.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Copy)]
pub enum MetricsEvent {
    Room(RoomSample),
    Registry(RegistrySample),
    Conn(ConnSample),
    /// A room was destroyed (or died unexpectedly and was reaped): its
    /// accumulator lingers for the grace windows below — reported, frozen,
    /// closed to straggler samples — and is then dropped. Emitted by the
    /// REGISTRY (the single authority on room existence) at the moment
    /// its table entry is removed.
    ///
    /// Why linger instead of dropping on the spot: the destroy often
    /// lands BETWEEN the room's last sample and the next report, and
    /// erasing immediately would discard that final MEASURED window —
    /// whose rate line is exactly what consumers diff when a room goes
    /// away. Why not linger forever: that is the unbounded per-room ghost
    /// this prune removes. See `ROOM_GONE_GRACE_REPORTS` for the window
    /// arithmetic and the straggler-ordering caveat (a late `Room`
    /// sample behind the notice is normal, not a resurrection attempt).
    RoomGone(RoomId),
}

/// How many report windows a destroyed room lingers after its
/// [`MetricsEvent::RoomGone`] notice before its accumulator is dropped:
/// reported once more with frozen counters, closed to new samples
/// (stragglers must not resurrect it). Two windows: stragglers arrive
/// within a tick or two of the destroy — orders of magnitude inside one
/// ~1 Hz report window — and a brand-new incarnation reusing the same
/// `RoomId` starts reporting normally once the windows burn down (its
/// counters restart from zero anyway, so the accepted cost is cosmetic).
const ROOM_GONE_GRACE_REPORTS: u32 = 2;

/// Per-room state in the accumulator: the latest sample plus the previous
/// sample (for rate computation). Rates are computed over the **sample
/// interval** — `latest.emit_at − prev.emit_at` — not the report window:
/// A2 makes the room send one sample per report period, and the two cadences
/// are not phase-locked, so a report window can span 0–2 samples and a
/// report-window rate (Δsteps / report Δt) would be wrong.
#[derive(Debug)]
struct RoomAcc {
    latest: RoomSample,
    /// The sample captured at the previous report; `None` before the first
    /// report.
    prev: Option<RoomSample>,
}

/// The collector's task-local state: pure accumulation over the event
/// stream — no shared state, no locks (owned by exactly one task).
#[derive(Debug, Default)]
pub struct MetricAccumulator {
    /// Live rooms plus destroyed rooms still inside their short
    /// report-window linger (see `ROOM_GONE_GRACE_REPORTS`): a destroyed
    /// room's entry is dropped when its windows run down, so this map is
    /// bounded by live rooms + recent destroys instead of every room id
    /// ever created.
    rooms: BTreeMap<RoomId, RoomAcc>,
    /// Destroyed-room ids still lingering: reported with frozen counters,
    /// closed to straggler samples, each report burning one window until
    /// the accumulator is dropped (see `ROOM_GONE_GRACE_REPORTS`).
    /// Bounded by the destroy rate × the constant window.
    rooms_gone_grace: BTreeMap<RoomId, u32>,
    registry: Option<RegistrySample>,
    conn_bytes_in: u64,
    conn_bytes_out: u64,
    conn_frames_in: u64,
    conn_frames_out: u64,
    /// Summed delta of connection-actor metric-channel drops (their sample
    /// is delta-based, like the other conn counters).
    conn_metrics_dropped: u64,
    /// Summed delta of protocol-violation events across all connection
    /// actors (the violation budget's activity, cumulative).
    conn_violations: u64,
    /// Cumulative input-action drops per connection (the sender's
    /// attribution: which connection's own input was lost to its full
    /// action channel). The collector owns this state — the connection
    /// actors only ever *report* their own deltas. Per-LIVE connection:
    /// the entry is retired into
    /// [`Self::conn_actions_dropped_retired`] when the connection's final
    /// flush arrives, so this map cannot grow by every connection that
    /// ever dropped a single action.
    conn_actions_dropped: BTreeMap<ConnectionId, u64>,
    /// Input-action drops RETIRED with their closed connections. The net
    /// scope's cumulative total folds this back in, so
    /// [`NetReport::actions_dropped`] stays monotonic even though the
    /// per-connection entries above are pruned at close.
    conn_actions_dropped_retired: u64,
}

/// Per-room slice of a report: current gauges + rates over the last
/// report period (rates are 0.0 until the room has reported twice).
#[derive(Debug, Clone, Copy)]
pub struct RoomReport {
    pub room: RoomId,
    /// Steps (cumulative) and measured rate (Δsteps/s since the previous
    /// report). Compare against the room's configured `tick_hz`: the room
    /// is on rate when these agree.
    ///
    /// Note: a report window in which the room emitted **no new sample**
    /// (the 1 Hz sample cadence and the 1 Hz report cadence are not
    /// phase-locked, and shutdown's final report often is such a window)
    /// reports `hz = 0.0` — read it as "no sample in this window", not
    /// "the room stopped". Consumers wanting a stable rate should median
    /// the positive windows (the load generator does).
    pub steps: u64,
    pub hz: f64,
    /// The room's tick budget in µs (the overflow boundary of the histogram).
    pub budget_us: u64,
    pub step_min_us: u64,
    pub step_mean_us: f64,
    pub step_max_us: u64,
    /// Cumulative step duration histogram ([`HIST_EDGES`] as fractions of
    /// [`Self::budget_us`]); bins `>= HIST_OVERFLOW_BIN` are budget overflow.
    pub step_hist: [u64; HIST_BINS],
    /// Fine step-duration histogram (cumulative; fixed 8 µs bins covering
    /// `[0, FINE_HIST_CAP_US)` µs — sub-budget resolution alongside
    /// [`Self::step_hist`]; `u64` so the shard fold can sum per element).
    pub step_fine_hist: [u64; FINE_HIST_BINS],
    pub late_min_us: u64,
    pub late_mean_us: f64,
    pub late_max_us: u64,
    /// Broadcast `Lagged` occurrences / missed ticks (cumulative).
    pub lagged_events: u64,
    pub lagged_ticks: u64,
    /// Batches dropped (cumulative) and drop rate (Δ/s).
    pub dropped: u64,
    pub dropped_s: f64,
    pub dropped_actions: u64,
    pub keepalive_resends: u64,
    /// Snapshots encoded (cumulative) and encoded-byte rate (Δ/s).
    pub snapshots: u64,
    pub snap_bytes_s: f64,
    pub snap_bytes_max: u32,
    pub snap_overflows: u64,
    /// Entity records encoded (cumulative; overlap-metric numerator — see
    /// [`RoomSample::snap_records`]).
    pub snap_records: u64,
    /// Bytes shipped to clients (cumulative) and shipped-byte rate (Δ/s)
    /// — the server-side bytes-out for this room.
    pub shipped_bytes: u64,
    pub shipped_s: f64,
    pub groups: u32,
    pub members: u32,
    pub max_group: u32,
    pub joins: u64,
    pub leaves: u64,
    /// RPC (see `crate::rpc`), cumulative: room-local answers, delegated
    /// (pending) requests, rejections split by cause (see
    /// `RoomSample::requests_rejected_malformed`), timeout sweeps, and
    /// late reports dropped by the reconciliation.
    pub requests_local: u64,
    pub requests_external: u64,
    pub requests_rejected_malformed: u64,
    pub requests_rejected_dup: u64,
    pub requests_rejected_no_handler: u64,
    pub requests_rejected_logic: u64,
    pub requests_rejected_conn_cap: u64,
    pub requests_rejected_room_cap: u64,
    pub requests_timed_out: u64,
    pub requests_late: u64,
    /// RPC: external requests currently in flight (gauge).
    pub pending_requests: u32,
    /// Metric samples dropped on a full metrics channel (cumulative).
    pub metrics_dropped: u64,
}

/// Registry slice of a report (latest gauges + cumulative counters).
#[derive(Debug, Clone, Copy)]
pub struct RegistryReport {
    pub rooms: u32,
    pub conns: u32,
    pub rooms_created: u64,
    pub rooms_destroyed: u64,
    /// Unexpected room/shard deaths (see [`RegistrySample::rooms_died`]).
    pub rooms_died: u64,
    pub joins: u64,
    pub leaves: u64,
    pub opens: u64,
    pub closes: u64,
}

/// Network slice of a report (cumulative since startup).
#[derive(Debug, Clone, Copy)]
pub struct NetReport {
    /// Bytes in over all connections (frame bodies).
    pub bytes_in: u64,
    /// Bytes out: room fan-out + connection control frames.
    pub bytes_out_room: u64,
    pub bytes_out_control: u64,
    pub bytes_out_total: u64,
    pub frames_in: u64,
    /// Control frames sent by connection actors.
    pub frames_out: u64,
    /// Input actions dropped on full (bounded) per-connection action
    /// channels (cumulative, all connections). The per-connection
    /// attribution (who dropped what) is in
    /// [`MetricReport::actions_dropped_top`].
    pub actions_dropped: u64,
    /// Protocol-violation events counted by the connection actors'
    /// violation budgets (cumulative, all connections). 0 on a healthy
    /// server; the per-event signal (peer address, close reason) is the
    /// structured tracing event emitted at budget exhaustion.
    pub violations: u64,
}

/// One periodic report: the server's current numeric state.
#[derive(Debug, Clone)]
pub struct MetricReport {
    /// Total metric samples dropped on the (bounded) metrics channel across
    /// all producers this report window. 0 in normal operation (A2 makes each
    /// room send at most one sample per report period); non-zero signals the
    /// collector falling behind (or a startup/shutdown flush burst).
    pub metrics_dropped: u64,
    pub rooms: Vec<RoomReport>,
    pub registry: Option<RegistryReport>,
    pub net: NetReport,
    /// Cumulative input-action drops per connection, worst offenders first
    /// (up to 5; ties broken by connection id). LIVE connections only — a
    /// closed connection's entry retires into the cumulative net total at
    /// its final flush. Empty when nothing was dropped by any currently
    /// connected peer — the only loss point for input is a connection's
    /// own full action channel, so this list names the flooders still on
    /// the wire.
    pub actions_dropped_top: Vec<(ConnectionId, u64)>,
}

impl MetricAccumulator {
    /// Apply one event. Pure state transition (owned by one task).
    pub fn apply(&mut self, ev: MetricsEvent) {
        match ev {
            MetricsEvent::Room(s) => {
                // Destroyed-room stragglers: while the room's id is inside
                // its grace window, samples for it are ignored — they are
                // the dying producers' final shutdown-step flushes racing
                // the destroy notice, and applying one would resurrect
                // the dead accumulator. When the window runs out the id
                // is forgotten: a NEW incarnation of the same RoomId
                // reports normally again (see ROOM_GONE_GRACE_REPORTS for
                // the accepted cost).
                if let Some(&left) = self.rooms_gone_grace.get(&s.room) {
                    if left > 0 {
                        return;
                    }
                    self.rooms_gone_grace.remove(&s.room);
                }
                match self.rooms.get_mut(&s.room) {
                    Some(acc) => acc.latest = s,
                    None => {
                        self.rooms
                            .insert(s.room, RoomAcc { latest: s, prev: None });
                    }
                }
            }
            MetricsEvent::Registry(s) => self.registry = Some(s),
            MetricsEvent::Conn(c) => {
                self.conn_bytes_in = self.conn_bytes_in.saturating_add(c.bytes_in);
                self.conn_bytes_out = self.conn_bytes_out.saturating_add(c.bytes_out);
                self.conn_frames_in = self.conn_frames_in.saturating_add(c.frames_in);
                self.conn_frames_out = self.conn_frames_out.saturating_add(c.frames_out);
                if c.actions_dropped > 0 {
                    let entry = self.conn_actions_dropped.entry(c.conn).or_default();
                    *entry = entry.saturating_add(c.actions_dropped);
                }
                self.conn_metrics_dropped =
                    self.conn_metrics_dropped.saturating_add(c.metrics_dropped);
                self.conn_violations = self.conn_violations.saturating_add(c.violations);
                if c.last {
                    // The final flush is the connection actor's LAST
                    // emission (its deltas fold above first — a closing
                    // flooder's last drops are still counted), so retiring
                    // here cannot be undone by a late delta. The
                    // attribution merges into the cumulative total (the
                    // net scope stays monotonic); the per-connection
                    // entry is freed — a closed connection cannot flood
                    // again, so naming it in `actions_dropped_top` has no
                    // operator value.
                    if let Some(n) = self.conn_actions_dropped.remove(&c.conn) {
                        self.conn_actions_dropped_retired =
                            self.conn_actions_dropped_retired.saturating_add(n);
                    }
                }
            }
            MetricsEvent::RoomGone(id) => {
                // Linger, then go: the accumulator stays (so the room's
                // final measured window still reaches the next report —
                // see the variant docs), samples are suppressed meanwhile
                // (the dying producers' shutdown-tick flushes must not
                // resurrect or refresh it), and report() drops the entry
                // when the windows run down. Idempotent: a repeated
                // notice just restarts the window.
                self.rooms_gone_grace.insert(id, ROOM_GONE_GRACE_REPORTS);
            }
        }
    }

    /// Snapshot the accumulated state as a report and advance the rate
    /// window. Per-room rates are Δ since the previous sample, over the
    /// sample interval (see [`RoomAcc::latest_at`]); `at` (the report time)
    /// is kept for the caller's bookkeeping but no longer drives the rates.
    pub fn report(&mut self, _at: Instant) -> MetricReport {
        let mut rooms = Vec::with_capacity(self.rooms.len());
        for (id, acc) in &mut self.rooms {
            let latest = acc.latest;
            // Rate window = the SAMPLE interval (this sample's `emit_at`
            // minus the previous sample's), not the report window — see
            // `RoomSample::emit_at`.
            let (hz, dropped_s, snap_bytes_s, shipped_s) = match acc.prev {
                Some(p) => {
                    let dt = latest
                        .emit_at
                        .saturating_duration_since(p.emit_at)
                        .as_secs_f64()
                        .max(1e-9);
                    (
                        (latest.steps as f64 - p.steps as f64) / dt,
                        (latest.dropped_frames as f64 - p.dropped_frames as f64) / dt,
                        (latest.snap_bytes as f64 - p.snap_bytes as f64) / dt,
                        (latest.shipped_bytes as f64 - p.shipped_bytes as f64) / dt,
                    )
                }
                None => (0.0, 0.0, 0.0, 0.0),
            };
            let steps = latest.steps.max(1);
            rooms.push(RoomReport {
                room: *id,
                steps: latest.steps,
                hz,
                budget_us: latest.budget_us,
                step_min_us: latest.step_min_us,
                step_mean_us: latest.step_sum_us as f64 / steps as f64,
                step_max_us: latest.step_max_us,
                step_hist: latest.step_hist,
                step_fine_hist: latest.step_fine_hist.map(u64::from),
                late_min_us: latest.late_min_us,
                late_mean_us: latest.late_sum_us as f64 / steps as f64,
                late_max_us: latest.late_max_us,
                lagged_events: latest.lagged_events,
                lagged_ticks: latest.lagged_ticks,
                dropped: latest.dropped_frames,
                dropped_s,
                dropped_actions: latest.dropped_actions,
                keepalive_resends: latest.keepalive_resends,
                snapshots: latest.snapshots,
                snap_bytes_s,
                snap_bytes_max: latest.snap_bytes_max,
                snap_overflows: latest.snap_overflows,
                snap_records: latest.snap_records,
                shipped_bytes: latest.shipped_bytes,
                shipped_s,
                groups: latest.groups,
                members: latest.members,
                max_group: latest.max_group,
                joins: latest.joins,
                leaves: latest.leaves,
                requests_local: latest.requests_local,
                requests_external: latest.requests_external,
                requests_rejected_malformed: latest.requests_rejected_malformed,
                requests_rejected_dup: latest.requests_rejected_dup,
                requests_rejected_no_handler: latest.requests_rejected_no_handler,
                requests_rejected_logic: latest.requests_rejected_logic,
                requests_rejected_conn_cap: latest.requests_rejected_conn_cap,
                requests_rejected_room_cap: latest.requests_rejected_room_cap,
                requests_timed_out: latest.requests_timed_out,
                requests_late: latest.requests_late,
                pending_requests: latest.pending_requests,
                metrics_dropped: latest.metrics_dropped,
            });
            acc.prev = Some(latest);
        }
        let bytes_out_room: u64 = rooms.iter().map(|r| r.shipped_bytes).sum();
        // Total metric-channel drops: room (cumulative, latest per room) +
        // registry (cumulative, latest) + connection actors (delta, summed).
        let metrics_dropped = rooms
            .iter()
            .map(|r| r.metrics_dropped)
            .sum::<u64>()
            .saturating_add(self.registry.map(|r| r.metrics_dropped).unwrap_or(0))
            .saturating_add(self.conn_metrics_dropped);
        // Per-connection input-drop attribution: worst offenders first
        // (count desc, connection id asc as the deterministic tie-break).
        // The cumulative total folds in the drops RETIRED with closed
        // connections, so `net.actions_dropped` stays monotonic even
        // though the map only names live connections.
        let actions_dropped_total: u64 = self
            .conn_actions_dropped
            .values()
            .sum::<u64>()
            .saturating_add(self.conn_actions_dropped_retired);
        let mut actions_dropped_top: Vec<(ConnectionId, u64)> = self
            .conn_actions_dropped
            .iter()
            .map(|(c, n)| (*c, *n))
            .collect();
        actions_dropped_top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        actions_dropped_top.truncate(5);
        // Age the destroyed-room linger windows: each emitted report burns
        // one (see ROOM_GONE_GRACE_REPORTS); when a room's windows run
        // out, its accumulator finally goes.
        let expired: Vec<RoomId> = self
            .rooms_gone_grace
            .iter()
            .filter(|(_, left)| **left == 1)
            .map(|(id, _)| *id)
            .collect();
        for left in self.rooms_gone_grace.values_mut() {
            *left -= 1;
        }
        self.rooms_gone_grace.retain(|_, left| *left > 0);
        for id in expired {
            self.rooms.remove(&id);
        }
        MetricReport {
            metrics_dropped,
            registry: self.registry.map(|r| RegistryReport {
                rooms: r.rooms,
                conns: r.conns,
                rooms_created: r.rooms_created,
                rooms_destroyed: r.rooms_destroyed,
                rooms_died: r.rooms_died,
                joins: r.joins,
                leaves: r.leaves,
                opens: r.opens,
                closes: r.closes,
            }),
            // (registry report intentionally carries no metrics_dropped: the
            // registry's drop count is cumulative in its sample and is folded
            // into the top-level `metrics_dropped` above.)
            net: NetReport {
                bytes_in: self.conn_bytes_in,
                bytes_out_room,
                bytes_out_control: self.conn_bytes_out,
                bytes_out_total: bytes_out_room.saturating_add(self.conn_bytes_out),
                frames_in: self.conn_frames_in,
                frames_out: self.conn_frames_out,
                actions_dropped: actions_dropped_total,
                violations: self.conn_violations,
            },
            actions_dropped_top,
            rooms,
        }
    }
}

impl MetricReport {
    /// Render as one parseable `key=value` line per scope (stable format:
    /// the load generator and operators grep these).
    pub fn render(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(r) = &self.registry {
            lines.push(format!(
                "gsb-metric scope=registry rooms={} conns={} opens={} closes={} \
                 joins={} leaves={} rooms_created={} rooms_destroyed={} rooms_died={}",
                r.rooms, r.conns, r.opens, r.closes, r.joins, r.leaves,
                r.rooms_created, r.rooms_destroyed, r.rooms_died
            ));
        }
        for r in &self.rooms {
            lines.push(format!(
                "gsb-metric scope=room id={} steps={} hz={:.2} \
                 step_budget_us={} step_min_us={} step_mean_us={:.1} step_max_us={} \
                 step_hist=[{}] \
                 late_min_us={} late_mean_us={:.1} late_max_us={} \
                 lagged_events={} lagged_ticks={} dropped={} dropped_s={:.1} \
                 dropped_actions={} keepalive_resends={} snapshots={} \
                 snap_bytes_s={:.0} snap_bytes_max={} snap_overflows={} \
                 snap_records={} \
                 shipped_bytes={} shipped_s={:.0} \
                 groups={} members={} max_group={} joins={} leaves={} \
                 req_local={} req_ext={} \
                 req_rej_malformed={} req_rej_dup={} req_rej_no_handler={} \
                 req_rej_logic={} req_rej_conn={} req_rej_room={} \
                 req_to={} req_late={} \
                 req_pending={} metrics_dropped={}",
                r.room, r.steps, r.hz, r.budget_us,
                r.step_min_us, r.step_mean_us, r.step_max_us,
                r.step_hist
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
                r.late_min_us, r.late_mean_us, r.late_max_us,
                r.lagged_events, r.lagged_ticks, r.dropped, r.dropped_s,
                r.dropped_actions, r.keepalive_resends, r.snapshots,
                r.snap_bytes_s, r.snap_bytes_max, r.snap_overflows,
                r.snap_records,
                r.shipped_bytes, r.shipped_s,
                r.groups, r.members, r.max_group, r.joins, r.leaves,
                r.requests_local, r.requests_external,
                r.requests_rejected_malformed, r.requests_rejected_dup,
                r.requests_rejected_no_handler, r.requests_rejected_logic,
                r.requests_rejected_conn_cap, r.requests_rejected_room_cap,
                r.requests_timed_out, r.requests_late, r.pending_requests,
                r.metrics_dropped
            ));
        }
        let n = &self.net;
        lines.push(format!(
            "gsb-metric scope=net bytes_in={} bytes_out_room={} \
             bytes_out_control={} bytes_out_total={} frames_in={} frames_out={} \
             actions_dropped={} violations={} metrics_dropped={}",
            n.bytes_in, n.bytes_out_room, n.bytes_out_control,
            n.bytes_out_total, n.frames_in, n.frames_out, n.actions_dropped,
            n.violations, self.metrics_dropped
        ));
        if !self.actions_dropped_top.is_empty() {
            // Attribution of the net-scope `actions_dropped`: which
            // connection's own input was lost to its full action channel
            // (worst first; "c<id>:<count>", comma-joined).
            lines.push(format!(
                "gsb-metric scope=net actions_dropped_top={}",
                self.actions_dropped_top
                    .iter()
                    .map(|(c, n)| format!("{c}:{n}"))
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
        lines
    }
}

/// Where reports go. Both sinks are message-passing: the collector never
/// shares its accumulator.
pub enum MetricSink {
    /// One `tracing::info!` line per rendered report line (visible under
    /// `RUST_LOG=info`; silent where no subscriber is installed, e.g. in
    /// most tests).
    Log,
    /// Send each [`MetricReport`] to a channel (programmatic consumers:
    /// the load generator, tests).
    Channel(mpsc::UnboundedSender<MetricReport>),
}

/// The metrics collector task.
///
/// One awaited source (a subscription to the global ticker's broadcast —
/// the same channel the rooms use), no `select!`, no shared state: on
/// each tick it drains the event channel with `try_recv` (synchronous)
/// and, at most once per `period`, emits a report through its sink. When
/// the ticker closes (shutdown), it emits one final report and exits.
pub struct MetricsCollector {
    ticks: broadcast::Receiver<TickInfo>,
    /// Bounded (see module docs: bounded + `try_send` producers with a drop
    /// counter); the collector drains it with `try_recv` on every tick.
    rx: mpsc::Receiver<MetricsEvent>,
    acc: MetricAccumulator,
    sink: MetricSink,
    period: Duration,
    next_report: Instant,
}

impl MetricsCollector {
    pub fn new(
        ticks: broadcast::Receiver<TickInfo>,
        rx: mpsc::Receiver<MetricsEvent>,
        sink: MetricSink,
        period: Duration,
    ) -> Self {
        Self {
            ticks,
            rx,
            acc: MetricAccumulator::default(),
            sink,
            period,
            next_report: Instant::now() + period,
        }
    }

    /// Run until the ticker closes (one final report is emitted).
    pub async fn run(mut self) {
        loop {
            // The collector's only awaited source: the ticker broadcast.
            // `Lagged` is irrelevant here (we do not index ticks); we just
            // resynchronize with the next one.
            let closed = matches!(
                self.ticks.recv().await,
                Err(broadcast::error::RecvError::Closed)
            );
            let now = Instant::now();
            while let Ok(ev) = self.rx.try_recv() {
                self.acc.apply(ev);
            }
            if now >= self.next_report {
                self.emit(now);
                self.next_report = now + self.period;
            }
            if closed {
                break;
            }
        }
        self.emit(Instant::now());
    }

    fn emit(&mut self, at: Instant) {
        let report = self.acc.report(at);
        match &self.sink {
            MetricSink::Log => {
                for line in report.render() {
                    tracing::info!(%line, "gsb-metric");
                }
            }
            MetricSink::Channel(tx) => {
                // Synchronous send: the collector is a plain task, not an
                // actor under the single-`await` discipline; a closed
                // receiver (consumer gone) is ignored.
                let _ = tx.send(report);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hist_index_binning() {
        // Budget-relative bins (budget = 1024 µs ⇒ clean integer edges
        // 8,16,32,64,128,256,512,1024,2048,4096,8192,16384,32768):
        // [0,8) [8,16) [16,32) [32,64) [64,128) [128,256) [256,512)
        // [512,1024) [1024,2048) [2048,4096) [4096,8192) [8192,16384)
        // [16384,32768) [32768,∞).
        const B: u64 = 1024;
        assert_eq!(hist_index(B, 0), 0);
        assert_eq!(hist_index(B, 7), 0);
        assert_eq!(hist_index(B, 8), 1); // 1/128× budget
        assert_eq!(hist_index(B, 15), 1);
        assert_eq!(hist_index(B, 16), 2); // 1/64× budget
        assert_eq!(hist_index(B, 511), 6); // 1/2×, under budget
        assert_eq!(hist_index(B, 1023), 7); // just under budget
        // The tick budget itself is the overflow boundary.
        assert_eq!(hist_index(B, 1024), HIST_OVERFLOW_BIN);
        assert_eq!(hist_index(B, 2047), HIST_OVERFLOW_BIN); // 1-2× budget
        assert_eq!(hist_index(B, 2048), 9); // 2× budget
        assert_eq!(hist_index(B, 4096), 10); // 4× budget
        assert_eq!(hist_index(B, 32768), 13); // 32× budget → top bin
        assert_eq!(hist_index(B, u64::MAX), HIST_BINS - 1);

        // The spec's own example: at a 30 Hz budget (33 333 µs) a 5.1 ms step
        // (inside the budget) and a 40 ms step (over it) must land in
        // *different* bins, and the 40 ms one must be readable as overflow.
        assert_ne!(hist_index(33_333, 5_100), hist_index(33_333, 40_000));
        assert_eq!(hist_index(33_333, 40_000), HIST_OVERFLOW_BIN);
        assert!(hist_index(33_333, 5_100) < HIST_OVERFLOW_BIN);
    }

    /// The fine histogram resolves sub-budget changes the log2 one cannot:
    /// a 10% step-time difference at ~390 µs is TWO different fine p50
    /// values, while both are the SAME log2 bin (both report the same
    /// coarse "~391" midpoint).
    #[test]
    fn fine_hist_resolves_ten_percent_difference() {
        let mut a = [0u64; FINE_HIST_BINS];
        let mut b = [0u64; FINE_HIST_BINS];
        a[fine_hist_index(390).unwrap()] = 1000;
        b[fine_hist_index(430).unwrap()] = 1000; // +10.3% step time
        let pa = fine_hist_percentile_us(&a, 1000, 50).unwrap();
        let pb = fine_hist_percentile_us(&b, 1000, 50).unwrap();
        assert_eq!(pa, 390 / 8 * 8); // bin lower edge
        assert_eq!(pb, 430 / 8 * 8);
        assert_ne!(pa, pb, "the fine histogram must separate a 10% change");
        // ...and the log2 histogram does NOT (same bin at a 30 Hz budget).
        let lo_a = hist_index(33_333, 390);
        let lo_b = hist_index(33_333, 430);
        assert_eq!(lo_a, lo_b, "sanity: the log2 bins are 2× apart here");
    }

    /// Known distributions → exact percentiles (integer arithmetic; the
    /// answer is the bin lower edge, so the error is < FINE_HIST_US_PER_BIN).
    #[test]
    fn fine_hist_percentiles_known_distributions() {
        // Uniform over [0, 4096): 2 steps per bin, 1000 total.
        let mut u = [0u64; FINE_HIST_BINS];
        for bin in u.iter_mut() {
            *bin = 2;
        }
        // The 500th of 1000 steps: bin 249 (500 steps in bins 0..=249).
        assert_eq!(fine_hist_percentile_us(&u, 1000, 50), Some(249 * 8));
        assert_eq!(fine_hist_percentile_us(&u, 1000, 99), Some(989 / 2 * 8));
        // Bimodal: 50% at 390 µs, 50% at 780 µs (the measured clusters).
        let mut m = [0u64; FINE_HIST_BINS];
        m[fine_hist_index(390).unwrap()] = 500;
        m[fine_hist_index(780).unwrap()] = 500;
        assert_eq!(
            fine_hist_percentile_us(&m, 1000, 50),
            Some(390 / 8 * 8),
            "p50: the 500th step is the last one in the 390 cluster"
        );
        assert_eq!(
            fine_hist_percentile_us(&m, 1000, 99),
            Some(780 / 8 * 8),
            "p99: inside the 780 cluster"
        );
        // Degenerate: all one value.
        let mut d = [0u64; FINE_HIST_BINS];
        d[fine_hist_index(1234).unwrap()] = 77;
        assert_eq!(fine_hist_percentile_us(&d, 77, 1), Some(1234 / 8 * 8));
        assert_eq!(fine_hist_percentile_us(&d, 77, 100), Some(1234 / 8 * 8));
    }

    /// Cap semantics: steps at/above FINE_HIST_CAP_US are absent from the
    /// fine histogram (the log2 histogram keeps the overflow signal), and a
    /// percentile whose rank falls beyond the cap reports `None`.
    #[test]
    fn fine_hist_cap_and_overflow() {
        assert_eq!(fine_hist_index(0), Some(0));
        assert_eq!(fine_hist_index(FINE_HIST_CAP_US - 1), Some(FINE_HIST_BINS - 1));
        assert_eq!(fine_hist_index(FINE_HIST_CAP_US), None);
        assert_eq!(fine_hist_index(u64::MAX), None);

        // 100 steps below the cap (last fine bin), 50 at/above it: the p50
        // is in the fine range, the p99 is not.
        let mut h = [0u64; FINE_HIST_BINS];
        h[FINE_HIST_BINS - 1] = 100;
        assert_eq!(
            fine_hist_percentile_us(&h, 150, 50),
            Some((FINE_HIST_BINS - 1) as u64 * FINE_HIST_US_PER_BIN)
        );
        assert_eq!(fine_hist_percentile_us(&h, 150, 99), None);

        // Empty / out-of-range p.
        let z = [0u64; FINE_HIST_BINS];
        assert_eq!(fine_hist_percentile_us(&z, 0, 50), None);
        assert_eq!(fine_hist_percentile_us(&h, 150, 0), None);
        assert_eq!(fine_hist_percentile_us(&h, 150, 101), None);
    }

    /// The accumulator applies all three event kinds and rates are
    /// delta-over-period between two reports.
    #[test]
    fn accumulator_applies_events_and_computes_rates() {
        let mut acc = MetricAccumulator::default();
        let t0 = Instant::now();

        let room0 = RoomSample {
            room: RoomId(1),
            emit_at: t0,
            steps: 30,
            budget_us: 33,
            dropped_frames: 2,
            snap_bytes: 3_000,
            shipped_bytes: 30_000,
            lagged_events: 0,
            lagged_ticks: 0,
            step_min_us: 8,
            step_max_us: 900,
            step_sum_us: 420,
            step_hist: [20, 5, 4, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            step_fine_hist: [0; FINE_HIST_BINS],
            late_min_us: 1,
            late_max_us: 2_000,
            late_sum_us: 300,
            dropped_actions: 0,
            keepalive_resends: 1,
            snapshots: 29,
            snap_bytes_max: 796,
            snap_overflows: 0,
            snap_records: 29,
            shipped_frames: 30,
            private_frames: 0,
            joins: 2,
            leaves: 0,
            requests_local: 0,
            requests_external: 0,
            requests_rejected_malformed: 0,
            requests_rejected_dup: 0,
            requests_rejected_no_handler: 0,
            requests_rejected_logic: 0,
            requests_rejected_conn_cap: 0,
            requests_rejected_room_cap: 0,
            requests_timed_out: 0,
            requests_late: 0,
            pending_requests: 0,
            groups: 1,
            members: 3,
            max_group: 3,
            metrics_dropped: 0,
        };
        acc.apply(MetricsEvent::Room(room0));
        acc.apply(MetricsEvent::Registry(RegistrySample {
            rooms: 1,
            conns: 3,
            rooms_created: 1,
            rooms_destroyed: 0,
            rooms_died: 0,
            joins: 3,
            leaves: 0,
            opens: 3,
            closes: 0,
            metrics_dropped: 1,
        }));
        acc.apply(MetricsEvent::Conn(ConnSample {
            conn: ConnectionId(1),
            bytes_in: 100,
            bytes_out: 20,
            frames_in: 5,
            frames_out: 2,
            actions_dropped: 0,
            metrics_dropped: 2,
            violations: 0,
            last: false,
        }));

        let first = acc.report(t0);
        assert_eq!(first.rooms.len(), 1);
        // First report: no previous window yet.
        assert_eq!(first.rooms[0].hz, 0.0);

        // Advance one second: 30 more steps, 2 more drops, more bytes. The
        // rate is over the sample interval (`emit_at`), so the second
        // sample carries `t1 = t0 + 1s`.
        let t1 = t0 + Duration::from_secs(1);
        acc.apply(MetricsEvent::Room(RoomSample {
            emit_at: t1,
            steps: 60,
            dropped_frames: 4,
            snap_bytes: 6_000,
            shipped_bytes: 60_000,
            ..room0
        }));
        acc.apply(MetricsEvent::Conn(ConnSample {
            conn: ConnectionId(1),
            bytes_in: 50,
            bytes_out: 10,
            frames_in: 2,
            frames_out: 1,
            actions_dropped: 7,
            metrics_dropped: 0,
            violations: 3,
            last: true,
        }));

        let second = acc.report(t1);
        let r = &second.rooms[0];
        // Total metric-channel drops: room (0) + registry (1) + conn deltas
        // (2 + 0) = 3.
        assert_eq!(second.metrics_dropped, 3);
        assert!((r.hz - 30.0).abs() < 1e-6, "hz from delta over 1 s");
        assert!((r.dropped_s - 2.0).abs() < 1e-6, "drop rate from delta");
        assert!((r.shipped_s - 30_000.0).abs() < 1e-6, "shipped rate from delta");
        assert!((r.snap_bytes_s - 3_000.0).abs() < 1e-6, "encode rate from delta");
        assert_eq!(r.steps, 60);
        assert_eq!(r.step_mean_us, 420.0 / 60.0);
        assert_eq!(r.members, 3);
        assert_eq!(second.registry.unwrap().conns, 3);
        assert_eq!(second.net.bytes_in, 150);
        assert_eq!(second.net.bytes_out_control, 30);
        assert_eq!(second.net.bytes_out_room, 60_000);
        assert_eq!(second.net.bytes_out_total, 60_030);

        // Per-connection input-drop attribution: the only dropping sender
        // is c1 (7 actions total). Its sample carried `last: true`, so
        // the drops fold into the cumulative net total (monotonic) and
        // the per-connection entry retires with the connection — a
        // closed peer cannot flood again, so it leaves the top list.
        assert_eq!(second.net.actions_dropped, 7);
        assert!(
            second.actions_dropped_top.is_empty(),
            "a closed connection's attribution is retired, not listed"
        );
        // Violation events sum across the conn samples (0 + 3).
        assert_eq!(second.net.violations, 3);

        // The render is one line per scope and parseable key=value. No
        // attribution line: nothing was dropped by a LIVE connection.
        let lines = second.render();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("gsb-metric scope=registry "));
        assert!(lines[1].starts_with("gsb-metric scope=room id=r1 "));
        assert!(lines[2].starts_with("gsb-metric scope=net "));
        assert!(lines[2].contains("actions_dropped=7"));
        for line in &lines {
            for kv in line.split_whitespace().skip(2) {
                assert!(kv.contains('='), "key=value field: {kv}");
            }
        }
    }

    /// A minimal `RoomSample` for the pruning tests below (all counters
    /// zero except `steps`).
    fn room_sample(room: RoomId, emit_at: Instant, steps: u64) -> RoomSample {
        RoomSample {
            room,
            emit_at,
            steps,
            budget_us: 33_333,
            dropped_frames: 0,
            snap_bytes: 0,
            shipped_bytes: 0,
            lagged_events: 0,
            lagged_ticks: 0,
            step_min_us: 0,
            step_max_us: 0,
            step_sum_us: 0,
            step_hist: [0; HIST_BINS],
            step_fine_hist: [0; FINE_HIST_BINS],
            late_min_us: 0,
            late_max_us: 0,
            late_sum_us: 0,
            dropped_actions: 0,
            keepalive_resends: 0,
            snapshots: 0,
            snap_bytes_max: 0,
            snap_overflows: 0,
            snap_records: 0,
            shipped_frames: 0,
            private_frames: 0,
            joins: 0,
            leaves: 0,
            requests_local: 0,
            requests_external: 0,
            requests_rejected_malformed: 0,
            requests_rejected_dup: 0,
            requests_rejected_no_handler: 0,
            requests_rejected_logic: 0,
            requests_rejected_conn_cap: 0,
            requests_rejected_room_cap: 0,
            requests_timed_out: 0,
            requests_late: 0,
            pending_requests: 0,
            groups: 0,
            members: 0,
            max_group: 0,
            metrics_dropped: 0,
        }
    }

    /// Table-prune lock 3a — a destroyed room's accumulator is dropped
    /// when its grace windows run down instead of living forever; its
    /// final measured window still reaches the reports (the destroy can
    /// land between the room's last sample and the next report), the
    /// dying producers' stragglers do NOT refresh it, and once the
    /// windows burn down a re-created id reports normally again.
    #[test]
    fn destroyed_room_accumulator_is_pruned_and_stragglers_suppressed() {
        let mut acc = MetricAccumulator::default();
        let t = Instant::now();

        // Live room → reported.
        acc.apply(MetricsEvent::Room(room_sample(RoomId(3), t, 30)));
        assert_eq!(acc.report(t).rooms.len(), 1);

        // Destroyed → lingers: the notice lands after the room's last
        // sample, so this report must still carry its final window.
        acc.apply(MetricsEvent::RoomGone(RoomId(3)));
        let r = acc.report(t); // burns linger window 2 → 1
        assert_eq!(r.rooms.len(), 1, "the final measured window survives");
        assert_eq!(r.rooms[0].steps, 30);

        // The dead incarnation's shutdown-tick straggler (it lost the
        // race against the notice on the shared FIFO) neither refreshes
        // nor resurrects the entry.
        acc.apply(MetricsEvent::Room(room_sample(RoomId(3), t, 31)));
        let r = acc.report(t); // burns the last window 1 → 0: dropped
        assert_eq!(r.rooms.len(), 1, "still inside the linger window");
        assert_eq!(r.rooms[0].steps, 30, "frozen: straggler not applied");

        // Windows ran down: the accumulator is gone, and a further
        // sample is treated as a fresh incarnation's first (destroy →
        // re-create works; counters restart from zero by contract).
        acc.apply(MetricsEvent::Room(room_sample(RoomId(3), t, 32)));
        let r = acc.report(t);
        assert_eq!(r.rooms.len(), 1);
        assert_eq!(r.rooms[0].steps, 32, "the id is reusable after the linger");
    }

    /// Table-prune lock 3b — a closed connection's `conn_actions_dropped`
    /// entry retires at its final flush (`last: true`), keeping the map
    /// live-connections-only, while the cumulative net total stays
    /// monotonic (retired drops fold into it).
    #[test]
    fn closing_connection_retires_its_actions_dropped_entry() {
        let mut acc = MetricAccumulator::default();
        let c1 = ConnectionId(1);
        let c2 = ConnectionId(2);

        // Two live-flush deltas accumulate under c1 and show in top.
        for delta in [3u64, 2] {
            acc.apply(MetricsEvent::Conn(ConnSample {
                conn: c1,
                bytes_in: 0,
                bytes_out: 0,
                frames_in: 0,
                frames_out: 0,
                actions_dropped: delta,
                metrics_dropped: 0,
                violations: 0,
                last: false,
            }));
        }
        let r = acc.report(Instant::now());
        assert_eq!(r.net.actions_dropped, 5);
        assert_eq!(r.actions_dropped_top, vec![(c1, 5)]);

        // The final flush may itself carry a last delta; everything folds
        // into the cumulative total and the entry retires.
        acc.apply(MetricsEvent::Conn(ConnSample {
            conn: c1,
            bytes_in: 0,
            bytes_out: 0,
            frames_in: 0,
            frames_out: 0,
            actions_dropped: 1,
            metrics_dropped: 0,
            violations: 0,
            last: true,
        }));
        // A different connection keeps dropping afterwards.
        acc.apply(MetricsEvent::Conn(ConnSample {
            conn: c2,
            bytes_in: 0,
            bytes_out: 0,
            frames_in: 0,
            frames_out: 0,
            actions_dropped: 4,
            metrics_dropped: 0,
            violations: 0,
            last: false,
        }));
        let r = acc.report(Instant::now());
        assert_eq!(
            r.net.actions_dropped,
            10,
            "cumulative total includes retired drops (5 + 1 + 4)"
        );
        assert_eq!(
            r.actions_dropped_top,
            vec![(c2, 4)],
            "only the live dropper is attributed"
        );

        // Idempotent close (no double-retire): a stray second `last`
        // sample changes nothing.
        acc.apply(MetricsEvent::Conn(ConnSample {
            conn: c1,
            bytes_in: 0,
            bytes_out: 0,
            frames_in: 0,
            frames_out: 0,
            actions_dropped: 0,
            metrics_dropped: 0,
            violations: 0,
            last: true,
        }));
        let r = acc.report(Instant::now());
        assert_eq!(r.net.actions_dropped, 10);
        assert_eq!(r.actions_dropped_top, vec![(c2, 4)]);
    }

    /// Test room logic for the flow test below: one byte per tick per
    /// group (declares "changed" every tick — a permitted, if wasteful,
    /// logic) so the fan-out runs and a full out channel produces drops.
    struct AlwaysLogic;

    impl crate::room::RoomLogic<()> for AlwaysLogic {
        type GroupKey = ();
        fn snapshot_op(&self) -> u16 {
            0x7200
        }
        fn private_op(&self) -> u16 {
            0x7201
        }
        fn group_of(&self, _w: &(), _c: ConnectionId) -> Self::GroupKey {}
        fn snapshot(
            &mut self,
            _w: &mut (),
            _c: &crate::room::TickCtx,
            _g: &Self::GroupKey,
            out: &mut bytes::BytesMut,
        ) -> bool {
            out.extend_from_slice(b"x");
            true
        }
        fn on_join(&mut self, _w: &mut (), _c: ConnectionId) -> crate::id::EntityId {
            1
        }
        fn on_leave(&mut self, _w: &mut (), _c: ConnectionId) {}
        fn ingest(&mut self, _w: &mut (), _c: &crate::room::TickCtx, a: &mut Vec<crate::room::Action>) {
            a.clear();
        }
        fn update(&mut self, _w: &mut (), _c: &crate::room::TickCtx) {}
    }

    /// The full path the spec asks for: counters live in the room actor's
    /// local state, leave it over the (bounded) metrics channel via
    /// `try_send`, and are accumulated by the collector task — asserted here
    /// from the collector's reports, i.e. from outside the actor.
    #[tokio::test]
    async fn room_counters_flow_to_collector() {
        use crate::channel::{FrameBatch, Mailbox, channel};
        use crate::room::{Action, RoomActor, RoomConfig, RoomControl};
        use tokio::sync::oneshot;

        // One manual broadcast feeds both the room and the collector's
        // clock (the collector's only awaited source). A feed task keeps
        // ticks flowing in real time (100 Hz), exactly like the real
        // ticker — the collector only wakes on ticks, so the stream must
        // outlive the report window.
        let (tick_tx, _first) = broadcast::channel(64);
        let feed_task = {
            let tx = tick_tx.clone();
            tokio::spawn(async move {
                let mut tick = 1u64;
                loop {
                    tx.send(TickInfo {
                        tick,
                        at: Instant::now(),
                    })
                    .ok();
                    tick += 1;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
        };
        let (m_tx, m_rx) = mpsc::channel::<MetricsEvent>(64);
        let (rep_tx, mut rep_rx) = mpsc::unbounded_channel::<MetricReport>();
        tokio::spawn(
            MetricsCollector::new(
                tick_tx.subscribe(),
                m_rx,
                MetricSink::Channel(rep_tx),
                Duration::from_millis(100),
            )
            .run(),
        );

        // The room: out channel capacity 2 and NO consumer, so after the
        // first batches the fan-out's try_send fails and `dropped` grows
        // — an actor-local counter that must reach the collector.
        // `metrics_cadence_hz = tick_hz` ⇒ sample every step, so this test's
        // ~100 ms report window is guaranteed to carry room samples (the
        // default 1 Hz cadence would first emit on step 30).
        let config = RoomConfig {
            id: RoomId(1),
            metrics_cadence_hz: 30.0,
            ..Default::default()
        };
        let (control, control_rx) = channel(config.control_capacity);
        let actor = RoomActor::new(
            config,
            (),
            Box::new(AlwaysLogic),
            tick_tx.subscribe(),
            control_rx,
            1,
            m_tx,
            None,
        );
        let room = tokio::spawn(actor.run());

        // Join one connection (its out channel is the room's fan-out target).
        let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(2);
        let (reply_tx, reply_rx) = oneshot::channel::<
            Result<(crate::id::EntityId, Mailbox<Action>), crate::error::CoreError>,
        >();
        control
            .send(RoomControl::Join {
                conn: ConnectionId(7),
                out: out_tx,
                reply: reply_tx,
            })
            .await
            .expect("control accepts");

        // Wait out two report periods (100 ms each): the first report
        // carries the room's state, the second proves the rate window
        // (Δ over period) works.
        tokio::time::sleep(Duration::from_millis(250)).await;
        let first = tokio::time::timeout(Duration::from_secs(3), rep_rx.recv())
            .await
            .expect("collector produced a report")
            .expect("report channel open");
        let r = first
            .rooms
            .iter()
            .find(|r| r.room == RoomId(1))
            .expect("report carries the room");
        assert_eq!(r.members, 1, "the join reached the room AND the report");
        assert_eq!(r.joins, 1);
        assert_eq!(r.groups, 1);
        assert_eq!(r.max_group, 1);
        assert!(r.steps >= 4, "room stepped on the fed ticks: {} steps", r.steps);
        assert!(r.snapshots > 0, "snapshots were encoded and counted");
        assert!(
            r.dropped > 0,
            "the unconsumed out channel must have produced drops, visible outside the actor"
        );
        assert!(r.step_max_us > 0, "step duration was measured");
        assert_eq!(r.step_hist.iter().sum::<u64>(), r.steps);
        // The join reply proves the room is alive and processing.
        tokio::time::timeout(Duration::from_secs(3), reply_rx)
            .await
            .expect("join reply")
            .expect("join reply dropped")
            .expect("join accepted (room not full)");

        // One more report period: the next report's rates are over the
        // 100 ms window since the previous report.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let second = tokio::time::timeout(Duration::from_secs(3), rep_rx.recv())
            .await
            .expect("collector produced a second report")
            .expect("report channel open");
        let r2 = second
            .rooms
            .iter()
            .find(|r| r.room == RoomId(1))
            .expect("second report carries the room");
        assert!(
            r2.steps > r.steps,
            "more steps in the second window: {} > {}",
            r2.steps,
            r.steps
        );
        assert!(
            r2.dropped >= r.dropped,
            "cumulative dropped never decreases"
        );
        // Net/registry scopes are absent here (no registry/conn actors in
        // this test): the render still has exactly one line (the room).
        assert_eq!(second.render().len(), 2); // room line + net line

        // Shutdown: abort the feed and drop the tick sender (closes the
        // broadcast); the room exits on Closed and the collector emits
        // one final report.
        feed_task.abort();
        drop(tick_tx);
        tokio::time::timeout(Duration::from_secs(3), room)
            .await
            .expect("room exited on closed ticker")
            .expect("room task panicked");
    }
}
