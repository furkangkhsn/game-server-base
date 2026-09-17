//! The room's member table rows and its local counters — owned by the
//! actor, shared in shape with the shard actor, and flushed to the
//! metrics channel as a sample.
use crate::channel::{FrameBatch, Inbox};
use crate::id::{ConnectionId, EntityId};
use crate::metrics::{FINE_HIST_BINS, HIST_BINS};
use crate::room::*;
use std::fmt::Debug;
use std::time::Instant;
use tokio::sync::mpsc;

/// Per-player row of the room (or shard) member table. `pub(crate)`
/// because the shard actor reuses the same table shape (a shard's `conns`
/// is the shard's share of the room's members — see `crate::shard`).
///
/// Keyed by the stable [`PlayerId`] (Faz 2): the row — and everything it
/// carries (entity, group, detach flags) — survives resume and migration
/// under its key. Only the transport session feeding it changes.
pub(crate) struct RoomConn<G> {
    /// The transport session CURRENTLY bound to this player (the binding
    /// table's back-reference; updated at join/resume together with the
    /// binding entry). Used exactly where a control message names a
    /// session-scoped thing owned by this row: pruning its RPC request
    /// state and removing its binding row on despawn.
    pub(crate) conn: ConnectionId,
    pub(crate) out: mpsc::Sender<FrameBatch>,
    /// This connection's input, written by its connection actor as frames
    /// arrive; pulled non-blockingly at each step.
    pub(crate) actions: Inbox<Action>,
    pub(crate) entity: EntityId,
    /// Snapshot group this connection belongs to (recomputed every tick via
    /// [`GameLogic::group_of`]).
    pub(crate) group: G,
    /// The fan-out batch buffer, reused across ticks (the measured floor
    /// carried one `Vec::with_capacity(2)` per connection per tick; the
    /// capacity is retained so the 0-2-frame batch never allocates again
    /// after warm-up — see `docs/ROADMAP.md`, the floor breakdown).
    pub(crate) batch: FrameBatch,
    // -- Detach/resume state (§14.4: the CLOCK is core-owned; §7: the
    //    broadcast/READ skip flags are flag-guarded so a dead outbound
    //    half never pollutes the drop counter). --------------------------
    /// The transport died but the entity is parked (a [`Detach::Hold`]
    /// policy answer): READ pulls nothing for this row and BROADCAST ships
    /// it nothing (the outbound half is dead; a `try_send` against it
    /// would count a drop nobody caused). Everything else is unchanged:
    /// the entity stays in `ingest`, `update`, snapshots, group
    /// membership, and the member/slot accounting (§4).
    pub(crate) detached: bool,
    /// When the hold ends at the latest (`Detach::Hold.grace` mapped to an
    /// absolute instant by the CORE — §14.4 deadline ownership);
    /// `None` = combat-held, only [`GameLogic::may_release`] ends it.
    pub(crate) detach_deadline: Option<Instant>,
    /// The policy's chosen end ([`ExpireTo`]) when the hold expires.
    pub(crate) expire_to: ExpireTo,
    /// The hold expired toward [`ExpireTo::AiHandover`]: the entity keeps
    /// playing on synthesized input (the game logic synthesizes; Tur B's
    /// demo bot consumes this marker). Channels stay alive; the row stays
    /// skipped for READ/broadcast exactly like a detached one (there is no
    /// human socket behind it), and the sweep never re-fires (deadline
    /// cleared).
    pub(crate) bot_fed: bool,
    /// The resume-accept epoch stamp (§7's guard): the newest session that
    /// (re)bound this row. A later resume with an older-or-equal epoch is
    /// a delayed duplicate and is rejected. `0` = unset (guard off).
    pub(crate) session_epoch: u64,
}

/// Per-group broadcast state, kept across ticks. `pub(crate)` because the
/// shard actor reuses the same group table shape (see `crate::shard`).
pub(crate) struct GroupState {
    /// Last snapshot payload emitted for the group; re-sent on keep-alive
    /// ticks when the group is unchanged.
    pub(crate) last: Option<bytes::Bytes>,
    /// The payload fanned out to the members this tick (the emitted
    /// snapshot or the keep-alive re-send); `None` = nothing shipped.
    pub(crate) sent: Option<bytes::Bytes>,
    /// A group that has members but has never emitted is in contract
    /// violation (a fresh group's first tick is a membership change and
    /// must emit) — warn once for it instead of every tick.
    pub(crate) never_emitted_warned: bool,
    /// A snapshot over `max_snapshot_bytes` is a standing property of the
    /// group (its content does not shrink on its own), so warn once for it
    /// rather than on every tick of every room.
    pub(crate) size_warned: bool,
}

