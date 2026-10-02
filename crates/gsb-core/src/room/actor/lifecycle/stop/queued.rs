//! What a stopping room's control channel still holds (BACKLOG B68): the
//! connection ops queued behind the `Shutdown` (or queued when the
//! ticker closed), never processed. A child of the room's stop.

use std::fmt::Debug;
use std::hash::Hash;

use crate::id::{ConnectionId, EntityId};
use crate::room::RoomControl;
use crate::room::actor::RoomActor;

impl<W, G, Sp> RoomActor<W, G, Sp>
where
    G: Eq + Hash + Clone + Debug,
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Close the control channel (a later send fails at its sender) and
    /// count what it holds, by op: a `Join`/`Resume` was never admitted
    /// (its dropped reply makes the dispatcher answer `RoomGone`); a
    /// `Leave`/`Detach` only when it would have acted here — the stale
    /// guards of `handle_control` — since a stale one loses nothing. A
    /// later `Join`/`Resume` is counted by its dispatcher (B75); a later
    /// `Leave`/`Detach` nowhere — this stop has ended the member (F55).
    pub(in crate::room) fn count_queued_ops(&mut self) {
        self.control_rx.close();
        while let Ok(op) = self.control_rx.try_recv() {
            match op {
                RoomControl::Join { .. } => self.m.stop.joins_unprocessed += 1,
                RoomControl::Resume { .. } => self.m.stop.resumes_unprocessed += 1,
                RoomControl::Leave { conn, entity } => {
                    if self.member(conn, entity).is_some() {
                        self.m.stop.leaves_unprocessed += 1;
                    }
                }
                RoomControl::Detach { conn, entity, .. }
                | RoomControl::DetachBy { conn, entity, .. } => {
                    if self.member(conn, entity) == Some(false) {
                        self.m.stop.detaches_unprocessed += 1;
                    }
                }
                RoomControl::Shutdown => {}
            }
        }
    }

    /// Whether `conn` is bound here to a row owning `entity` — and if so,
    /// whether that row is parked.
    fn member(&self, conn: ConnectionId, entity: EntityId) -> Option<bool> {
        let player = self.binding.get(&conn)?;
        let rc = self.conns.get(player)?;
        (rc.entity == entity).then_some(rc.detached)
    }
}
