//! Phase 0d — the input-idle ceiling sweep
//! ([`RoomConfig::max_idle_input_secs`], default OFF).

use crate::id::{ConnectionId, EntityId, PlayerId};
use crate::registry::LeaveRequest;
use std::fmt::Debug;
use std::hash::Hash;
use std::time::{Duration, Instant};
use tracing::warn;

use crate::room::actor::RoomActor;
use crate::room::{AfkAction, DisconnectCause, idle_close};

impl<W, G, Sp> RoomActor<W, G, Sp>
where
    G: Eq + Hash + Clone + Debug,
    // The strip payload's trait bounds (`GameLogic::Strip`) — restated so
    // calls through the logic object type-check; the room itself never
    // touches the payloads.
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Tick phase 0d — the input-idle ceiling.
    ///
    /// **Off by default, and then free.** The first line is the whole cost
    /// of the feature for every room that does not configure it: one
    /// `Option` test per step, nothing per member. That is deliberate —
    /// AFK is a game decision, so the base's ceiling has to be invisible
    /// until an operator asks for it.
    ///
    /// **When on:** a bounded rotation over the idle clock (at most
    /// `idle::SWEEP_BUDGET` slots per step, resuming where the last step
    /// stopped — the READ phase's rotation discipline), so the per-step
    /// cost is CONSTANT rather than proportional to the member count. A
    /// member is therefore examined within one rotation, which for a
    /// 10 000-member room at 30 Hz is ~5 s of extra latency on a ceiling
    /// measured in tens of seconds. Firing late is safe for a ceiling;
    /// firing early would not be.
    ///
    /// **What it does:** hands each expired member to
    /// [`RoomActor::detach_player`] — the SAME path a dead transport
    /// takes. The base despawns nothing itself; the game's
    /// `on_disconnect` decides park / AI handover / despawn, so there is
    /// one decision point instead of two, and a MOBA gets
    /// bot-takeover-on-AFK for free.
    ///
    /// **Who is exempt:** parked and bot-fed rows, structurally — the
    /// detach path takes them OFF the idle clock, so the sweep cannot see
    /// them at all and a hold can never be double-counted.
    ///
    /// **Then the close requests** (BACKLOG E6): whatever this sweep — or
    /// an earlier tick's, refused by a full registry mailbox — asked the
    /// registry to close goes out here (`flush_close_requests`: `try_send`,
    /// Full keeps, Closed drops), each with its `parked` re-checked
    /// (`reconcile_closes`, B41). An empty queue costs one length test.
    /// The leave requests (B40) follow the same rules, after their park
    /// keys are re-checked (`reconcile_parks`) and never ahead of a
    /// queued detach-despawn report.
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
            // Resolve the session the row currently belongs to. The
            // identity is the resume key the row remembers: without it an
            // idle-kicked player could not be parked under a ledger key,
            // and the AI-handover/reclaim story would be unreachable for
            // exactly the members this ceiling exists for.
            let Some((conn, entity, identity)) = self
                .conns
                .get(&player)
                .map(|rc| (rc.conn, rc.entity, rc.identity.clone()))
            else {
                // The clock outlived its row (it should not: every despawn
                // funnel stops the clock). Drop the stale slot.
                self.idle.stop(player);
                continue;
            };
            // Warn ONCE per room, the way the other forced ceilings do: a
            // room that is shedding idle members will shed many, and one
            // line per member per rotation would bury the signal.
            if self.idle_ceiling_warns == 0 {
                self.idle_ceiling_warns += 1;
                warn!(
                    room = %self.config.id,
                    %player,
                    %conn,
                    idle_limit_secs = limit.as_secs(),
                    afk_action = self.config.afk_action.label(),
                    "input-idle ceiling reached (max_idle_input_secs): the \
                     member is handed to the ordinary disconnect policy \
                     (on_disconnect decides park/AI-handover/despawn); \
                     afk_action = disconnect then also closes its \
                     connection. This warning is emitted once per room."
                );
            }
            // No `idle.stop` here on purpose: BOTH arms of
            // `detach_player` take the row off the clock (the despawn
            // funnel on one side, the park on the other), and duplicating
            // it here would mask a regression in either.
            // The default action settles the registry row through its
            // leave request (below), so the despawn arm's transport-death
            // report would be a second settlement of the same row.
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

    /// The default action's half of the settlement (BACKLOG B40): the
    /// membership is over and the connection stays open, so the room lets
    /// go of everything the live connection still reaches — and asks the
    /// registry to settle its row the way the member's own leave would.
    ///
    /// A despawn already dropped the row (and with it the connection's
    /// action channel). A PARK keeps its row, so the room releases both
    /// channel halves the row shares with the connection (its outbound
    /// clone would keep the socket's writer alive after the connection
    /// ends; its action receiver would swallow the connection's frames)
    /// and re-keys the park to the connection's park key: from here on it
    /// is a park whose session is gone — the transport-death shape —
    /// which nothing the live connection does next (a leave, a fresh
    /// join, its close) can touch.
    fn leave_behind(&mut self, player: PlayerId, conn: ConnectionId, entity: EntityId) {
        let parked = match self.conns.get_mut(&player) {
            Some(rc) if rc.detached => {
                rc.release_outbound();
                // The released channel's unread requests are counted
                // like a despawn's (B36).
                self.m.requests_dropped_unread += rc.release_actions();
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

    /// Re-key `player`'s parked row from `conn` to `conn.park_key()`:
    /// the binding row moves and the row's session back-reference follows
    /// (its request state was already dropped at the park). `None` — the
    /// park stays keyed by `conn` — when an earlier park of the same
    /// session still holds the key here: one park per key.
    fn rekey_park(&mut self, player: PlayerId, conn: ConnectionId) -> Option<ConnectionId> {
        let key = conn.park_key();
        if self.binding.contains_key(&key) {
            return None;
        }
        self.binding.remove(&conn);
        self.binding.insert(key, player);
        if let Some(rc) = self.conns.get_mut(&player) {
            rc.conn = key;
        }
        Some(key)
    }

    /// A queued leave request's park key is re-checked when it leaves:
    /// if the park it names already ended here (the hold ran out — its
    /// report went ahead — or another session resumed it), there is no
    /// park left to move the row to, so the request settles a despawn.
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

    /// A queued close request's `parked` is re-checked every time it
    /// tries to leave (BACKLOG B41): it stays `parked` only while this
    /// room still holds a membership of its connection — the park, the
    /// bot it was handed to, or one the still-open connection took up
    /// again (a resume or a rejoin), which that connection's own close
    /// then settles through the ordinary transport-death path.
    ///
    /// Why. A hold that ends in a despawn sends its `DetachDespawned` in
    /// phase 0c, AHEAD of this phase, so a request still waiting from an
    /// earlier tick arrives AFTER it — and the report reached a row the
    /// registry did not yet know was detached, which it drops as a stale
    /// echo. A `parked` close would then mark the row detached with
    /// nothing left to release it (its slot held for the room's life);
    /// re-checked, the request closes the despawn it now is.
    fn reconcile_closes(&mut self) {
        for req in &mut self.close_requests {
            if req.parked {
                req.parked = self.binding.contains_key(&req.conn);
            }
        }
    }
}