/// The room's local metric counters (all cumulative; see [`crate::metrics`]).
/// Owned by the room and never shared: each step the room builds a
/// [`RoomSample`] from them and hands it to the collector over the
/// (bounded) metrics channel with a synchronous `try_send` — no await.
/// `pub(crate)`
/// because the shard actor reuses the same counter shape (a shard's sample
/// is a [`RoomSample`] under its derived sample id — see `crate::shard`).
///
/// Manual `Default` (not derived): `[u32; FINE_HIST_BINS]` exceeds the
/// derived-`Default` array bound (32); every field is a zero.
#[derive(Debug)]
pub(crate) struct RoomCounters {
    /// Broadcast `Lagged` occurrences / missed tick indices.
    pub(crate) lagged_events: u64,
    pub(crate) lagged_ticks: u64,
    /// Step body duration µs: min / max / sum + histogram (binning).
    pub(crate) step_min_us: u64,
    pub(crate) step_max_us: u64,
    pub(crate) step_sum_us: u64,
    pub(crate) step_hist: [u64; HIST_BINS],
    /// Fine step-duration histogram (fixed 8 µs bins, `[0, 4096 µs)` —
    /// sub-budget resolution; steps at/above the cap stay in `step_hist`
    /// only. See `metrics::FINE_HIST_*`.
    pub(crate) step_fine_hist: [u32; FINE_HIST_BINS],
    /// Tick processing latency µs (step start − ticker `at`): min/max/sum.
    pub(crate) late_min_us: u64,
    pub(crate) late_max_us: u64,
    pub(crate) late_sum_us: u64,
    /// Outbound batches dropped at the fan-out (slow client), cumulative.
    pub(crate) dropped_frames: u64,
    /// Keep-alive re-sends, cumulative.
    pub(crate) keepalive_resends: u64,
    /// Group snapshots encoded, cumulative (+ encoded bytes, max payload).
    pub(crate) snapshots: u64,
    pub(crate) snap_bytes: u64,
    pub(crate) snap_bytes_max: u32,
    /// Snapshots whose payload exceeded `max_snapshot_bytes`, cumulative.
    pub(crate) snap_overflows: u64,
    /// Entity records encoded (summed over all groups, via
    /// [`GameLogic::encoded_records`]), cumulative. Together with the
    /// broadcastable entity count this is the *overlap multiplier*: how
    /// many times the same entity was encoded into group snapshots per
    /// tick (1.0 for one-group rooms, up to the block overlap for cell
    /// AOI, the visibility-table out-degree for PVS).
    pub(crate) snap_records: u64,
    /// Metric samples dropped on a full (bounded) metrics channel,
    /// cumulative.
    pub(crate) metrics_dropped: u64,
    /// Snapshot + private bytes/frames shipped to the room's connections,
    /// cumulative.
    pub(crate) shipped_bytes: u64,
    pub(crate) shipped_frames: u64,
    pub(crate) private_frames: u64,
    /// Joins / leaves processed on the control channel, cumulative.
    pub(crate) joins: u64,
    pub(crate) leaves: u64,
    /// Resumes accepted (a parked session rebound onto a fresh socket),
    /// cumulative (§10).
    pub(crate) resumes: u64,
    /// Resume attempts rejected as stale — the ledger answered
    /// [`ResumeFound::Ended`] or the epoch guard tripped (§7/§10). The
    /// client-visible outcome of an `Ended` rejection is still the
    /// transparent fresh join; this counter is the mechanism-level signal.
    pub(crate) resume_rejected_stale: u64,
    /// Holds that expired toward [`ExpireTo::Despawn`] (slot released),
    /// cumulative.
    pub(crate) detach_expired_despawn: u64,
    /// Holds that expired toward [`ExpireTo::AiHandover`] (entity kept,
    /// bot-fed marker set — Tur B's seam), cumulative.
    pub(crate) detach_expired_ai: u64,
    /// RPC requests answered room-local in the same tick, cumulative.
    pub(crate) requests_local: u64,
    /// RPC requests delegated to a worker (registered pending), cumulative.
    pub(crate) requests_external: u64,
    /// RPC rejections, split by cause (one bucket per terminal reject
    /// decision in the tick body — see the `2a`/`2c` phases): the
    /// buckets answer distinct operational questions (client protocol
    /// bug vs. duplicate storm vs. one hoarding connection vs. room
    /// budget vs. the game logic's own business rejections), which is
    /// what the cap-sizing measurement needs (§6.1 of
    /// `docs/RPC-CONTROL-PLANE.md`). Cumulative.
    /// Malformed envelope or correlation id = 0 (cannot correlate).
    pub(crate) requests_rejected_malformed: u64,
    /// In-flight duplicate id (every decision kind; rejected without
    /// re-processing).
    pub(crate) requests_rejected_dup: u64,
    /// The room's logic handles no request for the op.
    pub(crate) requests_rejected_no_handler: u64,
    /// The logic's own `Reject` decision (a business answer — normal
    /// flow, not an anomaly).
    pub(crate) requests_rejected_logic: u64,
    /// The per-connection pending cap bound the request.
    pub(crate) requests_rejected_conn_cap: u64,
    /// The room-wide pending cap bound the request.
    pub(crate) requests_rejected_room_cap: u64,
    /// Pending external requests swept as timed out (the client-visible
    /// timeout; see `crate::rpc`), cumulative.
    pub(crate) requests_timed_out: u64,
    /// Worker reports that arrived for an id no longer pending (already
    /// answered, timed out, or the connection left) and were dropped,
    /// cumulative (the exactly-one-answer reconciliation in action).
    pub(crate) requests_late: u64,
    /// Largest snapshot group this tick (recomputed in the broadcast
    /// phase; carried in the per-step sample as a gauge).
    pub(crate) step_max_group: u32,
}

impl Default for RoomCounters {
    fn default() -> Self {
        Self {
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
            dropped_frames: 0,
            keepalive_resends: 0,
            snapshots: 0,
            snap_bytes: 0,
            snap_bytes_max: 0,
            snap_overflows: 0,
            snap_records: 0,
            metrics_dropped: 0,
            shipped_bytes: 0,
            shipped_frames: 0,
            private_frames: 0,
            joins: 0,
            leaves: 0,
            resumes: 0,
            resume_rejected_stale: 0,
            detach_expired_despawn: 0,
            detach_expired_ai: 0,
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
            step_max_group: 0,
        }
    }
}
