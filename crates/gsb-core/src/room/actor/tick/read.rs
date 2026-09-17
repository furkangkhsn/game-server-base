//! Phase 1 — READ, and the binding translation that follows it.

use crate::room::*;
use std::fmt::Debug;
use std::hash::Hash;
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
    /// connection's actions, plus the binding translation that maps a
    /// wire session onto its stable player.
    pub(super) fn phase_read(&mut self) -> Vec<Action> {
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
        //    Consequence: the room never drops an action (`dropped_actions`
        //    stays 0). The architecture's only input-loss point is a
        //    connection's own full action channel — self-inflicted and
        //    attributed (see `conn::ConnectionActor`).
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
            if let Some(rc) = self.conns.get_mut(&self.roster[idx])
                && !rc.detached
            {
                for _ in 0..per_conn {
                    if budget == 0 {
                        break;
                    }
                    match rc.actions.try_recv() {
                        Ok(a) => {
                            budget -= 1;
                            actions.push(a);
                        }
                        Err(_) => break, // channel drained
                    }
                }
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
        //    suspenders.
        actions.retain_mut(|a| match self.binding.get(&a.conn) {
            Some(&player) => {
                a.player = player;
                true
            }
            None => {
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
