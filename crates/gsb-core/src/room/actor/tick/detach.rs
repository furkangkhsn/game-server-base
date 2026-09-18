//! Phase 0c — the detach-hold sweep and the detach-despawn reports.

use crate::id::PlayerId;
use crate::room::*;
use std::fmt::Debug;
use std::hash::Hash;
use std::time::Instant;
use tokio::sync::mpsc;
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
    /// Tick phase 0c — the detach-hold sweep (`docs/RECONNECT.md` §14.4)
    /// and the detach-despawn reports the registry is waiting on.
    pub(super) fn phase_detach_sweep(&mut self) {
        // -- Phase 0c — detach-hold sweep (§14.4: the deadline clock is
        //    CORE-owned; the logic owns the policy). Two arms, exactly as
        //    resolved in §14.4:
        //
        //    - `grace = Some(d)`: the core fires its OWN deadline —
        //      `may_release` is not consulted for timed holds (the grace
        //      IS the ceiling that makes an endless veto impossible);
        //    - `grace = None` (combat-held): the core asks `may_release`
        //      every tick — the detached set is tiny (parks are rare),
        //      so the per-tick cost is a filter pass over the table that
        //      short-circuits on the `detached` flag.
        //
        //    The ended hold is handed to `on_detach_expired(to)` and then:
        //    Despawn → the ordinary despawn path (`on_leave` stays THE one
        //    despawn funnel; slot released); AiHandover → everything stays
        //    alive under a `bot_fed` marker (Tur B synthesizes the input;
        //    this is the documented seam).
        if self.conns.values().any(|rc| rc.detached && !rc.bot_fed) {
            let now = Instant::now();
            // Timed holds past their deadline + combat-helds the logic is
            // ready to release. Collected first so each logic callback runs
            // against an unborrowed `self`.
            let mut due: Vec<(PlayerId, ExpireTo)> = Vec::new();
            let mut ask: Vec<PlayerId> = Vec::new();
            for (&pid, rc) in &self.conns {
                if !rc.detached || rc.bot_fed {
                    continue;
                }
                match rc.detach_deadline {
                    Some(dl) if now >= dl => due.push((pid, rc.expire_to)),
                    Some(_) => {}
                    None => ask.push(pid),
                }
            }
            for pid in ask {
                if self.logic.may_release(&mut self.world, pid) {
                    // The veto cleared: the hold ends NOW, toward the same
                    // `ExpireTo` the policy chose at detach time.
                    let to = self
                        .conns
                        .get(&pid)
                        .map(|rc| rc.expire_to)
                        .unwrap_or(ExpireTo::Despawn);
                    due.push((pid, to));
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
                        if let Some(rc) = self.conns.get_mut(&pid) {
                            rc.bot_fed = true;
                            // The deadline must never re-fire (the row stays
                            // held by the bot); clearing it also takes the
                            // row out of the `may_release` polling set.
                            rc.detach_deadline = None;
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
}
