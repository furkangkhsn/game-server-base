//! The metrics wire format the separate-process mode exports over:
//! a hand-rolled encode/decode pair, so the child needs no HTTP.

use std::time::Instant;

use gsb_core::conn::ServerClose;
use gsb_core::id::{ConnectionId, RoomId};
use gsb_core::metrics::{
    FINE_HIST_BINS, HIST_BINS, MetricReport, NetReport, RegistryReport, RoomReport, ServerCloses,
};

mod logic;

/// Metric-report wire format (server process → orchestrator, one TCP
/// connection). The *data* is exactly what the in-process mode already
/// receives through the channel sink (`MetricReport` structs, 1 Hz) —
/// this is a serialization of those structs over a socket, **not** a log
/// format: no parsing of human text, no dependency on `gsb-metric` line
/// layout. Little-endian, no padding, one frame per report:
///
/// ```text
/// [u32 magic = METRICS_MAGIC, "GSMJ"][u32 body_len][body]
///
/// body =
///   u64 metrics_dropped
///   u32 n_rooms
///   per room (order as in `MetricReport::rooms`):
///     u64 room_id  u64 steps  f64 hz  u64 budget_us  u64 step_min_us
///     f64 step_mean_us  u64 step_max_us  [u64; HIST_BINS] step_hist
///     [u32; FINE_HIST_BINS] step_fine_hist
///     u64 late_min_us  f64 late_mean_us  u64 late_max_us
///     u64 lagged_events  u64 lagged_ticks  u64 dropped  f64 dropped_s
///     u64 sends_closed  u64 keepalive_resends  u64 snapshots
///     f64 snap_bytes_s  u32 snap_bytes_max  u64 snap_overflows
///     u64 snap_records  u64 shipped_bytes  f64 shipped_s
///     u64 shipped_frames  u64 private_frames
///     u32 groups  u32 members  u32 max_group  u64 joins  u64 leaves
///     u32 detached  u64 resumes  u64 resume_rejected_stale
///     u64 detach_expired_despawn  u64 detach_expired_ai
///     u64 detach_forced
///     u64 effects_applied  u64 effects_forwarded  u64 effects_orphaned
///     u64 effects_dropped  u64 effects_refused
///     u64 migrations_out  u64 migrations_in  u64 migrations_failed
///     u64 team_exports  u64 team_export_drops  u64 team_export_records
///     u64 team_over_cap  u64 team_imports  u64 team_import_records
///     u64 team_expired  u64 team_over_budget
///     u64 req_local  u64 req_ext
///     u64 req_rej_malformed  u64 req_rej_dup  u64 req_rej_no_handler
///     u64 req_rej_logic  u64 req_rej_conn  u64 req_rej_room
///     u64 req_refused  u64 req_unread  u64 req_unbound
///     u64 req_to  u64 req_late
///     u64 req_undelivered  u64 req_abandoned
///     u64 actions_unread  u64 actions_unbound
///     u32 req_pending
///     u64 metrics_dropped
///     u8 n_logic  u32 logic_dropped
///     per logic counter: u8 name_len  [u8; name_len] name
///                        u8 fold (0 = SUM, 1 = MAX)  u64 value
///   u8 registry_present
///   [if present] u32 rooms  u32 conns  u64 rooms_created
///                u64 rooms_destroyed  u64 rooms_died
///                u64 joins  u64 leaves
///                u64 opens  u64 closes
///                u64 join_ops_dropped  u64 close_ops_dropped
///                u64 match_results_dropped_full
///                u64 match_results_dropped_closed
///   u64 bytes_in  u64 bytes_out_room  u64 bytes_out_control
///   u64 bytes_out_total  u64 frames_in  u64 frames_out
///   u64 actions_dropped  u64 violations  u64 input_rate_limited
///   u64 actions_dropped_closed  u64 requests_dropped_closed
///   u64 requests_dropped_full  u64 requests_no_room
///   u64 heartbeats_throttled_preauth  u64 heartbeats_throttled_authed
///   u64 frames_out_closed  u64 close_notices_dropped
///   u64 requests_unprocessed  u64 actions_unprocessed
///   u64 control_frames_unprocessed
///   [u64; ServerClose::COUNT] server_closes (ServerClose::ALL order)
///   u32 n_top  [per entry] u64 conn_id  u64 count
/// ```
///
/// Both directions live in this binary (the server's `--serve` mode and
/// the orchestrator are the same executable), so the format cannot
/// drift between sides; the magic guards against a stale/reordered
/// connection. GSM2 = the GSM1 layout plus the net-scope
/// `actions_dropped` total and its per-connection attribution tail
/// (worst offenders first, ≤ 5 — see `MetricReport::actions_dropped_top`).
/// GSM3 = the GSM2 layout plus each room's fine step-duration histogram
/// (`[u32; FINE_HIST_BINS]`, fixed 8 µs bins — sub-budget resolution
/// alongside the budget-relative log2 histogram, whose overflow
/// semantics are untouched).
/// GSM4 = the GSM3 layout plus each room's RPC counters
/// (`req_local / req_ext / req_rej / req_to / req_late` cumulative +
/// `req_pending` gauge, see `gsb_core::rpc` and `MetricReport::rooms`).
/// GSM5 = the GSM4 layout with the single `req_rej` counter replaced by
/// the six per-cause reject buckets (`req_rej_malformed / _dup /
/// _no_handler / _logic / _conn / _room` — one per terminal reject
/// decision in the room's tick body; the buckets answer distinct
/// operational questions, which the cap-sizing measurement needs).
/// GSM6 = the GSM5 layout with the room-scope `dropped_actions` counter
/// REMOVED. It was never written by either the room or the shard actor
/// (the READ phase is a bounded pull that defers), so it only ever
/// carried 0; the real input-loss signal is the net-scope
/// `actions_dropped` total already in this frame, counted at the
/// connection actor's `try_send` and attributed by the `n_top` tail.
/// GSM7 = the GSM6 layout plus each room's shipped FRAME counts
/// (`shipped_frames` and the `private_frames` half of it). Both actors
/// had maintained them since the fan-out was written and neither ever
/// reached a report, so nothing could read them; they are not derivable
/// from `shipped_bytes` (a datagram transport is bounded by packets as
/// well as by bytes, and the private half is the per-connection share of
/// the fan-out).
/// GSM8 = the GSM7 layout plus the net-scope server-close counters, one
/// `u64` per `ServerClose` reason in `ServerClose::ALL` order, right
/// after `violations`. Without them a separate-process capacity run
/// could not see the server shedding its clients: the kills reach the
/// clients as silence (the stalled socket cannot carry an ERROR), so no
/// client-side counter moves.
/// GSM9 = the GSM8 layout plus each room's `detach_forced`, the five
/// remote-effect counters and the three migration counters, right after
/// `detach_expired_ai` (the layout above also spells out the park
/// fields GSM8 already carried there).
/// GSMA (the tenth layout) = the GSM9 layout plus each room's seven
/// team-exchange counters (`team_exports` .. `team_expired`, the shard's
/// cumulative `TeamStats` — W2 promoted them from the
/// `team_exchange_summary` log line), right after `migrations_failed`.
/// GSMB = the GSMA layout plus each room's `team_over_budget` (the
/// records the game's per-team export budget cut — A29), right after
/// `team_expired`.
/// GSMC = the GSMB layout plus each room's logic counters (F9 — the
/// logic's own named counters, `gsb_core::metrics::LogicCounters`),
/// right after `metrics_dropped`: their count, the set's overflow count,
/// then name, fold rule and value per counter (the help line does not
/// travel — the load generator prints values, not an exposition). The
/// names are validated on the way in, like everything else here.
/// GSMD = the GSMC layout plus each room's `requests_refused_congested`
/// (the congested connections' unanswered RPC refusals — F15, counted
/// with `req_rej_conn` before), right after `requests_rejected_room_cap`.
/// GSME = the GSMD layout plus the net-scope `input_rate_limited` (valid
/// game-band input refused over a room's input rate limit — E1), right
/// after `violations`.
/// GSMF = the GSME layout with one more server-close slot at the end of
/// `server_closes`: `idle_input` (the room's input-idle ceiling closing
/// a session under `afk_action = disconnect` — E6). The slot count is
/// `ServerClose::COUNT`, so a new reason is a new layout.
/// GSMG = the GSMF layout with one more server-close slot at the end:
/// `kicked` (a game's kick closing the session — E8).
/// GSMH = the GSMG layout plus each room's `requests_dropped_unread` (the
/// requests a session left unread in its action channel when it ended —
/// B36), right after `requests_refused_congested`.
/// GSMI = the GSMH layout plus each room's `sends_closed` (the fan-out's
/// batches tried on an already closed connection, split from `dropped`
/// — B32), right after `dropped_s`.
/// GSMJ = the GSMI layout plus the net-scope `actions_dropped_closed` and
/// `requests_dropped_closed` (the forwards a connection dropped into an
/// action channel the room had already closed — B51), right after
/// `input_rate_limited`.
/// GSMK = the GSMJ layout plus each room's `requests_undelivered` and
/// `requests_abandoned` (the answers and the in-flight requests a session
/// that ended took with it — B53), right after `requests_late`.
/// GSML = the GSMK layout plus each room's `requests_dropped_unbound`
/// (right after `requests_dropped_unread`) and `actions_dropped_unread`
/// / `actions_dropped_unbound` (right after `requests_abandoned`) — the
/// input the room dropped unprocessed, B54.
/// GSMM = the GSML layout plus the net-scope `requests_dropped_full` and
/// `requests_no_room` (the RPC ledger's two connection-side edges, split
/// out of `actions_dropped` and `violations` — B55), right after
/// `requests_dropped_closed`. `actions_dropped` keeps its slot and now
/// counts game-band actions only.
/// GSMN = the GSMM layout plus the net-scope
/// `heartbeats_throttled_preauth` and `heartbeats_throttled_authed` (the
/// heartbeat throttle's unanswered surplus — B56), right after
/// `requests_no_room`.
/// GSMO = the GSMN layout plus the net-scope `frames_out_closed` and
/// `close_notices_dropped` (the connection actors' own outbound losses —
/// B57), right after `heartbeats_throttled_authed`.
/// GSMP = the GSMO layout plus the registry section's `join_ops_dropped`,
/// `close_ops_dropped`, `match_results_dropped_full` and
/// `match_results_dropped_closed` (control-plane losses — B57), right
/// after `closes`.
/// GSMQ = the GSMP layout plus the net-scope `requests_unprocessed`,
/// `actions_unprocessed` and `control_frames_unprocessed` (what a
/// server-decided end left unprocessed — B60), right after
/// `close_notices_dropped`.
pub(crate) const METRICS_MAGIC: u32 = 0x4753_4D51;

