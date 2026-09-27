//! Phase 3b — KICK (BACKLOG E8): the kicks the game's hooks asked for
//! through the tick context, applied once those hooks have returned.

use std::fmt::Debug;
use std::hash::Hash;

use tracing::debug;

use crate::room::actor::RoomActor;
use crate::room::{Kick, kick_close};

impl<W, G, Sp> RoomActor<W, G, Sp>
where
    G: Eq + Hash + Clone + Debug,
    // The strip payload's trait bounds (`GameLogic::Strip`) — restated so
    // calls through the logic object type-check; the room itself never
    // touches the payloads.
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Apply `kicks`, in the order they were asked. Called twice a tick
    /// (after SYSTEMS, and after BROADCAST); an empty list — every tick
    /// of a game that never kicks — costs one length test.
    ///
    /// Each kick of a LIVE member takes the path the input-idle ceiling
    /// takes under `afk_action = Disconnect` (E6): the game's
    /// `on_disconnect` decides the entity's fate, then — with a registry
    /// — the room queues the close request (flushed in the next tick's
    /// phase 0d, E6's rules). Anyone else is a no-op, logged at `debug`
    /// and not counted: an unknown player, one already gone (a leave, a
    /// despawn, an earlier kick of this tick), a parked or bot-fed row
    /// (no live connection is its own — a transport death already ended
    /// it, or the AI plays it).
    pub(super) fn apply_kicks(&mut self, kicks: Vec<Kick>) {
        for Kick { player, reason } in kicks {
            let Some((conn, entity, identity)) = self
                .conns
                .get(&player)
                .filter(|rc| !rc.detached)
                .map(|rc| (rc.conn, rc.entity, rc.identity.clone()))
            else {
                debug!(
                    room = %self.config.id,
                    %player,
                    %reason,
                    "kick of a player that is not a live member here: no-op"
                );
                continue;
            };
            debug!(room = %self.config.id, %player, %conn, %reason, "the game kicked a member");
            self.detach_player(player, conn, &identity, true);
            if self.registry.is_some() {
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
                    .push(kick_close(conn, self.config.id, entity, parked, &reason));
            }
        }
    }
}
