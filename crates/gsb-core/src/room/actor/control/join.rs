//! Admission and rebinding: a fresh join's seat, a resume's channel
//! swap onto a live entity, and the one despawn funnel both end in.

use std::fmt::Debug;
use std::hash::Hash;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};
use crate::channel::{FrameBatch, Mailbox};
use crate::error::CoreError;
use crate::id::{ConnectionId, EntityId, PlayerId};
use crate::room::*;
use crate::room::actor::RoomActor;

impl<W, G, Sp> RoomActor<W, G, Sp>
where
    G: Eq + Hash + Clone + Debug,
    // The strip payload's trait bounds (`GameLogic::Strip`) — restated so
    // calls through the logic object type-check; the room itself never
    // touches the payloads.
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Admit a connection as a FRESH member: the exact body of the
    /// pre-reconnect `Join` arm (supersede own stale state, cap check,
    /// `on_join`, register, roster, reply) — now shared by the plain
    /// `Join` arm and the resume fallback paths, so the fallback can
    /// never drift from an ordinary join.
    pub(super) fn admit_fresh(
        &mut self,
        conn: ConnectionId,
        out: mpsc::Sender<FrameBatch>,
        reply: oneshot::Sender<Result<(EntityId, Mailbox<Action>), CoreError>>,
    ) -> bool {
        // A join supersedes any stale state this connection had
        // (e.g. a leave queued behind it in the control channel) —
        // including its request state (a rejoin is a new session:
        // in-flight requests and queued answers of the old one
        // are dropped, and their late reports are discarded).
        if let Some(&stale) = self.binding.get(&conn) {
            let rc = self.conns.remove(&stale).expect("binding implies row");
            self.roster_remove(&stale);
            self.drop_conn_request_state(conn);
            self.logic.on_leave(&mut self.world, stale);
            drop(rc);
        }
        // Capacity: the room knows its own membership — this is the
        // only place a join can structurally fail. A fresh join to a
        // full room is rejected (no entity, no channel, no state);
        // a re-join of an existing member (removed above) never
        // hits the cap because it supersedes itself.
        if let Some(cap) = self.config.max_players
            && self.conns.len() >= cap
        {
            warn!(
                room = %self.config.id,
                %conn,
                capacity = cap,
                "room full; join rejected (CoreError::RoomFull)"
            );
            let _ = reply.send(Err(CoreError::RoomFull(self.config.id.0)));
            return true;
        }
        // The LOGIC mints the stable player identity here (Faz 2) — core
        // never invents player ids.
        let admission = self.logic.on_join(&mut self.world, conn);
        self.m.joins += 1;
        let (act_tx, act_rx) = mpsc::channel(self.config.action_capacity);
        self.binding.insert(conn, admission.player);
        self.conns.insert(
            admission.player,
            RoomConn {
                conn,
                out,
                actions: act_rx,
                entity: admission.entity,
                // Authoritative value is recomputed every broadcast
                // phase (a group may depend on the world); this is
                // the join-time value.
                group: self.logic.group_of(&self.world, admission.player),
                batch: Vec::new(),
                detached: false,
                detach_deadline: None,
                expire_to: ExpireTo::Despawn,
                bot_fed: false,
                session_epoch: 0,
            },
        );
        let _ = reply.send(Ok((admission.entity, act_tx)));
        self.roster_add(admission.player);
        debug!(
            room = %self.config.id,
            %conn,
            player = %admission.player,
            entity = admission.entity,
            "player joined"
        );
        true
    }

    /// Bind a resumed session onto its parked row: swap the channel
    /// halves (§7), stamp the guard epoch, move THE binding row, and hand
    /// the logic its `on_resume` hook (ledger consumption + seq/ack
    /// reset + fresh-member mark). The wire id does not move: the reply
    /// carries the SAME entity id the original join returned (§5).
    ///
    /// **The RebindKey shrink (Faz 2).** Pre-Faz-2 this function ran a
    /// single-pass rename over every conn-keyed table (`conns`, `roster`,
    /// `roster_pos`) — §14.1's one-point discipline against the
    /// "new-table-added-later-and-forgotten" bug. With every table keyed
    /// by the stable [`PlayerId`] the rename class is GONE: a resume now
    /// updates exactly ONE row of ONE table — the binding — plus the
    /// channel halves inside the (unmoved) `conns` row. The signpost
    /// enumeration survives as the proof that nothing else was missed;
    /// today it is a list of tables that must NOT be touched:
    ///
    ///   1. `binding` — MOVED below (`old conn → new conn`, same player):
    ///      the entire re-key surface.
    ///   2. `conns` — NOT re-keyed: the row's key IS the stable player
    ///      id; only `RoomConn.conn` (its bound session back-reference)
    ///      and the channel halves are rewritten.
    ///   3. `roster` / `roster_pos` / `read_cursor` — NOT touched:
    ///      player-keyed; the parked row kept its slot in the READ
    ///      rotation the whole time (it simply had nothing to pull).
    ///   4. `pending` / `queued` — NOT re-keyed AND not moved: they stay
    ///      conn-keyed BY DESIGN (session-scoped RPC state, §11) and were
    ///      already cleared at DETACH time; listed here as CHECKED.
    ///   5. `groups` — NOT touched: `G` is opaque (it MAY embed a
    ///      per-player key) and cannot be derived generically. Safe
    ///      because the broadcast phase rebuilds the whole group table
    ///      from `conns` every tick (phase 4b): a changed group key
    ///      self-heals within one tick at the cost of one extra emission
    ///      for that group (its cached snapshot ledger is unreachable
    ///      under the old key and is dropped with it).
    ///
    /// Anything the LOGIC keys per-session goes through
    /// [`GameLogic::on_resume`] — which under Faz 2 is nearly empty for
    /// a player-keyed logic (ledger + seq reset only).
    pub(super) fn rebind_session(
        &mut self,
        player: PlayerId,
        conn: ConnectionId,
        epoch: u64,
        identity: String,
        out: mpsc::Sender<FrameBatch>,
        reply: oneshot::Sender<Result<(EntityId, Mailbox<Action>), CoreError>>,
    ) {
        // Fresh input channel for the fresh session (the old channel's
        // senders died with the old connection actor); the seq/ack
        // contract (DESIGN §14.2) makes the NEW session start from a
        // clean numbering, so the old channel object is dropped, not
        // reused.
        let (act_tx, act_rx) = mpsc::channel(self.config.action_capacity);
        let (entity, old_conn) = {
            let rc = self.conns.get_mut(&player).expect("parked row checked by caller");
            rc.out = out;
            rc.actions = act_rx;
            rc.detached = false;
            rc.bot_fed = false;
            rc.detach_deadline = None;
            rc.session_epoch = epoch;
            let old = rc.conn;
            rc.conn = conn;
            (rc.entity, old)
        };
        self.m.resumes += 1;
        // THE binding move — the whole remaining re-key surface (see the
        // enumeration above). The old session's row is removed first so a
        // stray frame under the dead conn finds no binding from here on
        // (locked by `actions_from_the_old_session_are_dropped_after_resume`).
        self.binding.remove(&old_conn);
        self.binding.insert(conn, player);
        self.logic
            .on_resume(&mut self.world, &identity, conn, player, entity);
        let _ = reply.send(Ok((entity, act_tx)));
        debug!(
            room = %self.config.id,
            %old_conn,
            %conn,
            %player,
            entity,
            epoch,
            "player resumed onto parked row (binding moved; tables untouched)"
        );
    }

    pub(in crate::room) fn despawn_conn(&mut self, player: PlayerId, count_as_leave: bool) {
        let Some(rc) = self.conns.remove(&player) else {
            return;
        };
        // Tear the session binding down with the row (the leave/detach-
        // expiry/despawn half of the binding lifecycle).
        self.binding.remove(&rc.conn);
        self.roster_remove(&player);
        // The request state goes with the SESSION: in-flight requests
        // are released (their slots free up for other connections) and
        // any queued answer is dropped (a reply to a gone session is not
        // delivered).
        self.drop_conn_request_state(rc.conn);
        self.logic.on_leave(&mut self.world, player);
        if count_as_leave {
            self.m.leaves += 1;
        }
    }
}
