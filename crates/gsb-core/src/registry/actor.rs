//! The registry actor's state. Its behaviour is split across child
//! modules by concern; they are children, so the tables stay private
//! to this module tree.

use std::collections::{HashMap, VecDeque};
use std::fmt::Debug;
use std::hash::Hash;

use tokio::sync::mpsc;

use crate::channel::{Inbox, Mailbox};
use crate::id::{ConnectionId, RoomId};
use crate::metrics::{MetricsEvent, RegistrySample};
use crate::registry::*;
use crate::ticker::Ticker;

mod conns;
mod dispatch;
mod leave;
mod players;
mod room_ops;
mod rooms;
mod run;
mod stop;

/// The registry actor. `W`/`G` are the room's world / group-key types;
/// `St` is the sharded room's migration state (unused by single rooms —
/// see [`BuiltRoom`]).
pub struct Registry<W, G, St, Sp> {
    factory: RoomFactory<W, G, St, Sp>,
    inbox: Inbox<RegistryMsg>,
    /// Sender half of our own mailbox: cloned to dispatcher tasks so they
    /// can report back.
    self_mailbox: Mailbox<RegistryMsg>,
    rooms: HashMap<RoomId, RoomEntry<St, Sp>>,
    /// Per-room-id incarnation counter: bumped on every install (first
    /// create AND each panic rebuild). A room id can outlive several
    /// incarnations (destroy → re-create, death → restart); each death
    /// watcher carries its own incarnation's number, so a late report from
    /// a dead-and-replaced room can never reap the wrong entry — no
    /// cancellation plumbing, just one integer comparison at report time.
    room_gen: HashMap<RoomId, u64>,
    conns: HashMap<ConnectionId, ConnInfo>,
    conn_ops: HashMap<ConnectionId, mpsc::Sender<RoomOp<St, Sp>>>,
    ticker: Ticker,
    /// Local control-plane counters (flushed as a sample whenever a table
    /// changes — event-driven; no timer, no new await; see
    /// [`crate::metrics`]).
    reg_created: u64,
    reg_destroyed: u64,
    /// Rooms that died UNEXPECTEDLY (a panicked room or shard task; one
    /// dead shard counts once), cumulative — see `RegistryMsg::RoomDied`.
    /// A destroy never increments this; a rebuild after a death does not
    /// increment `reg_created` (the rebirth is not a control-plane create).
    reg_died: u64,
    reg_joins: u64,
    reg_leaves: u64,
    reg_opens: u64,
    reg_closes: u64,
    /// Metric samples dropped on a full (bounded) metrics channel,
    /// cumulative.
    reg_metrics_dropped: u64,
    /// Global monotonic join-epoch counter, minted here at dispatch (see
    /// the `RoomOp::Join::epoch` doc: per-connection counters made every
    /// resume after an identity's first trip the staleness guard once).
    next_join_epoch: u64,
    /// Outbound metrics path (bounded channel; the registry sends with the
    /// synchronous `try_send` — no await).
    metrics: mpsc::Sender<MetricsEvent>,
    /// Server-wide connection cap (the accept loop's guardrail, enforced
    /// where the connection *count* lives — the registry's table, not the
    /// accept loop's local state, because the accept loop cannot observe
    /// disconnects without a second awaited source). `None` = unlimited.
    /// A rejected connection is never recorded (no table entry, no
    /// `reg_opens`) and is told to close itself via
    /// [`ConnIn::ServerClosed`] (an `ERROR` frame, code 9, then EOF).
    max_connections: Option<u64>,
    /// Cap on simultaneously UNAUTHENTICATED connections
    /// (docs/SECURITY.md §4), enforced exactly where `max_connections` is:
    /// the connection table is the only place that sees both opens,
    /// closes, and (now) auth transitions. A rejected connection gets the
    /// same gentle birth rejection (`ERROR` frame, code 9, "server at
    /// unauthenticated capacity") and no table entry. Detached/resumed
    /// sessions are authenticated entries by construction and never count.
    /// The default derivation (25 % of `max_connections`, floored at 64)
    /// lives at the composition root — this actor takes the resolved cap.
    /// `None` = no unauth cap (explicitly disabled, or derived-off).
    max_unauth_conns: Option<u64>,
    /// The match-result sink (the control plane's result seam, see
    /// [`crate::room::RoomLogic::match_result`]): a bounded mailbox the
    /// composition root reads from (its reference adapter). Cloned to
    /// each room at creation; `None` = rooms report no result. The
    /// registry never awaits the sink (it only holds a sender clone).
    result_sink: Option<Mailbox<MatchResult>>,
    /// RETIRED room ids (§8): ids whose room ENDED in this process — an
    /// accepted `DestroyRoom` (an ended match, or a persistent room's
    /// decommissioning), or an unexpected death left unrebuilt. Joins and
    /// resumes answer ERROR 12 ([`CoreError::RoomRetired`], "do not retry
    /// — return to the lobby") instead of 4, because the client decision
    /// differs: an ended match must never silently reopen, and an
    /// operator's decommissioning intent must never be crushed by
    /// automatic re-creation.
    ///
    /// Bounded by [`RETIRED_SET_CAP`] with FIFO eviction of the oldest
    /// retirement: the set is operator-scale by nature (rooms end at
    /// human/matchmaker pace), and the cap keeps the discipline that no
    /// table grows without bound; an id evicted after extreme churn
    /// degrades safely to the pre-reconnect answer (4).
    retired: HashMap<RoomId, ()>,
    /// FIFO order for the eviction cap above.
    retired_order: VecDeque<RoomId>,
}

