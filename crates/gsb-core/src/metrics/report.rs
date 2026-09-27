//! The report the collector publishes: per-room, registry and network
//! views, plus the lookups a consumer needs to read one room out.

use std::time::{Duration, Instant};

use crate::id::{ConnectionId, RoomId};
use crate::metrics::*;

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
    /// Batches dropped on a full outbound channel (slow client —
    /// cumulative) and drop rate (Δ/s).
    pub dropped: u64,
    pub dropped_s: f64,
    /// Batches tried on an already closed outbound channel (the
    /// connection gone before the room processed its end — see
    /// `RoomSample::sends_closed`), cumulative. No rate: it is bounded by
    /// the connection ends, so its rate is the leave rate.
    pub sends_closed: u64,
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
    /// Frames shipped to clients (cumulative), and how many of them were
    /// PRIVATE (per-connection: RPC answers, acks, one-shot fulls).
    ///
    /// Not derivable from the bytes: `shipped_bytes / shipped_frames` is
    /// the mean frame size, and a datagram transport (rUDP) is bounded by
    /// PACKETS as well as by bytes — the same byte rate in twice as many
    /// frames is a different load. The private split separates
    /// per-connection traffic from the shared snapshot fan-out
    /// (`shipped_frames - private_frames` is the broadcast half), which
    /// is the encode-per-unit decision input `snap_records` answers from
    /// the encode side.
    pub shipped_frames: u64,
    pub private_frames: u64,
    pub groups: u32,
    pub members: u32,
    pub max_group: u32,
    pub joins: u64,
    pub leaves: u64,
    /// Detached-but-parked connections right now (gauge; their slots are
    /// held — see [`RoomSample::detached`]).
    pub detached: u32,
    pub resumes: u64,
    pub resume_rejected_stale: u64,
    pub detach_expired_despawn: u64,
    pub detach_expired_ai: u64,
    /// Holds forced to end by the detach-hold ceiling (see
    /// [`RoomSample::detach_forced`]).
    pub detach_forced: u64,
    /// Remote effects and migrations (shard rows only; see
    /// [`RoomSample::effects_applied`] and
    /// [`RoomSample::migrations_out`]).
    pub effects_applied: u64,
    pub effects_forwarded: u64,
    pub effects_orphaned: u64,
    pub effects_dropped: u64,
    pub effects_refused: u64,
    pub migrations_out: u64,
    pub migrations_in: u64,
    pub migrations_failed: u64,
    /// The team exchange (shard rows only; see
    /// [`RoomSample::team_exports`]).
    pub team_exports: u64,
    pub team_export_drops: u64,
    pub team_export_records: u64,
    pub team_over_cap: u64,
    pub team_over_budget: u64,
    pub team_imports: u64,
    pub team_import_records: u64,
    pub team_expired: u64,
    /// RPC (see `crate::rpc`), cumulative: room-local answers, delegated
    /// (pending) requests, rejections split by cause (see
    /// `RoomSample::requests_rejected_malformed`), the congested
    /// connections' unanswered refusals (see
    /// `RoomSample::requests_refused_congested`), the requests a session
    /// left unread (see `RoomSample::requests_dropped_unread`), timeout
    /// sweeps, and late reports dropped by the reconciliation.
    pub requests_local: u64,
    pub requests_external: u64,
    pub requests_rejected_malformed: u64,
    pub requests_rejected_dup: u64,
    pub requests_rejected_no_handler: u64,
    pub requests_rejected_logic: u64,
    pub requests_rejected_conn_cap: u64,
    pub requests_rejected_room_cap: u64,
    pub requests_refused_congested: u64,
    pub requests_dropped_unread: u64,
    /// Requests / plain actions the room dropped unprocessed (B54; see
    /// `RoomSample::requests_dropped_unbound` and
    /// `RoomSample::actions_dropped_unread`), cumulative.
    pub requests_dropped_unbound: u64,
    pub actions_dropped_unread: u64,
    pub actions_dropped_unbound: u64,
    pub requests_timed_out: u64,
    pub requests_late: u64,
    /// RPC answers discarded undelivered because their session ended, and
    /// external requests still in flight when it did (B53; see
    /// `RoomSample::requests_undelivered`), cumulative.
    pub requests_undelivered: u64,
    pub requests_abandoned: u64,
    /// RPC: external requests currently in flight (gauge).
    pub pending_requests: u32,
    /// Metric samples dropped on a full metrics channel (cumulative).
    pub metrics_dropped: u64,
    /// What the room/shard still held at its stop beyond its sessions
    /// (B68, [`StopCounts`]), from its final sample.
    pub stop: StopCounts,
    /// The logic's own named counters (see [`RoomSample::logic`]), as
    /// the latest sample carried them.
    pub logic: LogicCounters,
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
    /// Joins / close ops the registry could not hand to a connection's
    /// op dispatcher (see [`RegistrySample::join_ops_dropped`]),
    /// cumulative (B57).
    pub join_ops_dropped: u64,
    pub close_ops_dropped: u64,
    /// Match results a stopping room (or shard) could not hand to the
    /// result sink — full / closed (see
    /// [`MetricsEvent::MatchResultDropped`]), cumulative (B57). Counted
    /// by the collector from the rooms' events, reported with the
    /// registry's control-plane counters.
    pub match_results_dropped_full: u64,
    pub match_results_dropped_closed: u64,
    /// Room/shard TASKS that ended without their final count — a panic
    /// (see [`MetricsEvent::RoomEndedUncounted`]), cumulative (B67). A
    /// shard counts itself, so a sharded room's panic is one per dead
    /// shard (its surviving shards stop with their final count); compare
    /// [`Self::rooms_died`], which counts the LOGICAL rooms the registry
    /// reaped. Counted by the collector from the watchers' events.
    pub rooms_ended_uncounted: u64,
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
    /// Control frames sent by connection actors (queued on their outbound
    /// channels; a frame a closed channel refused is
    /// [`Self::frames_out_closed`] — B57).
    pub frames_out: u64,
    /// Game-band input actions dropped on full (bounded) per-connection
    /// action channels (cumulative, all connections). The per-connection
    /// attribution (who dropped what) is in
    /// [`MetricReport::actions_dropped_top`]. Game actions only since
    /// B55: RPC requests dropped the same way are
    /// [`Self::requests_dropped_full`].
    pub actions_dropped: u64,
    /// Protocol-violation events counted by the connection actors'
    /// violation budgets (cumulative, all connections). 0 on a healthy
    /// server; the per-event signal (peer address, close reason) is the
    /// structured tracing event emitted at budget exhaustion.
    pub violations: u64,
    /// Valid game-band input refused over a room's input rate limit
    /// (cumulative, all connections — see
    /// [`crate::metrics::ConnSample::input_rate_limited`]). 0 while no
    /// room limits input (the default); a rising count is the limit
    /// working, not a protocol problem.
    pub input_rate_limited: u64,
    /// Game-band actions / RPC requests a connection forwarded into an
    /// already closed action channel — the room had ended the membership
    /// and the connection had not learned it yet (cumulative, all
    /// connections; B51, see
    /// [`crate::metrics::ConnSample::actions_dropped_closed`]). Disjoint:
    /// a request is counted in the second only.
    pub actions_dropped_closed: u64,
    pub requests_dropped_closed: u64,
    /// RPC requests dropped on a full action channel, and RPC requests
    /// received outside any room (cumulative, all connections; B55, see
    /// [`crate::metrics::ConnSample::requests_dropped_full`]). Both are
    /// terms of the RPC ledger; the second is also in [`Self::violations`].
    pub requests_dropped_full: u64,
    pub requests_no_room: u64,
    /// Heartbeats over the 1/s ACK rate, counted and not answered, before
    /// and after authentication (cumulative, all connections; B56, see
    /// [`crate::metrics::ConnSample::heartbeats_throttled_preauth`]). Not
    /// violations.
    pub heartbeats_throttled_preauth: u64,
    pub heartbeats_throttled_authed: u64,
    /// Control frames refused by an already closed outbound channel, and
    /// best-effort close notices dropped on a full one (cumulative, all
    /// connections; B57, see
    /// [`crate::metrics::ConnSample::frames_out_closed`]).
    pub frames_out_closed: u64,
    pub close_notices_dropped: u64,
    /// Inbound frames never processed because the server ended the
    /// session first — requests, game-band frames, other base-band frames
    /// (cumulative, all connections; B60, see
    /// [`crate::metrics::ConnSample::requests_unprocessed`]). The first is
    /// a term of the RPC ledger.
    pub requests_unprocessed: u64,
    pub actions_unprocessed: u64,
    pub control_frames_unprocessed: u64,
    /// Sessions the SERVER ended on its own initiative, by reason
    /// (cumulative, all connections; see [`crate::conn::ServerClose`] for
    /// the taxonomy and what is deliberately not in it). A client-side
    /// end is never counted, so on a healthy run this is all zeros — and a
    /// load measurement whose clients saw no errors can still read here
    /// that the server shed half of them.
    pub server_closes: ServerCloses,
}

