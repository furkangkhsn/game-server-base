//! The metrics path: actor-local counters, carried out over a channel.
//!
//! Nothing in this module — or anywhere in the architecture — is shared
//! mutable state. Counters live in the producing actor's own local state
//! (the room, the registry, each connection actor); an actor hands a
//! compact **sample** of them to the collector through an *unbounded*
//! `mpsc` channel, which is a mailbox exactly like every other actor
//! channel in this codebase (a transport, not shared state). The
//! collector accumulates the samples into its own task-local
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

/// A room's counter sample: cumulative counters + current gauges,
/// produced once per step (synchronous — see the module docs for why the
/// unbounded send adds no await to the tick loop).
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
}

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
    rooms: BTreeMap<RoomId, RoomAcc>,
    registry: Option<RegistrySample>,
    conn_bytes_in: u64,
    conn_bytes_out: u64,
    conn_frames_in: u64,
    conn_frames_out: u64,
    /// Summed delta of connection-actor metric-channel drops (their sample
    /// is delta-based, like the other conn counters).
    conn_metrics_dropped: u64,
    /// Cumulative input-action drops per connection (the sender's
    /// attribution: which connection's own input was lost to its full
    /// action channel). The collector owns this state — the connection
    /// actors only ever *report* their own deltas.
    conn_actions_dropped: BTreeMap<ConnectionId, u64>,
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
    /// (up to 5; ties broken by connection id). Empty when nothing was
    /// dropped — the only loss point for input is a connection's own full
    /// action channel, so this list names the flooders.
    pub actions_dropped_top: Vec<(ConnectionId, u64)>,
}

impl MetricAccumulator {
    /// Apply one event. Pure state transition (owned by one task).
    pub fn apply(&mut self, ev: MetricsEvent) {
        match ev {
            MetricsEvent::Room(s) => match self.rooms.get_mut(&s.room) {
                Some(acc) => acc.latest = s,
                None => {
                    self.rooms
                        .insert(s.room, RoomAcc { latest: s, prev: None });
                }
            },
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
        let actions_dropped_total: u64 = self.conn_actions_dropped.values().sum();
        let mut actions_dropped_top: Vec<(ConnectionId, u64)> = self
            .conn_actions_dropped
            .iter()
            .map(|(c, n)| (*c, *n))
            .collect();
        actions_dropped_top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        actions_dropped_top.truncate(5);
        MetricReport {
            metrics_dropped,
            registry: self.registry.map(|r| RegistryReport {
                rooms: r.rooms,
                conns: r.conns,
                rooms_created: r.rooms_created,
                rooms_destroyed: r.rooms_destroyed,
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
                 joins={} leaves={} rooms_created={} rooms_destroyed={}",
                r.rooms, r.conns, r.opens, r.closes, r.joins, r.leaves,
                r.rooms_created, r.rooms_destroyed
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
                 metrics_dropped={}",
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
                r.metrics_dropped
            ));
        }
        let n = &self.net;
        lines.push(format!(
            "gsb-metric scope=net bytes_in={} bytes_out_room={} \
             bytes_out_control={} bytes_out_total={} frames_in={} frames_out={} \
             actions_dropped={} metrics_dropped={}",
            n.bytes_in, n.bytes_out_room, n.bytes_out_control,
            n.bytes_out_total, n.frames_in, n.frames_out, n.actions_dropped,
            self.metrics_dropped
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
        // is c1 (7 actions, from the last:true sample's delta).
        assert_eq!(second.net.actions_dropped, 7);
        assert_eq!(
            second.actions_dropped_top,
            vec![(ConnectionId(1), 7)]
        );

        // The render is one line per scope and parseable key=value. The
        // net scope gets a second (attribution) line because input drops
        // are non-zero.
        let lines = second.render();
        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with("gsb-metric scope=registry "));
        assert!(lines[1].starts_with("gsb-metric scope=room id=r1 "));
        assert!(lines[2].starts_with("gsb-metric scope=net "));
        assert!(lines[2].contains("actions_dropped=7"));
        assert!(
            lines[3] == "gsb-metric scope=net actions_dropped_top=c1:7",
            "attribution line: {}",
            lines[3]
        );
        for line in &lines {
            for kv in line.split_whitespace().skip(2) {
                assert!(kv.contains('='), "key=value field: {kv}");
            }
        }
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
