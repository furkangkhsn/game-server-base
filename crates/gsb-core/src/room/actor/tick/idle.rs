//! Phase 0d — the input-idle ceiling sweep
//! ([`RoomConfig::max_idle_input_secs`], default OFF).

use crate::id::PlayerId;
use std::fmt::Debug;
use std::hash::Hash;
use std::time::{Duration, Instant};
use tracing::warn;

use crate::room::actor::RoomActor;
use crate::room::{AfkAction, idle_close};

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
    /// Full keeps, Closed drops). An empty queue costs one length test.
    pub(super) fn phase_idle_sweep(&mut self, now: Instant) {
        if let Some(limit) = self.config.max_idle_input() {
            self.expire_idle(now, limit);
        }
        if let Some(registry) = &self.registry {
            crate::registry::flush_close_requests(registry, &mut self.close_requests);
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
            self.detach_player(player, conn, &identity);
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
}
