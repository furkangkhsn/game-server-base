//! The registry's side of the room→registry close verb (BACKLOG E6):
//! settle the row the ended membership leaves behind, then tell the
//! connection.

use std::fmt::Debug;
use std::hash::Hash;

use tracing::debug;

use crate::conn::ConnIn;
use crate::registry::CloseRequest;
use crate::registry::actor::Registry;

impl<W, G, St, Sp> Registry<W, G, St, Sp>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
    St: Debug + Send + 'static,
    // The strip payload's trait bounds (`GameLogic::Strip`) — the
    // registry never inspects payloads, but both actor shapes it spawns
    // require them.
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// [`crate::registry::RegistryMsg::CloseConn`]: one table lookup, no
    /// room round trip (the room already ran the disconnect policy — the
    /// request says how it ended), and the verdict sent on a spawned task
    /// like every other registry-side close.
    ///
    /// Why the table is settled HERE rather than left to the close that
    /// follows. The room ended this membership on its own — no
    /// `RoomControl::Detach` preceded it — so the row is still a LIVE
    /// affiliation. Left alone, the connection's own close would mark it
    /// detached and route a DETACH the room ignores (the binding is gone,
    /// or the row is already parked): a despawned member's row would then
    /// stand forever with its `max_connections` slot (and, on the grid,
    /// its `ShardGroup` member slot), since nothing ever reports the end
    /// of a park that does not exist. Settled here, every order of this
    /// request, the connection's `ConnClosed` and the room's
    /// `DetachDespawned` converges to the same end state.
    pub(super) fn on_close_conn(&mut self, req: CloseRequest) {
        let CloseRequest {
            conn,
            room,
            entity,
            parked,
            cause,
            reason,
        } = req;
        let Some(info) = self.conns.get_mut(&conn) else {
            debug!(%conn, room = %room, "close request for a released connection: no-op");
            return;
        };
        if info.room != Some(room) || info.entity != Some(entity) {
            // The membership the room ended is not this row's current
            // one (it left, rejoined elsewhere or as a new entity).
            debug!(%conn, room = %room, entity, "stale close request: no-op");
            return;
        }
        let inbox = info.inbox.clone();
        if parked {
            // The park holds its slot (§4): the row stays, marked the way
            // a transport death marks it, so the hold's end
            // (`DetachDespawned`) or a resume's re-affiliation releases it.
            info.detached = true;
        } else {
            let transport_gone = info.detached;
            info.room = None;
            info.entity = None;
            if transport_gone {
                // Its `ConnClosed` came first and kept the row for a park
                // that never was: nothing else would ever release it.
                self.conns.remove(&conn);
            }
            // The membership left: hand the sharded member slot back and
            // count the leave (the despawn went through the room's
            // ordinary leave funnel).
            if let Some(e) = self.rooms.get_mut(&room).and_then(|e| e.shards.as_mut()) {
                e.members = e.members.saturating_sub(1);
            }
            self.reg_leaves += 1;
            self.emit_metrics();
        }
        debug!(%conn, room = %room, parked, ?cause, "room asked for the connection's close");
        if let Some(inbox) = inbox {
            tokio::spawn(async move {
                let _ = inbox.send(ConnIn::ServerClosed { cause, reason }).await;
            });
        }
    }
}