impl<W, G, St, Sp> Registry<W, G, St, Sp>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
    St: Debug + Send + 'static,
    // The strip payload's trait bounds (`GameLogic::Strip`) — the
    // registry never inspects payloads, but both actor shapes it spawns
    // require them.
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        inbox: Inbox<RegistryMsg>,
        self_mailbox: Mailbox<RegistryMsg>,
        factory: RoomFactory<W, G, St, Sp>,
        ticker: Ticker,
        // Outbound metrics path (see `crate::metrics`): a bounded channel;
        // the registry sends with the synchronous `try_send` (no await).
        metrics: mpsc::Sender<MetricsEvent>,
        // Server-wide connection cap (`None` = unlimited; see the field).
        max_connections: Option<u64>,
        // Unauthenticated-connection cap (docs/SECURITY.md §4; `None` =
        // no cap — the derivation from `max_connections` happened at the
        // composition root; see the field).
        max_unauth_conns: Option<u64>,
        // The match-result sink (see the field): `None` = no result seam.
        result_sink: Option<Mailbox<MatchResult>>,
    ) -> Self {
        Self {
            factory,
            inbox,
            self_mailbox,
            rooms: HashMap::new(),
            room_gen: HashMap::new(),
            conns: HashMap::new(),
            conn_ops: HashMap::new(),
            ticker,
            reg_created: 0,
            reg_destroyed: 0,
            reg_died: 0,
            reg_joins: 0,
            reg_leaves: 0,
            reg_opens: 0,
            reg_closes: 0,
            reg_metrics_dropped: 0,
            next_join_epoch: 0,
            metrics,
            max_connections,
            max_unauth_conns,
            result_sink,
            retired: HashMap::new(),
            retired_order: VecDeque::new(),
        }
    }

    /// Flush the registry's local counters as a sample. Synchronous
    /// `try_send` on the bounded metrics channel (A3): the registry's await
    /// set is unchanged (its only await stays the mailbox `recv`), and a
    /// full channel drops + counts the sample (harmless — the counters are
    /// cumulative, so the next flush carries everything).
    pub(super) fn emit_metrics(&mut self) {
        let sample = RegistrySample {
            rooms: self.rooms.len() as u32,
            conns: self.conns.len() as u32,
            rooms_created: self.reg_created,
            rooms_destroyed: self.reg_destroyed,
            rooms_died: self.reg_died,
            joins: self.reg_joins,
            leaves: self.reg_leaves,
            opens: self.reg_opens,
            closes: self.reg_closes,
            metrics_dropped: self.reg_metrics_dropped,
        };
        if let Err(mpsc::error::TrySendError::Full(_)) =
            self.metrics.try_send(MetricsEvent::Registry(sample))
        {
            self.reg_metrics_dropped += 1;
        }
    }

    /// Tell the collector a room's accumulator can go: sent at every
    /// point this actor removes a live table entry — an accepted
    /// [`RegistryMsg::DestroyRoom`], an unexpected-death reap, and the
    /// shutdown-all drain. Without it the collector kept a per-room
    /// accumulator for every room id ever created. A notice lost to a
    /// full channel leaves that one accumulator until process end — the
    /// same bounded-imprecision trade as `emit_metrics` above (the
    /// counters there are cumulative; here the cost is one stale entry,
    /// never a wrong number).
    pub(super) fn emit_room_gone(&mut self, id: RoomId) {
        if let Err(mpsc::error::TrySendError::Full(_)) =
            self.metrics.try_send(MetricsEvent::RoomGone(id))
        {
            self.reg_metrics_dropped += 1;
        }
    }

    /// Mark a room id retired (§8), FIFO-capped (see the `retired` field).
    pub(super) fn retire_room(&mut self, id: RoomId) {
        if self.retired.insert(id, ()).is_none() {
            self.retired_order.push_back(id);
            while self.retired_order.len() > RETIRED_SET_CAP {
                let evicted = self.retired_order.pop_front().expect("len checked above");
                self.retired.remove(&evicted);
            }
        }
    }

    /// Lift a retirement (the explicit-create override; see the
    /// `CreateRoom` handler). Order-vec pruning keeps the FIFO honest.
    pub(super) fn unretire_room(&mut self, id: &RoomId) {
        self.retired.remove(id);
        self.retired_order.retain(|x| x != id);
    }

    /// Count the connections affiliated with `room` (the registry-side
    /// membership view — maintained from the join/leave reports of every
    /// room, sharded or single; the same source the sharded room-cap
    /// enforcement reads). O(connections): this is a control-plane query
    /// (rare, human-paced), not a tick-path operation.
    pub(super) fn room_members(&self, room: RoomId) -> u32 {
        self.conns
            .values()
            .filter(|info| info.room == Some(room))
            .count() as u32
    }
}
