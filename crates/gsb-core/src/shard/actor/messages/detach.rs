//! The registry's close broadcast (`ShardMsg::Detach`,
//! `ShardMsg::DetachBy`): exactly the owning shard runs the policy, told
//! why the connection closed (BACKLOG F28); the binding + entity guards
//! make the others no-ops (the same shape as a broadcast `Leave`). The
//! room actor's `on_detach`, mirrored.

use std::fmt::Debug;
use std::hash::Hash;

use crate::conn::ServerClose;
use crate::id::{ConnectionId, EntityId};
use crate::room::DisconnectCause;
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
    /// `conn`'s membership as `entity` lost its transport: the policy is
    /// asked with [`DisconnectCause::closed`]`(verdict)` — `None` when
    /// the client ended the session.
    pub(super) fn on_detach(
        &mut self,
        conn: ConnectionId,
        entity: EntityId,
        identity: &str,
        verdict: Option<ServerClose>,
    ) {
        if let Some(&player) = self.binding.get(&conn)
            && self.conns.get(&player).map(|c| c.entity) == Some(entity)
            // A row that is ALREADY parked has had its policy run once; a
            // second Detach for it is a duplicate (the room actor's
            // guard, mirrored).
            && !self.conns.get(&player).is_some_and(|c| c.detached)
        {
            let cause = DisconnectCause::closed(verdict);
            self.detach_player(player, conn, identity, true, cause);
        }
    }
}