/// Little-endian writer (the encode side of the format above).
pub(crate) struct W(Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f64(&mut self, v: f64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
}

pub(crate) fn encode_report(r: &MetricReport) -> Vec<u8> {
    let mut w = W(Vec::with_capacity(128 + r.rooms.len() * 256));
    w.u64(r.metrics_dropped);
    w.u32(r.rooms.len() as u32);
    for room in &r.rooms {
        w.u64(room.room.0);
        w.u64(room.steps);
        w.f64(room.hz);
        w.u64(room.budget_us);
        w.u64(room.step_min_us);
        w.f64(room.step_mean_us);
        w.u64(room.step_max_us);
        for bin in &room.step_hist {
            w.u64(*bin);
        }
        for bin in &room.step_fine_hist {
            w.u32(*bin as u32);
        }
        w.u64(room.late_min_us);
        w.f64(room.late_mean_us);
        w.u64(room.late_max_us);
        w.u64(room.lagged_events);
        w.u64(room.lagged_ticks);
        w.u64(room.dropped);
        w.f64(room.dropped_s);
        w.u64(room.sends_closed);
        w.u64(room.keepalive_resends);
        w.u64(room.snapshots);
        w.f64(room.snap_bytes_s);
        w.u32(room.snap_bytes_max);
        w.u64(room.snap_overflows);
        w.u64(room.snap_records);
        w.u64(room.shipped_bytes);
        w.f64(room.shipped_s);
        w.u64(room.shipped_frames);
        w.u64(room.private_frames);
        w.u32(room.groups);
        w.u32(room.members);
        w.u32(room.max_group);
        w.u64(room.joins);
        w.u64(room.leaves);
        w.u32(room.detached);
        w.u64(room.resumes);
        w.u64(room.resume_rejected_stale);
        w.u64(room.detach_expired_despawn);
        w.u64(room.detach_expired_ai);
        w.u64(room.detach_forced);
        w.u64(room.effects_applied);
        w.u64(room.effects_forwarded);
        w.u64(room.effects_orphaned);
        w.u64(room.effects_dropped);
        w.u64(room.effects_refused);
        w.u64(room.migrations_out);
        w.u64(room.migrations_in);
        w.u64(room.migrations_failed);
        w.u64(room.team_exports);
        w.u64(room.team_export_drops);
        w.u64(room.team_export_records);
        w.u64(room.team_over_cap);
        w.u64(room.team_imports);
        w.u64(room.team_import_records);
        w.u64(room.team_expired);
        w.u64(room.team_over_budget);
        w.u64(room.requests_local);
        w.u64(room.requests_external);
        w.u64(room.requests_rejected_malformed);
        w.u64(room.requests_rejected_dup);
        w.u64(room.requests_rejected_no_handler);
        w.u64(room.requests_rejected_logic);
        w.u64(room.requests_rejected_conn_cap);
        w.u64(room.requests_rejected_room_cap);
        w.u64(room.requests_refused_congested);
        w.u64(room.requests_dropped_unread);
        w.u64(room.requests_dropped_unbound);
        w.u64(room.requests_timed_out);
        w.u64(room.requests_late);
        w.u64(room.requests_undelivered);
        w.u64(room.requests_abandoned);
        w.u64(room.actions_dropped_unread);
        w.u64(room.actions_dropped_unbound);
        w.u32(room.pending_requests);
        w.u64(room.metrics_dropped);
        logic::encode(&mut w, &room.logic);
    }
    w.u8(match &r.registry {
        Some(_) => 1,
        None => 0,
    });
    if let Some(g) = &r.registry {
        w.u32(g.rooms);
        w.u32(g.conns);
        w.u64(g.rooms_created);
        w.u64(g.rooms_destroyed);
        w.u64(g.rooms_died);
        w.u64(g.joins);
        w.u64(g.leaves);
        w.u64(g.opens);
        w.u64(g.closes);
        w.u64(g.join_ops_dropped);
        w.u64(g.close_ops_dropped);
        w.u64(g.match_results_dropped_full);
        w.u64(g.match_results_dropped_closed);
    }
    w.u64(r.net.bytes_in);
    w.u64(r.net.bytes_out_room);
    w.u64(r.net.bytes_out_control);
    w.u64(r.net.bytes_out_total);
    w.u64(r.net.frames_in);
    w.u64(r.net.frames_out);
    w.u64(r.net.actions_dropped);
    w.u64(r.net.violations);
    w.u64(r.net.input_rate_limited);
    w.u64(r.net.actions_dropped_closed);
    w.u64(r.net.requests_dropped_closed);
    w.u64(r.net.requests_dropped_full);
    w.u64(r.net.requests_no_room);
    w.u64(r.net.heartbeats_throttled_preauth);
    w.u64(r.net.heartbeats_throttled_authed);
    w.u64(r.net.frames_out_closed);
    w.u64(r.net.close_notices_dropped);
    w.u64(r.net.requests_unprocessed);
    w.u64(r.net.actions_unprocessed);
    w.u64(r.net.control_frames_unprocessed);
    for (_, n) in r.net.server_closes.iter() {
        w.u64(n);
    }
    w.u32(r.actions_dropped_top.len() as u32);
    for (conn, n) in &r.actions_dropped_top {
        w.u64(conn.0);
        w.u64(*n);
    }
    let mut frame = Vec::with_capacity(8 + w.0.len());
    frame.extend_from_slice(&METRICS_MAGIC.to_le_bytes());
    frame.extend_from_slice(&(w.0.len() as u32).to_le_bytes());
    frame.extend_from_slice(&w.0);
    frame
}

/// Bounds-checked reader (the decode side).
pub(crate) struct R<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> R<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, i: 0 }
    }
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let end = self.i.checked_add(n)?;
        if end > self.b.len() {
            return None;
        }
        let s = &self.b[self.i..end];
        self.i = end;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|s| s[0])
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8)
            .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
    }
    fn f64(&mut self) -> Option<f64> {
        self.take(8)
            .map(|s| f64::from_le_bytes(s.try_into().unwrap()))
    }
    fn done(&self) -> bool {
        self.i == self.b.len()
    }
}

