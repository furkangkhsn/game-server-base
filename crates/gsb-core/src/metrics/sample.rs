//! What the actors send: cumulative counters and current gauges, one
//! sample per reporting period, never a rate (the collector times
//! those against each sample's own stamp).
use std::time::Instant;

use crate::conn::ServerClose;
use crate::id::{ConnectionId, RoomId};
use crate::metrics::*;

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
    ///
    /// All three are CUMULATIVE over the actor's life — never reset, so
    /// the extremes are the smallest and largest step the actor has ever
    /// taken, not this sample interval's. `*_min_us` really is a minimum:
    /// it held the first step's duration until the "min counters" round
    /// (see `RoomCounters::observe_step_us`).
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
    /// timestamp), µs: min / max / sum — cumulative over the actor's
    /// life, same rule as the `step_*` trio above.
    pub late_min_us: u64,
    pub late_max_us: u64,
    pub late_sum_us: u64,
    /// Outbound batches dropped at the fan-out (out channel full: slow
    /// client), cumulative. A closed out channel is not counted here:
    /// see [`Self::sends_closed`].
    pub dropped_frames: u64,
    /// Outbound batches the fan-out tried on an already CLOSED out
    /// channel (the connection is gone and the room has not processed its
    /// leave or detach yet — at most about one per connection end; no
    /// frame the client wanted is lost), cumulative (B32).
    pub sends_closed: u64,
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
    /// Detached-but-parked connections right now (the instant park count
    /// — a gauge, §10). Their cap slots are held (§4), so `members`
    /// includes them.
    pub detached: u32,
    /// Resumes accepted (a parked session rebound onto a fresh socket;
    /// same wire id), cumulative (§10).
    pub resumes: u64,
    /// Resume attempts rejected as stale (`ResumeFound::Ended` lookup or
    /// a tripped epoch guard; §7/§10), cumulative.
    pub resume_rejected_stale: u64,
    /// Holds that expired toward despawn (slot released), cumulative.
    pub detach_expired_despawn: u64,
    /// Holds that expired toward AI handover (`bot_fed` marker set; Tur B
    /// consumes it), cumulative.
    pub detach_expired_ai: u64,
    /// Holds the detach-hold ceiling (`RoomConfig::max_detach_hold`)
    /// forced to end over a standing `may_release` veto — a subset of the
    /// two `detach_expired_*` counters, cumulative (RECONNECT §17).
    pub detach_forced: u64,
    /// Remote effects (CROSS-SHARD §2–§4; shard actors only — 0 for a
    /// single room), cumulative, from the shard's effect counters: the
    /// effects this shard's game APPLIED as their authority; handed on
    /// to a migrated target's new owner (FORWARDED); aimed at a target
    /// that is gone (ORPHANED); lost on the way — a full retry buffer, a
    /// closed link, the hop bound or the age bound (DROPPED); and `emit`
    /// calls refused at the source — budget spent or target not lent
    /// (REFUSED). The finer split stays in the ~1 s
    /// `remote_effect_summary` log line.
    pub effects_applied: u64,
    pub effects_forwarded: u64,
    pub effects_orphaned: u64,
    pub effects_dropped: u64,
    pub effects_refused: u64,
    /// Migrations (shard actors only — 0 for a single room), cumulative:
    /// entities handed to a neighbour (committed sends), entities
    /// installed from one, and sends a full neighbour inbox refused (the
    /// entity stays; the crossing is retried next tick).
    pub migrations_out: u64,
    pub migrations_in: u64,
    pub migrations_failed: u64,
    /// The team exchange (`docs/CROSS-SHARD.md` §8b; shard actors whose
    /// logic exports team views — 0 otherwise), cumulative, from the
    /// shard's team counters: exports queued on the registry's mailbox
    /// and the records in them; exports a full or closed registry
    /// mailbox refused (the next tick's export carries the set again);
    /// records (and viewed teams) the core's per-message caps cut, on
    /// the way out and on the way in; records the LOGIC's own per-team
    /// budget cut before the export reached the core (the game's policy —
    /// the kit's `with_team_budget`; A29); imports applied (the hub's
    /// relays that arrived) and their records; source slots the TTL
    /// dropped.
    /// The same numbers, per ~1 s window, make the
    /// `team_exchange_summary` log line.
    pub team_exports: u64,
    pub team_export_drops: u64,
    pub team_export_records: u64,
    pub team_over_cap: u64,
    pub team_over_budget: u64,
    pub team_imports: u64,
    pub team_import_records: u64,
    pub team_expired: u64,
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
    /// RPC: requests REFUSED — neither processed nor answered — because
    /// their connection was congested (its latest batch dropped) and
    /// already owed its per-connection cap (the storm bound, F14; its own
    /// counter since F15, apart from the answered cap rejections),
    /// cumulative.
    pub requests_refused_congested: u64,
    /// RPC: requests still UNREAD in a session's action channel when the
    /// session ended (a leave that landed before the READ pull, a
    /// despawned or resumed park, a superseding rejoin, a park the
    /// input-idle ceiling left behind under `afk_action = leave_room`) —
    /// neither
    /// processed nor answered; the bucket that closes the ledger `sent =
    /// answered + rejected + refused + accepted + this` (B36),
    /// cumulative.
    pub requests_dropped_unread: u64,
    /// RPC: pending external requests swept as timed out (the
    /// client-visible timeout), cumulative.
    pub requests_timed_out: u64,
    /// RPC: worker reports for ids no longer pending (answered, timed
    /// out, or the connection left), dropped by the reconciliation,
    /// cumulative.
    pub requests_late: u64,
    /// RPC: answers produced for a session that ended before any batch
    /// carried them — a leave, a detach, a migration, a superseding
    /// rejoin — discarded undelivered (B53), cumulative. The request is
    /// already in its own bucket (`requests_local`, a rejection, …); this
    /// counts what became of its answer. An answer put back after a
    /// failed send (F14) is counted once, when it is finally discarded.
    pub requests_undelivered: u64,
    /// RPC: external requests still in flight when their session ended
    /// (the same ends): no answer will reach that session (B53),
    /// cumulative. The worker's later report, if one comes, is counted
    /// again as a report in [`Self::requests_late`].
    pub requests_abandoned: u64,
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
    /// The logic's own named counters (F9, see [`LogicCounters`]),
    /// cumulative as the logic keeps them, read from
    /// `GameLogic::logic_counters` when the sample is built. Empty for a
    /// logic that declares none.
    pub logic: LogicCounters,
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
    /// the architecture (the room's READ phase is a bounded *pull* that
    /// drops nothing, so no room-scope drop counter exists) and always
    /// self-inflicted: a
    /// flooding connection drops its own input. The collector sums these
    /// per connection so a report can attribute the loss to its sender.
    pub actions_dropped: u64,
    /// Game-band actions this actor forwarded into an already CLOSED
    /// action channel, delta since its last flush (B51): the room had
    /// ended the membership itself (a kick, the input-idle ceiling, the
    /// room closing or retiring) and the connection forwarded before it
    /// learned — the room never sees them. At most one forward per ended
    /// membership lands here: the closed send detaches the connection, so
    /// the next frame is answered `ERROR 6` (not in a room). Unlike
    /// [`Self::actions_dropped`] (which counts RPC requests too), RPC
    /// requests are counted apart: [`Self::requests_dropped_closed`].
    pub actions_dropped_closed: u64,
    /// RPC requests (`RPC_REQ`) this actor forwarded into an already
    /// CLOSED action channel, delta since its last flush (B51) — the
    /// same moment as [`Self::actions_dropped_closed`]. Never processed,
    /// never answered, and never seen by the room, so no room counter can
    /// hold them: the connection-side term of the RPC ledger
    /// (`docs/RPC-CONTROL-PLANE.md` §8.3).
    pub requests_dropped_closed: u64,
    /// Metric samples this actor dropped on a full (bounded) metrics
    /// channel, delta since its last flush.
    pub metrics_dropped: u64,
    /// Protocol-violation events counted by this actor's budget (module
    /// docs in `gsb_core::conn`), delta since its last flush. The net
    /// scope sums these: a rising total is the budget working (clients
    /// violating); the per-event detail (peer address, close reason) is in
    /// the tracing close signal, not here.
    pub violations: u64,
    /// Valid game-band input this actor refused over its room's input
    /// rate limit (`RoomConfig::input_rate`, docs/SECURITY.md "post-auth
    /// input volume"), delta since its last flush. Dropped, never queued,
    /// and deliberately NOT a violation: an over-rate client is not
    /// sending anything a correct client could not — the number is the
    /// game's gameplay limit, not evidence of hostility. Always 0 while
    /// no room limits input (the default).
    pub input_rate_limited: u64,
    /// Why the SERVER ended this session — set on the final sample only
    /// (`last = true`), and only when the end was a server verdict (see
    /// [`ServerClose`]); `None` for a client-side end and on every
    /// earlier sample. The collector counts it into
    /// [`NetReport::server_closes`].
    ///
    /// Caveat shared with every other delta here: the final sample is a
    /// `try_send` like the rest, so a metrics channel that is FULL at the
    /// instant of the close loses it (the loss itself is then counted
    /// nowhere — the actor is gone). The channel is 4096 deep and drained
    /// every tick, so this needs thousands of closes inside one tick.
    pub server_close: Option<ServerClose>,
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
