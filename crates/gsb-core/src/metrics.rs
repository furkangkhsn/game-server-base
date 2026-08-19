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
//! **Why an unbounded channel, and why this keeps the room's discipline:**
//! a room's tick body must stay fully synchronous and the room's only
//! `await` must stay `tick_rx.recv()`. A *bounded* `mpsc::Sender::send`
//! is a future — awaiting it would add an await to the tick loop (the
//! sender could park when the collector lags), which is exactly what the
//! tick-architecture constraint forbids. An *unbounded* sender's `send`
//! is a plain synchronous function (it cannot park: there is no
//! capacity), so emitting a sample costs one `Vec`-push into the channel
//! and nothing else. Backpressure is a non-issue by design: a sample is
//! a fixed-size value sent at most once per step per room (a few Hz to
//! tens of Hz, rooms are few), so the channel cannot grow with load. If a
//! producer is ever so slow the collector drains faster than it fills, the
//! cost is a few lost samples, never a stalled tick.
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
//!   body duration min/mean/max + a fixed 7-bin histogram (µs);
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
//! Rates (`hz`, `*_s`) are computed by the collector as deltas between
//! consecutive reports over the cumulative counters — the actors send
//! cumulative samples and current gauges, nothing else.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc};

use crate::id::{ConnectionId, RoomId};
use crate::ticker::TickInfo;

/// Histogram edges (µs) for the per-step body duration:
/// bins are `[0,50) [50,100) [100,250) [250,500) [500,1000) [1000,5000)
/// [5000,∞)`.
pub const HIST_EDGES_US: [u64; 6] = [50, 100, 250, 500, 1_000, 5_000];
/// Number of histogram bins (`HIST_EDGES_US.len() + 1`).
pub const HIST_BINS: usize = HIST_EDGES_US.len() + 1;

/// Bin index for a step body duration in µs (see [`HIST_EDGES_US`]).
#[inline]
pub fn hist_index(us: u64) -> usize {
    let mut i = 0;
    while i < HIST_EDGES_US.len() && us >= HIST_EDGES_US[i] {
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
    /// Steps run (cumulative).
    pub steps: u64,
    /// Broadcast `Lagged` occurrences / total missed tick indices (the
    /// room fell behind the ticker's buffer; each one is caught up via
    /// the wall-clock `dt` of the next step).
    pub lagged_events: u64,
    pub lagged_ticks: u64,
    /// Step body duration, µs: min / max / sum (mean = sum / steps).
    pub step_min_us: u64,
    pub step_max_us: u64,
    pub step_sum_us: u64,
    /// Step body duration histogram (cumulative; [`HIST_EDGES_US`]).
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
    /// True on the actor's final flush (connection closing).
    pub last: bool,
}

/// A metrics event: one actor's sample, in flight over the metrics
/// channel.
#[derive(Debug, Clone, Copy)]
pub enum MetricsEvent {
    Room(RoomSample),
    Registry(RegistrySample),
    Conn(ConnSample),
}

/// Per-room state in the accumulator: the latest sample plus the sample
/// captured at the previous report (for rate computation).
#[derive(Debug)]
struct RoomAcc {
    latest: RoomSample,
    /// (sample, when it was captured) at the previous report; `None`
    /// before the first report.
    prev: Option<(RoomSample, Instant)>,
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
}

/// Per-room slice of a report: current gauges + rates over the last
/// report period (rates are 0.0 until the room has reported twice).
#[derive(Debug, Clone, Copy)]
pub struct RoomReport {
    pub room: RoomId,
    /// Steps (cumulative) and measured rate (Δsteps/s since the previous
    /// report). Compare against the room's configured `tick_hz`: the room
    /// is on rate when these agree.
    pub steps: u64,
    pub hz: f64,
    pub step_min_us: u64,
    pub step_mean_us: f64,
    pub step_max_us: u64,
    /// Cumulative step duration histogram ([`HIST_EDGES_US`]).
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
    /// Bytes shipped to clients (cumulative) and shipped-byte rate (Δ/s)
    /// — the server-side bytes-out for this room.
    pub shipped_bytes: u64,
    pub shipped_s: f64,
    pub groups: u32,
    pub members: u32,
    pub max_group: u32,
    pub joins: u64,
    pub leaves: u64,
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
}

/// One periodic report: the server's current numeric state.
#[derive(Debug, Clone)]
pub struct MetricReport {
    pub rooms: Vec<RoomReport>,
    pub registry: Option<RegistryReport>,
    pub net: NetReport,
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
            }
        }
    }

    /// Snapshot the accumulated state as a report and advance the rate
    /// window: per-room rates are Δ(since the previous report) / Δt.
    pub fn report(&mut self, at: Instant) -> MetricReport {
        let mut rooms = Vec::with_capacity(self.rooms.len());
        for (id, acc) in &mut self.rooms {
            let latest = acc.latest;
            let (hz, dropped_s, snap_bytes_s, shipped_s) = match acc.prev {
                Some((p, prev_at)) => {
                    let dt = at.saturating_duration_since(prev_at).as_secs_f64().max(1e-9);
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
                shipped_bytes: latest.shipped_bytes,
                shipped_s,
                groups: latest.groups,
                members: latest.members,
                max_group: latest.max_group,
                joins: latest.joins,
                leaves: latest.leaves,
            });
            acc.prev = Some((latest, at));
        }
        let bytes_out_room: u64 = rooms.iter().map(|r| r.shipped_bytes).sum();
        MetricReport {
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
            net: NetReport {
                bytes_in: self.conn_bytes_in,
                bytes_out_room,
                bytes_out_control: self.conn_bytes_out,
                bytes_out_total: bytes_out_room.saturating_add(self.conn_bytes_out),
                frames_in: self.conn_frames_in,
                frames_out: self.conn_frames_out,
            },
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
                 step_min_us={} step_mean_us={:.1} step_max_us={} step_hist=[{}] \
                 late_min_us={} late_mean_us={:.1} late_max_us={} \
                 lagged_events={} lagged_ticks={} dropped={} dropped_s={:.1} \
                 dropped_actions={} keepalive_resends={} snapshots={} \
                 snap_bytes_s={:.0} snap_bytes_max={} shipped_bytes={} shipped_s={:.0} \
                 groups={} members={} max_group={} joins={} leaves={}",
                r.room, r.steps, r.hz,
                r.step_min_us, r.step_mean_us, r.step_max_us,
                r.step_hist
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
                r.late_min_us, r.late_mean_us, r.late_max_us,
                r.lagged_events, r.lagged_ticks, r.dropped, r.dropped_s,
                r.dropped_actions, r.keepalive_resends, r.snapshots,
                r.snap_bytes_s, r.snap_bytes_max,
                r.shipped_bytes, r.shipped_s,
                r.groups, r.members, r.max_group, r.joins, r.leaves
            ));
        }
        let n = &self.net;
        lines.push(format!(
            "gsb-metric scope=net bytes_in={} bytes_out_room={} \
             bytes_out_control={} bytes_out_total={} frames_in={} frames_out={}",
            n.bytes_in, n.bytes_out_room, n.bytes_out_control,
            n.bytes_out_total, n.frames_in, n.frames_out
        ));
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
    rx: mpsc::UnboundedReceiver<MetricsEvent>,
    acc: MetricAccumulator,
    sink: MetricSink,
    period: Duration,
    next_report: Instant,
}

