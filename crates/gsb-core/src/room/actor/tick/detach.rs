//! Phase 0c — the detach-hold sweep and the detach-despawn reports.

use crate::id::PlayerId;
use crate::room::*;
use std::fmt::Debug;
use std::hash::Hash;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::room::actor::RoomActor;

impl<W, G, Sp> RoomActor<W, G, Sp>
where
    G: Eq + Hash + Clone + Debug,
    // The strip payload's trait bounds (`GameLogic::Strip`) — restated so
    // calls through the logic object type-check; the room itself never
    // touches the payloads.
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Tick phase 0c — the detach-hold sweep (`docs/RECONNECT.md` §14.4)
    /// and the detach-despawn reports the registry is waiting on.
    pub(super) fn phase_detach_sweep(&mut self) {
        // -- Phase 0c — detach-hold sweep (§14.4: the clock is CORE-owned;
        //    the logic owns the policy). The logic's `may_release` veto is
        //    asked about every held row whose grace has run out — a timed
        //    hold (`grace = Some(d)`) from its deadline on, an untimed
        //    (combat-held) one from its first sweep — once per sweep. A
        //    veto extends the hold to the next sweep; a veto still standing
        //    at the row's ceiling (`RoomConfig::max_detach_hold` after the
        //    detach) is overridden: the harass-lock bound. A logic that
        //    never vetoes therefore ends every hold on the same sweep as
        //    before the veto was asked at a deadline. Bounded: at most one
        //    ask per held row per tick, the held set is small (parks are
        //    rare) and short-circuits on the `detached` flag.
        //
        //    The ended hold is handed to `on_detach_expired(to)` and then:
        //    Despawn → the ordinary despawn path (`on_leave` stays THE one
        //    despawn funnel; slot released); AiHandover → everything stays
        //    alive under a `bot_fed` marker (Tur B synthesizes the input;
        //    this is the documented seam).
        if self.conns.values().any(|rc| rc.detached && !rc.bot_fed) {
            let now = crate::ticker::now();
            // Collected first so each logic callback runs against an
            // unborrowed `self`.
            let ask: Vec<PlayerId> = self
                .conns
                .iter()
                .filter(|(_, rc)| rc.hold_asks(now))
                .map(|(&pid, _)| pid)
                .collect();
            let mut due: Vec<(PlayerId, ExpireTo)> = Vec::new();
            for pid in ask {
                let released = self.logic.may_release(&mut self.world, pid);
                let Some(rc) = self.conns.get(&pid) else {
                    continue;
                };
                match rc.hold_end(released, now) {
                    HoldEnd::Extend => {}
                    HoldEnd::Release(to) => due.push((pid, to)),
                    HoldEnd::Forced(to) => {
                        self.warn_detach_ceiling(pid, rc.conn);
                        self.m.detach_forced += 1;
                        due.push((pid, to));
                    }
                }
            }
            for (pid, to) in due {
                self.logic.on_detach_expired(&mut self.world, pid, to);
                match to {
                    ExpireTo::Despawn => {
                        self.m.detach_expired_despawn += 1;
                        // The registry is holding a detached row (and a
                        // cap slot) for this session; the despawn below
                        // is the event that ends it, and this room is the
                        // only actor that sees it happen. Queued only when
                        // there IS a registry — a standalone room has no
                        // reader, so the queue must not accumulate.
                        if self.registry.is_some()
                            && let Some(conn) = self.conns.get(&pid).map(|rc| rc.conn)
                        {
                            self.despawn_reports.push(conn);
                        }
                        self.despawn_conn(pid, false);
                        debug!(room = %self.config.id, %pid, "detach hold expired: despawn");
                    }
                    ExpireTo::AiHandover => {
                        self.m.detach_expired_ai += 1;
                        // A bot-fed row stays OFF the input-idle clock
                        // (the park already took it off): the bot's input
                        // is synthesized inside the logic's `ingest` and
                        // never crosses an action channel, so it is not
                        // input by the structural definition — and an
                        // AI-handover must not become an AFK bypass.
                        self.idle.stop(pid);
                        if let Some(rc) = self.conns.get_mut(&pid) {
                            // The marker takes the row out of the sweep for
                            // good (the bot holds it); the clock goes too.
                            rc.bot_fed = true;
                            rc.clear_hold_clock();
                        }
                        debug!(
                            room = %self.config.id,
                            %pid,
                            "detach hold expired: AI handover (bot_fed; Tur B seam)"
                        );
                    }
                }
            }
        }

        // -- Detach-despawn reports: hand the registry back the rows (and
        //    cap slots) whose detaches ended in a despawn this tick, plus
        //    anything an earlier tick could not place. BOTH producers feed
        //    this one queue — the sweep above (a hold that ran out) and
        //    the CONTROL phase's `Detach::Despawn` arm (a policy that
        //    declined to park at all), which runs earlier in this same
        //    tick. Synchronous `try_send` — the tick body stays await-free
        //    — and whatever the mailbox refuses stays queued for the next
        //    tick rather than being dropped (a dropped report IS the leak
        //    this closes).
        //    A CLOSED mailbox (the registry is gone — the process is
        //    coming down) drops the report instead of retrying forever:
        //    there is no table left to leak into.
        if !self.despawn_reports.is_empty()
            && let Some(registry) = &self.registry
        {
            let room = self.config.id;
            self.despawn_reports.retain(|&conn| {
                matches!(
                    registry.try_send(crate::registry::RegistryMsg::DetachDespawned { conn, room }),
                    Err(mpsc::error::TrySendError::Full(_))
                )
            });
        }
    }

    /// Warn ONCE per room that the veto ceiling overrode a standing
    /// `may_release` veto (the other forced ceilings' rule: a logic that
    /// keeps vetoing past it does so for many holds, and one line per
    /// hold would bury the signal). The count is the observable.
    fn warn_detach_ceiling(&mut self, player: PlayerId, conn: crate::id::ConnectionId) {
        if self.detach_ceiling_warns == 0 {
            self.detach_ceiling_warns += 1;
            warn!(
                room = %self.config.id,
                %player,
                %conn,
                max_detach_hold = ?self.config.max_detach_hold,
                "detach-hold ceiling reached (max_detach_hold): a standing \
                 may_release veto was overridden and the hold ended toward \
                 its ExpireTo. This warning is emitted once per room."
            );
        }
    }
}