/// One periodic report: the server's current numeric state.
#[derive(Debug, Clone)]
pub struct MetricReport {
    /// Total metric samples dropped on the (bounded) metrics channel across
    /// all producers this report window. 0 in normal operation (A2 makes each
    /// room send at most one sample per report period); non-zero signals the
    /// collector falling behind (or a startup/shutdown flush burst).
    pub metrics_dropped: u64,
    /// Wall-clock instant at which the collector emitted this report (the
    /// `at` argument of [`MetricAccumulator::report`]).
    ///
    /// WHY on the value itself: the HTTP ops surface's `/healthz` answers
    /// "is the metrics ticker alive" from the *age* of the latest watch
    /// snapshot. Stamping the emission time into the value means ONE
    /// `watch::Sender<MetricReport>` carries content and freshness
    /// together — no second channel, no shared timestamp cell, no extra
    /// task whose only job would be copying a clock.
    pub emitted_at: Instant,
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
    /// The transport's own losses, cumulative, every door together (B58,
    /// see [`TransportCounters`]).
    pub transport: TransportCounters,
}

impl MetricReport {
    /// The watch channel's initial value (before the collector's first real
    /// emission): an empty report stamped *five periods in the past*, i.e.
    /// already past every staleness threshold a consumer applies (the HTTP
    /// `/healthz` threshold is three — see `gsb_server::http`). WHY back-
    /// dated instead of "now": an unstaled placeholder would let a health
    /// check answer "ok" during startup from a report nobody produced; the
    /// honest initial state is "no fresh report yet". The subtraction can
    /// fail on a platform whose monotonic clock started less than five
    /// periods ago (fresh boot); then "now" is the best available stamp and
    /// the window of wrong-side-of-threshold answers is bounded by one
    /// collector period anyway.
    pub fn initial_stale(period: Duration) -> Self {
        let emitted_at = Instant::now()
            .checked_sub(period.saturating_mul(5))
            .unwrap_or_else(Instant::now);
        Self {
            metrics_dropped: 0,
            emitted_at,
            rooms: Vec::new(),
            registry: None,
            net: NetReport {
                bytes_in: 0,
                bytes_out_room: 0,
                bytes_out_control: 0,
                bytes_out_total: 0,
                frames_in: 0,
                frames_out: 0,
                actions_dropped: 0,
                violations: 0,
                input_rate_limited: 0,
                actions_dropped_closed: 0,
                requests_dropped_closed: 0,
                requests_dropped_full: 0,
                requests_no_room: 0,
                heartbeats_throttled_preauth: 0,
                heartbeats_throttled_authed: 0,
                frames_out_closed: 0,
                close_notices_dropped: 0,
                requests_unprocessed: 0,
                actions_unprocessed: 0,
                control_frames_unprocessed: 0,
                server_closes: ServerCloses::default(),
            },
            actions_dropped_top: Vec::new(),
            transport: TransportCounters::default(),
        }
    }
}
