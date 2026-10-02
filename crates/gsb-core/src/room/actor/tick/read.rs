//! Phase 1 — READ, and the binding translation that follows it.

use crate::room::*;
use std::fmt::Debug;
use std::hash::Hash;
use std::time::Instant;
use tracing::debug;

use crate::room::actor::RoomActor;

impl<W, G, Sp> RoomActor<W, G, Sp>
where
    G: Eq + Hash + Clone + Debug,
    // The strip payload's trait bounds (`GameLogic::Strip`) — restated so
    // calls through the logic object type-check; the room itself never
    // touches the payloads.
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Tick phase 1 — READ: the bounded, rotating pull of each live
    /// connection's actions, the input-idle stamp that pull produces, and
    /// the binding translation that maps a wire session onto its stable
    /// player.
    ///
    /// `now` is the tick's own wall clock (the ticker's `at`), so the
    /// phase needs no clock read of its own.
    pub(super) fn phase_read(&mut self, now: Instant) -> Vec<Action> {
        // -- Phase 1 — READ: pull each connection's actions (non-blocking;
        //    per-connection isolation — one flooder only fills its own
        //    channel), bounded twice:
        //
        //    (a) per-connection per-tick budget
        //    (`max_actions_per_conn_per_tick`): a single connection cannot
        //    consume more than this of the tick's pull budget. This is the
        //    *fairness* cut — before it, the merged list's overflow dropped
        //    the oldest entries regardless of owner, so one flooding
        //    connection could push other connections' earlier input out of
        //    a tick. Now a flooder's excess stays in its own bounded
        //    channel: it is pulled on later ticks (deferred), and if the
        //    flooder outpaces the budget sustainedly its own `try_send`
        //    hits the full channel and drops *its own* newest input,
        //    counted by the connection actor and attributed to it.
        //
        //    (b) room-level pull budget (`max_pending_actions`): the total
        //    number of actions pulled this tick. It bounds the tick's
        //    ingest cost — the measured 10k-connection wall is the room's
        //    serial step path, and a mass flood must not be able to
        //    exceed it. It is a *pull* bound, not a drop bound: when it is
        //    exhausted the room simply pulls no more this tick; the
        //    remainder waits in the senders' bounded channels.
        //
        //    (c) deterministic rotation: the scan does NOT walk the
        //    `conns` HashMap — its iteration order is arbitrary but fixed
        //    within a run, so under a sustained overload that fixed
        //    hash-order prefix would consume the entire pull budget on
        //    every tick while the tail connections were never REACHED
        //    (deferred ≠ ever delivered). Instead the scan follows
        //    `roster` (join order) from a rotating cursor advanced past
        //    every connection examined, so every connection is reached
        //    within one full rotation (`roster.len()` READ phases) no
        //    matter how much input the connections ahead of it hold.
        //    This is the reach-side sibling of (a): (a) bounds how much
        //    ONE connection can take from a tick; (c) guarantees what is
        //    left is shared around, not taken by the same fixed prefix
        //    every tick.
        //
        //    Consequence: the pull never drops an action for want of
        //    budget. That is why there is NO room-scope "input over
        //    budget" counter: it could only ever report 0. The flooding
        //    loss point is a connection's own full action channel —
        //    self-inflicted, counted at the drop site
        //    (`conn::ConnectionActor`) and exported at the net scope as
        //    `gsb_net_actions_dropped_total` with per-connection
        //    attribution. What the room DOES drop unprocessed is counted
        //    where it happens (B36, B54): input still unread when a
        //    session ends (`crate::room::drop_unread`), and input from an
        //    unbound connection (the translation below).
        let per_conn = self.config.max_actions_per_conn_per_tick;
        let mut budget = self.config.max_pending_actions;
        let mut actions: Vec<Action> = Vec::new();
        // The rotating scan (see (c) above): start at the cursor over the
        // join-order roster, visit connections until either a full
        // rotation is done or the pull budget ran out, then advance the
        // cursor past every connection EXAMINED — so the next READ
        // resumes exactly where this one stopped. Same shape as the
        // hash-order walk it replaces (O(visited), no allocation, no
        // sort); only the start position moves.
        let n = self.roster.len();
        let mut visited = 0usize;
        let mut idx = if n == 0 { 0 } else { self.read_cursor % n };
        while visited < n && budget > 0 {
            // Roster and table are kept in sync by the control path, so
            // the entry is always present; a plain lookup (no unwrap)
            // keeps hypothetical drift a skip, not a panic. A DETACHED
            // row is skipped (§3.2): its input source is dead — nothing
            // pulls from it — but the visit still counts toward the
            // rotation so the cursor's fairness contract is untouched.
            let player = self.roster[idx];
            let mut pulled = 0usize;
            if let Some(rc) = self.conns.get_mut(&player)
                && !rc.detached
            {
                for _ in 0..per_conn {
                    if budget == 0 {
                        break;
                    }
                    match rc.actions.try_recv() {
                        Ok(a) => {
                            budget -= 1;
                            // The path marker (B103) is the transport's
                            // news, not the player's input: it sets the
                            // member's path and never reaches the idle
                            // stamp or the game (`crate::path`).
                            if let Some(path) = crate::path::read_path(&a) {
                                crate::path::settle(&mut self.paths, player, path);
                                continue;
                            }
                            pulled += 1;
                            actions.push(a);
                        }
                        Err(_) => break, // channel drained
                    }
                }
            }
            // -- The INPUT-IDLE stamp. This pull IS the structural
            //    definition of "action-bearing" (see `crate::room::IdleView`):
            //    a frame moves this clock exactly when the connection
            //    actor forwarded it here as an `Action`. A heartbeat is
            //    answered inside the connection actor and never reaches
            //    this channel, so a heartbeat-only client stays connected
            //    and goes input-idle — which is the whole point of the
            //    signal. Cost: ONE stamp per member that actually
            //    delivered input this tick; a silent member costs nothing
            //    at all, so the bookkeeping does not scale with idle
            //    players.
            if pulled > 0 {
                self.idle.touch(player, now);
            }
            visited += 1;
            idx += 1;
            if idx == n {
                idx = 0;
            }
        }
        self.read_cursor = self.read_cursor.wrapping_add(visited);

        // -- Phase 1.5 — BINDING TRANSLATION (Faz 2): the wire protocol is
        //    unchanged — every action still names its transport session
        //    (`Action.conn`) — but the world is keyed by stable player
        //    identity. The binding table is the ONE authority for the
        //    conn ↔ PlayerId context: each action's session is translated
        //    to its player here, before any logic sees the action.
        //
        //    An unbound conn DROPS here: today's stale-action path (the
        //    game ingest silently skipping actions of conns not in the
        //    table) mirrored — the drop just moved to where the binding
        //    actually lives. This is what makes a resumed player safe: the
        //    old session's binding row was removed at the rebind, so a
        //    stray frame sent under the OLD conn after resume can never
        //    reach the world (locked by
        //    `actions_from_the_old_session_are_dropped_after_resume`).
        //    Structurally such strays are already rare — the old channel's
        //    receiver died with the rebind — this is the belt under that
        //    suspenders. The drop is counted by kind (B54): an RPC request
        //    in `requests_dropped_unbound` (a term of the RPC ledger), a
        //    plain action in `actions_dropped_unbound`.
        actions.retain_mut(|a| match self.binding.get(&a.conn) {
            Some(&player) => {
                a.player = player;
                true
            }
            None => {
                self.m.count_unbound(a.op);
                debug!(
                    room = %self.config.id,
                    conn = %a.conn,
                    op = a.op,
                    "action dropped: connection not bound (stale/old session)"
                );
                false
            }
        });

        actions
    }
}
