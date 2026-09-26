//! Phase 0c — the detach-hold sweep and the detach-despawn reports.

use std::fmt::Debug;
use std::hash::Hash;

use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::id::PlayerId;
use crate::room::{ExpireTo, HoldEnd};

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
    /// Tick phase 0c — the detach-hold sweep and the detach-despawn reports
    /// the registry is waiting on.
    pub(crate) fn phase_detach_sweep(&mut self) {
        // -- Phase 0c — detach-hold sweep: the shard-side mirror of the
        //    room actor's (§14.4 — core owns the clock; the `may_release`
        //    veto is asked once per sweep about every held row whose grace
        //    has run out, a veto extends the hold, a veto still standing at
        //    the row's `max_detach_hold` ceiling is overridden; the ended
        //    hold goes to `on_detach_expired` and then despawns or turns
        //    bot-fed). Runs BEFORE READ so an expired row is gone before
        //    this tick's pulls.
        if self.conns.values().any(|rc| rc.detached && !rc.bot_fed) {
            let now = crate::ticker::now();
            let ask: Vec<PlayerId> = self
                .conns
                .iter()
                .filter(|(_, rc)| rc.hold_asks(now))
                .map(|(&player, _)| player)
                .collect();
            let mut due: Vec<(PlayerId, ExpireTo)> = Vec::new();
            for player in ask {
                let released = self.logic.may_release(&mut self.world, player);
                let Some(rc) = self.conns.get(&player) else {
                    continue;
                };
                match rc.hold_end(released, now) {
                    HoldEnd::Extend => {}
                    HoldEnd::Release(to) => due.push((player, to)),
                    HoldEnd::Forced(to) => {
                        self.warn_detach_ceiling(player, rc.conn);
                        self.m.detach_forced += 1;
                        due.push((player, to));
                    }
                }
            }
            for (player, to) in due {
                self.logic.on_detach_expired(&mut self.world, player, to);
                match to {
                    ExpireTo::Despawn => {
                        self.m.detach_expired_despawn += 1;
                        // The registry is holding a detached row for this
                        // session — and on the grid that row also holds a
                        // slot in the ShardGroup member count, the only
                        // whole-room capacity view there is. Report the
                        // end so both come back.
                        if self.registry.is_some()
                            && let Some(conn) = self.conns.get(&player).map(|rc| rc.conn)
                        {
                            self.despawn_reports.push(conn);
                        }
                        self.despawn_conn(player, false);
                        debug!(
                            room = %self.config.id,
                            shard = self.index,
                            %player,
                            "detach hold expired: despawn"
                        );
                    }
                    ExpireTo::AiHandover => {
                        self.m.detach_expired_ai += 1;
                        if let Some(rc) = self.conns.get_mut(&player) {
                            rc.bot_fed = true;
                            rc.clear_hold_clock();
                        }
                        debug!(
                            room = %self.config.id,
                            shard = self.index,
                            %player,
                            "detach hold expired: AI handover (bot_fed; Tur B seam)"
                        );
                    }
                }
            }
        }

        // -- Detach-despawn reports: hand the registry back the rows (and
        //    the member slots) whose detaches ended in a despawn this
        //    tick, plus anything an earlier tick could not place. BOTH
        //    producers feed this one queue — the sweep above (a hold that
        //    ran out) and the `ShardMsg::Detach` handler's
        //    `Detach::Despawn` arm (a policy that declined to park),
        //    drained earlier in this same tick. Synchronous `try_send`
        //    (the tick body stays await-free); a FULL mailbox keeps the id
        //    queued for the next tick instead of dropping it, a CLOSED one
        //    drops it (the registry is gone — no table left to leak into).
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

    /// Warn ONCE per shard that the veto ceiling overrode a standing
    /// `may_release` veto (the room actor's `warn_detach_ceiling`,
    /// mirrored).
    fn warn_detach_ceiling(&mut self, player: PlayerId, conn: crate::id::ConnectionId) {
        if self.detach_ceiling_warns == 0 {
            self.detach_ceiling_warns += 1;
            warn!(
                room = %self.config.id,
                shard = self.index,
                %player,
                %conn,
                max_detach_hold = ?self.config.max_detach_hold,
                "detach-hold ceiling reached (max_detach_hold): a standing \
                 may_release veto was overridden and the hold ended toward \
                 its ExpireTo. This warning is emitted once per shard."
            );
        }
    }
}
