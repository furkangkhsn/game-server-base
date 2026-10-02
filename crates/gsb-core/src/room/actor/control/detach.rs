//! The registry's detach of a closed connection (`RoomControl::Detach`,
//! `RoomControl::DetachBy`): the POLICY is the logic's (§3 — the
//! registry only reports the fact), told why the connection closed
//! (BACKLOG F28).

use std::fmt::Debug;
use std::hash::Hash;

use crate::conn::ServerClose;
use crate::id::{ConnectionId, EntityId};
use crate::room::DisconnectCause;
use crate::room::actor::RoomActor;

impl<W, G, Sp> RoomActor<W, G, Sp>
where
    G: Eq + Hash + Clone + Debug,
    // The strip payload's trait bounds (`GameLogic::Strip`) — restated so
    // calls through the logic object type-check; the room itself never
    // touches the payloads.
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Transport death (registry `ConnClosed` route) of `conn`'s
    /// membership as `entity`: the policy is asked with
    /// [`DisconnectCause::closed`]`(verdict)` — `None` when the client
    /// ended the session. Same stale guard as `Leave`: binding first,
    /// then the entity this player currently owns.
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
            // second Detach for it is a duplicate (the transport of an
            // idle-expired member dying later is exactly that shape) and
            // must not re-ask the policy.
            && !self.conns.get(&player).is_some_and(|c| c.detached)
        {
            let cause = DisconnectCause::closed(verdict);
            self.detach_player(player, conn, identity, true, cause);
        }
    }
}
