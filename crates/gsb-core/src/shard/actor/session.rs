//! Session bookkeeping: rebinding a resumed player onto its parked
//! row, the one despawn funnel, and the RPC reply queue.

use std::fmt::Debug;
use std::hash::Hash;

use tokio::sync::mpsc;
use tracing::debug;

use crate::channel::{FrameBatch, Mailbox};
use crate::id::{ConnectionId, PlayerId};
use crate::room::Action;
use crate::rpc::RpcReply;

use crate::shard::actor::ShardActor;

impl<W, G, St, Sp> ShardActor<W, G, St, Sp>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
    St: Debug + Send + 'static,
    // The strip rides every exchange and view; the bounds mirror what
    // the delta protocol does with it (diff via PartialEq, clone into
    // each neighbor's message, store in the actor's maps).
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Bind a resumed session onto its parked row (the shard-side
    /// mirror of the room actor's `rebind_session`): swap the channel
    /// halves, stamp the guard epoch, move THE binding row, and hand
    /// the logic its `on_resume` hook. Returns the fresh action sender
    /// for the reply.
    ///
    /// **The RebindKey shrink (Faz 2)** — the signpost enumeration, now a
    /// list of what must NOT be touched (see the room actor for the full
    /// rationale):
    /// - `binding` — MOVED below (`old conn → new conn`, same player):
    ///   the entire re-key surface;
    /// - `conns` — NOT re-keyed: the row's key IS the stable player id;
    ///   only `RoomConn.conn` and the channel halves are rewritten;
    /// - `conn_epoch` — MOVED as part of the binding move (remove the
    ///   dead session's entry, stamp the resume epoch under the new one:
    ///   outgoing Migrates of the LIVE session must carry ITS epoch so
    ///   the leave/migration gate pairs correctly);
    /// - `conn_tombstone` — NOT touched, deliberately: a tombstone is
    ///   keyed by the id of the join that DIED. A detached session never
    ///   died (its leave was never processed), so it wrote no tombstone;
    ///   tombstoned old sessions stay under their own (dead) ids where
    ///   their guards belong;
    /// - `deferred` — in-flight `Migrate`s carry the OLD id; they are
    ///   gated by epoch/tombstones exactly as before (a post-resume ghost
    ///   of the parked session loses to the new session's newer epoch on
    ///   any later leave);
    /// - `groups` — NOT touched (opaque `G`); rebuilt wholesale from
    ///   `conns` every broadcast phase, so a per-player group key
    ///   self-heals in one tick (same argument as the room actor).
    ///
    /// The ledger goes through [`GameLogic::on_resume`] (the park
    /// metadata itself traveled INSIDE the migrated player state — §14.2
    /// — so whichever shard now owns the entity also owns the record).
    pub(crate) fn rebind_session(
        &mut self,
        player: PlayerId,
        conn: ConnectionId,
        epoch: u64,
        identity: &str,
        out: mpsc::Sender<FrameBatch>,
    ) -> Mailbox<Action> {
        let (act_tx, act_rx) = mpsc::channel(self.config.action_capacity);
        let mut old_conn = conn;
        let mut entity = 0;
        if let Some(rc) = self.conns.get_mut(&player) {
            rc.out = out;
            rc.actions = act_rx;
            rc.detached = false;
            rc.bot_fed = false;
            rc.clear_hold_clock();
            rc.session_epoch = epoch;
            rc.identity = identity.to_string();
            old_conn = rc.conn;
            rc.conn = conn;
            entity = rc.entity;
        }
        self.m.resumes += 1;
        // The clock RESTARTS with the new session (the room actor's rule).
        self.idle.start(player, std::time::Instant::now());
        // THE binding move + its session-epoch companion (see the
        // enumeration above): the old session loses both rows; the new
        // session owns the player from here on.
        let moved_epoch = self.conn_epoch.remove(&old_conn);
        self.binding.remove(&old_conn);
        self.binding.insert(conn, player);
        self.conn_epoch
            .insert(conn, epoch.max(moved_epoch.unwrap_or(0)));
        self.logic
            .on_resume(&mut self.world, identity, conn, player, entity);
        debug!(
            room = %self.config.id,
            shard = self.index,
            %old_conn,
            %conn,
            %player,
            epoch,
            "player resumed onto parked row on this shard (binding moved)"
        );
        act_tx
    }

    /// The shard's despawn funnel: remove the row, tear down its binding,
    /// run `on_leave`, count. (`on_leave` stays THE single despawn seam
    /// for snapshots and bookkeeping, exactly like the room actor.)
    pub(crate) fn despawn_conn(&mut self, player: PlayerId, count_as_leave: bool) {
        let Some(rc) = self.conns.remove(&player) else {
            return;
        };
        self.binding.remove(&rc.conn);
        self.conn_epoch.remove(&rc.conn);
        self.idle.stop(player);
        // The request state goes with the SESSION (the room actor's rule):
        // in-flight requests release their slots and any queued answer is
        // dropped (a reply to a gone session is not delivered); late
        // worker reports find no pending entry and are counted late.
        self.drop_conn_request_state(rc.conn);
        self.logic.on_leave(&mut self.world, player);
        if count_as_leave {
            self.m.leaves += 1;
        }
    }

    /// Queue one RPC answer for a connection's next (or this tick's, if
    /// broadcast has not run yet) private frame. All request paths —
    /// same-tick reply/reject, cap/duplicate rejects, the worker-report
    /// reconciliation, the timeout sweep — funnel through here, so the
    /// per-tick delivery point is exactly one. (The room actor's helper,
    /// byte-for-byte.)
    pub(crate) fn queue_reply(
        &mut self,
        conn: ConnectionId,
        id: u64,
        op: u16,
        ok: bool,
        reason: String,
        payload: bytes::Bytes,
    ) {
        self.queued.entry(conn).or_default().push(RpcReply {
            id,
            ok,
            op,
            reason,
            payload,
        });
    }

    /// Drop a connection's request state (pending set + queued answers).
    /// Called on leave, on join (a join supersedes the connection's prior
    /// state), on detach-park, and at migrate-out. Late worker reports for
    /// the dropped requests find no pending entry and are dropped by the
    /// 0b reconciliation; the workers themselves exit on their own (their
    /// report send fails against the dropped entry, or their timeout
    /// fires first). (The room actor's helper, byte-for-byte.)
    pub(crate) fn drop_conn_request_state(&mut self, conn: ConnectionId) {
        if let Some(deq) = self.pending.remove(&conn) {
            self.pending_total = self.pending_total.saturating_sub(deq.len());
        }
        self.queued.remove(&conn);
    }
}
