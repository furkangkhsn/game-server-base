//! Phase 3c — KICK (BACKLOG E8): the room actor's phase, mirrored.

use std::fmt::Debug;
use std::hash::Hash;

use tracing::debug;

use crate::room::{Kick, kick_close};
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
    /// Apply `kicks`, in the order they were asked — the room actor's
    /// rules (see its `apply_kicks`). Called twice a tick: before
    /// MIGRATE (what the input and systems hooks asked: the kicked
    /// member leaves this shard's membership before the crossings are
    /// collected, so a kick never races its own migration), and after
    /// BROADCAST (what the TEAMS and BROADCAST hooks asked: a member that
    /// crossed in this tick's MIGRATE is its new owner's by then, and a
    /// kick of it here is a no-op — this shard did not own it when the
    /// hook asked).
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
                    shard = self.index,
                    %player,
                    %reason,
                    "kick of a player that is not a live member of this shard: no-op"
                );
                continue;
            };
            debug!(
                room = %self.config.id,
                shard = self.index,
                %player,
                %conn,
                %reason,
                "the game kicked a member"
            );
            self.detach_player(player, conn, &identity, true);
            if self.registry.is_some() {
                // A parked row keeps its row, not the socket's queue.
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