impl MetricsCollector {
    pub fn new(
        ticks: broadcast::Receiver<TickInfo>,
        rx: mpsc::UnboundedReceiver<MetricsEvent>,
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
        // Bins: [0,50) [50,100) [100,250) [250,500) [500,1000) [1000,5000)
        // [5000,∞).
        assert_eq!(hist_index(0), 0);
        assert_eq!(hist_index(49), 0);
        assert_eq!(hist_index(50), 1);
        assert_eq!(hist_index(99), 1);
        assert_eq!(hist_index(100), 2);
        assert_eq!(hist_index(249), 2);
        assert_eq!(hist_index(250), 3);
        assert_eq!(hist_index(499), 3);
        assert_eq!(hist_index(500), 4);
        assert_eq!(hist_index(999), 4);
        assert_eq!(hist_index(1_000), 5);
        assert_eq!(hist_index(4_999), 5);
        assert_eq!(hist_index(5_000), 6);
        assert_eq!(hist_index(u64::MAX), 6);
    }

    /// The accumulator applies all three event kinds and rates are
    /// delta-over-period between two reports.
    #[test]
    fn accumulator_applies_events_and_computes_rates() {
        let mut acc = MetricAccumulator::default();
        let t0 = Instant::now();

        let room0 = RoomSample {
            room: RoomId(1),
            steps: 30,
            dropped_frames: 2,
            snap_bytes: 3_000,
            shipped_bytes: 30_000,
            lagged_events: 0,
            lagged_ticks: 0,
            step_min_us: 8,
            step_max_us: 900,
            step_sum_us: 420,
            step_hist: [25, 4, 1, 0, 0, 0, 0],
            late_min_us: 1,
            late_max_us: 2_000,
            late_sum_us: 300,
            dropped_actions: 0,
            keepalive_resends: 1,
            snapshots: 29,
            snap_bytes_max: 796,
            shipped_frames: 30,
            private_frames: 0,
            joins: 2,
            leaves: 0,
            groups: 1,
            members: 3,
            max_group: 3,
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
        }));
        acc.apply(MetricsEvent::Conn(ConnSample {
            conn: ConnectionId(1),
            bytes_in: 100,
            bytes_out: 20,
            frames_in: 5,
            frames_out: 2,
            last: false,
        }));

        let first = acc.report(t0);
        assert_eq!(first.rooms.len(), 1);
        // First report: no previous window yet.
        assert_eq!(first.rooms[0].hz, 0.0);

        // Advance one second: 30 more steps, 2 more drops, more bytes.
        let t1 = t0 + Duration::from_secs(1);
        acc.apply(MetricsEvent::Room(RoomSample {
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
            last: true,
        }));

        let second = acc.report(t1);
        let r = &second.rooms[0];
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

        // The render is one line per scope and parseable key=value.
        let lines = second.render();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("gsb-metric scope=registry "));
        assert!(lines[1].starts_with("gsb-metric scope=room id=r1 "));
        assert!(lines[2].starts_with("gsb-metric scope=net "));
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
    /// local state, leave it over the (unbounded) metrics channel, and are
    /// accumulated by the collector task — asserted here from the
    /// collector's reports, i.e. from outside the actor.
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
        let (m_tx, m_rx) = mpsc::unbounded_channel::<MetricsEvent>();
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
        let config = RoomConfig {
            id: RoomId(1),
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
        let (reply_tx, reply_rx) = oneshot::channel::<(crate::id::EntityId, Mailbox<Action>)>();
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
            .expect("join reply dropped");

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
