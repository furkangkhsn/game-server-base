//! Phase 0d — the shard's input-idle ceiling sweep and the disconnect
//! path both it and `ShardMsg::Detach` run (the room actor's
//! `phase_idle_sweep` / `detach_player`, mirrored).

use std::fmt::Debug;
use std::hash::Hash;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use crate::id::{ConnectionId, EntityId, PlayerId};
use crate::registry::LeaveRequest;
use crate::room::{AfkAction, Detach, DisconnectCause, idle_close};

use crate::shard::actor::ShardActor;

impl<W, G, St, Sp> ShardActor<W, G, St, Sp>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
    St: Debug + Send + 'static,
    // The strip rides every exchange and view; the bounds mirror what
    // the delta protocol does with it.
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// THE disconnect path on a shard: ask the policy, run its arm. Two
    /// callers, one decision point — `ShardMsg::Detach` (a dead
    /// transport) and the input-idle ceiling (a live transport that
    /// stopped playing). The room actor's `detach_player`, mirrored;
    /// callers own the guards. `report` = queue the detach-despawn report
    /// on the despawn arm (the ceiling's default action settles the row
    /// through its leave request instead, B40). `cause` = which caller
    /// this is, handed to the policy (BACKLOG F27).
    pub(crate) fn detach_player(
        &mut self,
        player: PlayerId,
        conn: ConnectionId,
        identity: &str,
        report: bool,
        cause: DisconnectCause,
    ) {
        match self
            .logic
            .on_disconnect_with(&mut self.world, player, identity, cause)
        {
            Detach::Despawn => {
                // Today's close semantics, plus the registry report — the
                // room actor's arm mirrored (see it for the full
                // argument). A declined park never starts a hold, so no
                // phase-0c sweep can ever end it; on the grid the
                // unreported row also keeps a `ShardGroup` member slot,
                // the only whole-room capacity view there is. Flushed in
                // this same tick's phase 0c.
                if report && self.registry.is_some() {
                    self.despawn_reports.push(conn);
                }
                self.despawn_conn(player, false);
            }
            Detach::Hold { grace, to } => {
                // Park: keep row (stable key)/entity/slot and the binding
                // row; core owns the clock (§14.4). The dead session's
                // in-flight requests die with it (RECONNECT §11).
                let ceiling = self.config.max_detach_hold;
                let rc = self.conns.get_mut(&player).expect("guarded by caller");
                rc.park(grace, to, ceiling, crate::ticker::now());
                // OFF the input-idle clock while parked (the room actor's
                // rule): the row has no live input source, so the ceiling
                // must not fire on top of a hold. Resume restarts it.
                self.idle.stop(player);
                self.drop_conn_request_state(conn);
                debug!(
                    room = %self.config.id,
                    shard = self.index,
                    %conn,
                    %player,
                    ?grace,
                    ?to,
                    ?cause,
                    "player detached on shard (entity parked)"
                );
            }
        }
    }

    /// Tick phase 0d — the input-idle ceiling
    /// ([`crate::room::RoomConfig::max_idle_input_secs`], default OFF).
    /// The room actor's phase, mirrored: free when unset (one `Option`
    /// test per step), a constant-cost bounded rotation when set, and on
    /// expiry the SAME disconnect path a dead transport takes.
    /// Then the close requests go out (E6, their `parked` re-checked —
    /// B41 — the room actor's rules), and the leave requests (B40 — the
    /// room actor's rules, mirrored).
    pub(super) fn phase_idle_sweep(&mut self, now: Instant) {
        if let Some(limit) = self.config.max_idle_input() {
            self.expire_idle(now, limit);
        }
        let hold_back = !self.despawn_reports.is_empty();
        if !hold_back {
            self.reconcile_parks();
        }
        self.reconcile_closes();
        if let Some(registry) = &self.registry {
            crate::registry::flush_close_requests(registry, &mut self.close_requests);
            if !hold_back {
                crate::registry::flush_leave_requests(registry, &mut self.leave_requests);
            }
        }
    }

    /// The ceiling itself: every member due this step goes to the
    /// disconnect path.
    fn expire_idle(&mut self, now: Instant, limit: Duration) {
        let mut due: Vec<PlayerId> = Vec::new();
        self.idle.sweep_due(now, limit, &mut due);
        for player in due {
            let Some((conn, entity, identity)) = self
                .conns
                .get(&player)
                .map(|rc| (rc.conn, rc.entity, rc.identity.clone()))
            else {
                self.idle.stop(player);
                continue;
            };
            if self.idle_ceiling_warns == 0 {
                self.idle_ceiling_warns += 1;
                warn!(
                    room = %self.config.id,
                    shard = self.index,
                    %player,
                    %conn,
                    idle_limit_secs = limit.as_secs(),
                    afk_action = self.config.afk_action.label(),
                    "input-idle ceiling reached (max_idle_input_secs): the \
                     member is handed to the ordinary disconnect policy \
                     (on_disconnect decides park/AI-handover/despawn); \
                     afk_action = disconnect then also closes its \
                     connection. This warning is emitted once per shard."
                );
            }
            // No `idle.stop` here: both arms of `detach_player` already
            // take the row off the clock (the room actor's rule). The
            // default action settles the registry row through its leave
            // request (B40), so the despawn arm reports nothing.
            let leave = self.config.afk_action == AfkAction::LeaveRoom;
            self.detach_player(player, conn, &identity, !leave, DisconnectCause::IdleInput);
            if leave && self.registry.is_some() {
                self.leave_behind(player, conn, entity);
            }
            // The ACTION (E6): under `Disconnect` the connection goes
            // too — asked AFTER the policy ran, so the request can say
            // whether the entity was parked (the registry then keeps the
            // row for the park) or despawned. Queued only when there IS
            // a registry; flushed at the end of this phase.
            if self.config.afk_action == AfkAction::Disconnect && self.registry.is_some() {
                // A parked row keeps its row, not the socket's queue:
                // the socket closes only once every sender is gone.
                let parked = match self.conns.get_mut(&player) {
                    Some(rc) if rc.detached => {
                        rc.release_outbound();
                        true
                    }
                    _ => false,
                };
                self.close_requests
                    .push(idle_close(conn, self.config.id, entity, parked, limit));
            }
        }
    }

    /// The default action's half of the settlement (B40) — the room
    /// actor's `leave_behind`, mirrored: release the parked row's channel
    /// halves, re-key the park, queue the leave request.
    fn leave_behind(&mut self, player: PlayerId, conn: ConnectionId, entity: EntityId) {
        let parked = match self.conns.get_mut(&player) {
            Some(rc) if rc.detached => {
                rc.release_outbound();
                rc.release_actions();
                true
            }
            _ => false,
        };
        let park = if parked {
            self.rekey_park(player, conn)
        } else {
            None
        };
        self.leave_requests.push(LeaveRequest {
            conn,
            room: self.config.id,
            entity,
            park,
        });
    }

    /// Re-key the parked row to `conn.park_key()` (the room actor's rule),
    /// moving the session-epoch entry with the binding the way a resume
    /// does: the park's migrations keep pairing with its own join, and a
    /// later leave of the live connection tombstones only that connection.
    fn rekey_park(&mut self, player: PlayerId, conn: ConnectionId) -> Option<ConnectionId> {
        let key = conn.park_key();
        if self.binding.contains_key(&key) {
            return None;
        }
        self.binding.remove(&conn);
        self.binding.insert(key, player);
        if let Some(epoch) = self.conn_epoch.remove(&conn) {
            self.conn_epoch.insert(key, epoch);
        }
        if let Some(rc) = self.conns.get_mut(&player) {
            rc.conn = key;
        }
        Some(key)
    }

    /// A queued leave request whose park already ended here settles a
    /// despawn (the room actor's rule).
    fn reconcile_parks(&mut self) {
        for req in &mut self.leave_requests {
            if let Some(key) = req.park {
                let held = self
                    .binding
                    .get(&key)
                    .and_then(|p| self.conns.get(p))
                    .is_some_and(|rc| rc.detached);
                if !held {
                    req.park = None;
                }
            }
        }
    }

    /// A queued close request stays `parked` only while this shard
    /// still holds a membership of its connection (the room actor's
    /// rule, B41; see it for why). A park that migrated away while its
    /// request waited reads as ended here: the request closes a despawn,
    /// and the registry stops counting a park that still lives on the
    /// neighbour — an under-count until that park ends or is resumed,
    /// never a row that nothing releases.
    fn reconcile_closes(&mut self) {
        for req in &mut self.close_requests {
            if req.parked {
                req.parked = self.binding.contains_key(&req.conn);
            }
        }
    }
}
