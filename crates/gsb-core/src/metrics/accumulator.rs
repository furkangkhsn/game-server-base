//! The collector's running state: the latest sample per room, the
//! grace window a destroyed room's accumulator lingers for, and the
//! fold of samples into one report (in the child module).

use std::collections::BTreeMap;

use crate::id::{ConnectionId, RoomId};
use crate::metrics::*;

mod cut;
mod report;

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
    /// Summed delta of rate-limited game-band input across all connection
    /// actors (E1; cumulative, never attributed — see the net report).
    conn_input_rate_limited: u64,
    /// Summed deltas of the forwards the connection actors dropped into a
    /// closed action channel (B51; cumulative, never attributed).
    conn_actions_dropped_closed: u64,
    conn_requests_dropped_closed: u64,
    /// Summed deltas of the RPC requests the connection actors dropped on
    /// a full action channel / received outside any room (B55;
    /// cumulative, never attributed).
    conn_requests_dropped_full: u64,
    conn_requests_no_room: u64,
    /// Summed deltas of the heartbeats the connection actors counted and
    /// did not answer, before / after authentication (B56; cumulative).
    conn_heartbeats_throttled_preauth: u64,
    conn_heartbeats_throttled_authed: u64,
    /// Summed deltas of the control frames refused by a closed outbound
    /// channel and of the close notices dropped on a full one (B57).
    conn_frames_out_closed: u64,
    conn_close_notices_dropped: u64,
    /// Summed deltas of the inbound frames the connection actors never
    /// processed because the server ended their sessions, by kind (B60).
    conn_requests_unprocessed: u64,
    conn_actions_unprocessed: u64,
    conn_control_frames_unprocessed: u64,
    /// Server-initiated session closes by reason (cumulative; one per
    /// closed session at most, from its final sample).
    conn_server_closes: ServerCloses,
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
    /// Match results the stopping rooms could not hand to the result
    /// sink, by cause (B57; cumulative — reported in the registry slice).
    match_results_dropped_full: u64,
    match_results_dropped_closed: u64,
    /// Room/shard tasks that ended without their final count (B67;
    /// cumulative — reported in the registry slice).
    rooms_ended_uncounted: u64,
    /// Joins refused by a closed room (B75; cumulative — reported in the
    /// registry slice).
    joins_refused_closed: u64,
    /// Joins the stopped registry's closed mailbox refused (F54;
    /// cumulative — reported in the registry slice).
    joins_unsent: u64,
    /// Session verdicts the stop kept from being carried out, summed over
    /// the places that counted them (F56; cumulative — reported in the
    /// registry slice).
    verdicts_lost: VerdictsLost,
    /// The transport tasks' loss deltas, summed (B58).
    transport: TransportCounters,
    /// Periodic reports the collector emitted torn at its cut grace
    /// (F70; cumulative — see `cut`).
    reports_torn_at_cut_grace: u64,
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
                        self.rooms.insert(
                            s.room,
                            RoomAcc {
                                latest: s,
                                prev: None,
                            },
                        );
                    }
                }
            }
            MetricsEvent::RoomFinal(s) => {
                // The room's last word (B62): taken even inside the
                // linger — it is the truth at the stop, not a straggler
                // refreshing a dead row — and it starts the linger when
                // the registry's notice has not (or was lost): the room
                // is gone either way, so the row is dropped when the
                // windows run down and is never a ghost. An existing
                // countdown is kept: the row is reported at least once
                // more whatever is left of it.
                match self.rooms.get_mut(&s.room) {
                    Some(acc) => acc.latest = s,
                    None => {
                        self.rooms.insert(
                            s.room,
                            RoomAcc {
                                latest: s,
                                prev: None,
                            },
                        );
                    }
                }
                self.rooms_gone_grace
                    .entry(s.room)
                    .or_insert(ROOM_GONE_GRACE_REPORTS);
            }
            MetricsEvent::Registry(s) => self.registry = Some(s),
            MetricsEvent::Transport(d) => self.transport.add(&d),
            MetricsEvent::MatchResultDropped(cause) => {
                let n = match cause {
                    MatchResultDrop::Full => &mut self.match_results_dropped_full,
                    MatchResultDrop::Closed => &mut self.match_results_dropped_closed,
                };
                *n = n.saturating_add(1);
            }
            MetricsEvent::RoomEndedUncounted(id) => {
                // The task is gone and will send nothing more: count it,
                // and let its row linger with its last sample, exactly as
                // a final sample would (an existing countdown is kept).
                self.rooms_ended_uncounted = self.rooms_ended_uncounted.saturating_add(1);
                self.rooms_gone_grace
                    .entry(id)
                    .or_insert(ROOM_GONE_GRACE_REPORTS);
            }
            MetricsEvent::JoinRefusedClosed => {
                self.joins_refused_closed = self.joins_refused_closed.saturating_add(1);
            }
            MetricsEvent::JoinUnsent => {
                self.joins_unsent = self.joins_unsent.saturating_add(1);
            }
            MetricsEvent::VerdictsLost(v) => self.verdicts_lost.add(&v),
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
                self.conn_input_rate_limited = self
                    .conn_input_rate_limited
                    .saturating_add(c.input_rate_limited);
                self.conn_actions_dropped_closed = self
                    .conn_actions_dropped_closed
                    .saturating_add(c.actions_dropped_closed);
                self.conn_requests_dropped_closed = self
                    .conn_requests_dropped_closed
                    .saturating_add(c.requests_dropped_closed);
                self.conn_requests_dropped_full = self
                    .conn_requests_dropped_full
                    .saturating_add(c.requests_dropped_full);
                self.conn_requests_no_room = self
                    .conn_requests_no_room
                    .saturating_add(c.requests_no_room);
                self.conn_heartbeats_throttled_preauth = self
                    .conn_heartbeats_throttled_preauth
                    .saturating_add(c.heartbeats_throttled_preauth);
                self.conn_heartbeats_throttled_authed = self
                    .conn_heartbeats_throttled_authed
                    .saturating_add(c.heartbeats_throttled_authed);
                self.conn_frames_out_closed = self
                    .conn_frames_out_closed
                    .saturating_add(c.frames_out_closed);
                self.conn_close_notices_dropped = self
                    .conn_close_notices_dropped
                    .saturating_add(c.close_notices_dropped);
                self.conn_requests_unprocessed = self
                    .conn_requests_unprocessed
                    .saturating_add(c.requests_unprocessed);
                self.conn_actions_unprocessed = self
                    .conn_actions_unprocessed
                    .saturating_add(c.actions_unprocessed);
                self.conn_control_frames_unprocessed = self
                    .conn_control_frames_unprocessed
                    .saturating_add(c.control_frames_unprocessed);
                if let Some(reason) = c.server_close {
                    self.conn_server_closes.add(reason);
                }
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
}