pub(crate) fn decode_report(body: &[u8]) -> Option<MetricReport> {
    let mut r = R::new(body);
    let metrics_dropped = r.u64()?;
    let n = r.u32()?;
    let mut rooms = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let room_id = r.u64()?;
        let steps = r.u64()?;
        let hz = r.f64()?;
        let budget_us = r.u64()?;
        let step_min_us = r.u64()?;
        let step_mean_us = r.f64()?;
        let step_max_us = r.u64()?;
        // (the read order must mirror the encode order above)
        let step_hist = {
            let mut h = [0u64; HIST_BINS];
            for bin in &mut h {
                *bin = r.u64()?;
            }
            h
        };
        let step_fine_hist = {
            let mut h = [0u64; FINE_HIST_BINS];
            for bin in &mut h {
                *bin = u64::from(r.u32()?);
            }
            h
        };
        rooms.push(RoomReport {
            room: RoomId(room_id),
            steps,
            hz,
            budget_us,
            step_min_us,
            step_mean_us,
            step_max_us,
            step_hist,
            step_fine_hist,
            late_min_us: r.u64()?,
            late_mean_us: r.f64()?,
            late_max_us: r.u64()?,
            lagged_events: r.u64()?,
            lagged_ticks: r.u64()?,
            dropped: r.u64()?,
            dropped_s: r.f64()?,
            sends_closed: r.u64()?,
            keepalive_resends: r.u64()?,
            snapshots: r.u64()?,
            snap_bytes_s: r.f64()?,
            snap_bytes_max: r.u32()?,
            snap_overflows: r.u64()?,
            snap_records: r.u64()?,
            shipped_bytes: r.u64()?,
            shipped_s: r.f64()?,
            shipped_frames: r.u64()?,
            private_frames: r.u64()?,
            groups: r.u32()?,
            members: r.u32()?,
            max_group: r.u32()?,
            joins: r.u64()?,
            leaves: r.u64()?,
            detached: r.u32()?,
            resumes: r.u64()?,
            resume_rejected_stale: r.u64()?,
            detach_expired_despawn: r.u64()?,
            detach_expired_ai: r.u64()?,
            detach_forced: r.u64()?,
            effects_applied: r.u64()?,
            effects_forwarded: r.u64()?,
            effects_orphaned: r.u64()?,
            effects_dropped: r.u64()?,
            effects_refused: r.u64()?,
            migrations_out: r.u64()?,
            migrations_in: r.u64()?,
            migrations_failed: r.u64()?,
            team_exports: r.u64()?,
            team_export_drops: r.u64()?,
            team_export_records: r.u64()?,
            team_over_cap: r.u64()?,
            team_imports: r.u64()?,
            team_import_records: r.u64()?,
            team_expired: r.u64()?,
            team_over_budget: r.u64()?,
            requests_local: r.u64()?,
            requests_external: r.u64()?,
            requests_rejected_malformed: r.u64()?,
            requests_rejected_dup: r.u64()?,
            requests_rejected_no_handler: r.u64()?,
            requests_rejected_logic: r.u64()?,
            requests_rejected_conn_cap: r.u64()?,
            requests_rejected_room_cap: r.u64()?,
            requests_refused_congested: r.u64()?,
            requests_dropped_unread: r.u64()?,
            requests_dropped_unbound: r.u64()?,
            requests_timed_out: r.u64()?,
            requests_late: r.u64()?,
            requests_undelivered: r.u64()?,
            requests_abandoned: r.u64()?,
            actions_dropped_unread: r.u64()?,
            actions_dropped_unbound: r.u64()?,
            pending_requests: r.u32()?,
            metrics_dropped: r.u64()?,
            logic: logic::decode(&mut r)?,
        });
    }
    let registry = match r.u8()? {
        1 => Some(RegistryReport {
            rooms: r.u32()?,
            conns: r.u32()?,
            rooms_created: r.u64()?,
            rooms_destroyed: r.u64()?,
            rooms_died: r.u64()?,
            joins: r.u64()?,
            leaves: r.u64()?,
            opens: r.u64()?,
            closes: r.u64()?,
            join_ops_dropped: r.u64()?,
            close_ops_dropped: r.u64()?,
            match_results_dropped_full: r.u64()?,
            match_results_dropped_closed: r.u64()?,
        }),
        0 => None,
        _ => return None,
    };
    let net = NetReport {
        bytes_in: r.u64()?,
        bytes_out_room: r.u64()?,
        bytes_out_control: r.u64()?,
        bytes_out_total: r.u64()?,
        frames_in: r.u64()?,
        frames_out: r.u64()?,
        actions_dropped: r.u64()?,
        violations: r.u64()?,
        input_rate_limited: r.u64()?,
        actions_dropped_closed: r.u64()?,
        requests_dropped_closed: r.u64()?,
        requests_dropped_full: r.u64()?,
        requests_no_room: r.u64()?,
        heartbeats_throttled_preauth: r.u64()?,
        heartbeats_throttled_authed: r.u64()?,
        frames_out_closed: r.u64()?,
        close_notices_dropped: r.u64()?,
        requests_unprocessed: r.u64()?,
        actions_unprocessed: r.u64()?,
        control_frames_unprocessed: r.u64()?,
        server_closes: {
            let mut counts = [0u64; ServerClose::COUNT];
            for n in &mut counts {
                *n = r.u64()?;
            }
            ServerCloses::from_counts(counts)
        },
    };
    let n_top = r.u32()?;
    let mut actions_dropped_top = Vec::with_capacity(n_top as usize);
    for _ in 0..n_top {
        actions_dropped_top.push((ConnectionId(r.u64()?), r.u64()?));
    }
    if !r.done() {
        return None;
    }
    Some(MetricReport {
        metrics_dropped,
        // The emission stamp (`emitted_at`) is a monotonic-clock `Instant`
        // from the SERVER process — meaningless across a process boundary
        // (the orchestrator's timeline differs), so it does not ride the
        // wire format (unchanged "GSM1"). The decoder stamps ARRIVAL time:
        // good enough for the orchestrator's age-style uses and honest
        // about when this side first held the value.
        emitted_at: Instant::now(),
        rooms,
        registry,
        net,
        actions_dropped_top,
    })
}
