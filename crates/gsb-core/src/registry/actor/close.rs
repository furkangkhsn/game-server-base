//! The registry's side of the room→registry close verb (BACKLOG E6):
//! settle the row the ended membership leaves behind, then tell the
//! connection.

use std::fmt::Debug;
use std::hash::Hash;

use tracing::debug;

use crate::conn::ConnIn;
use crate::id::{ConnectionId, RoomId};
use crate::registry::actor::Registry;
use crate::registry::{CloseRequest, ConnInfo, LeaveRequest};

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
            self.settle_ended(conn, room);
        }
        debug!(%conn, room = %room, parked, ?cause, "room asked for the connection's close");
        if let Some(inbox) = inbox {
            tokio::spawn(async move {
                let _ = inbox.send(ConnIn::ServerClosed { cause, reason }).await;
            });
        }
    }

    /// [`crate::registry::RegistryMsg::LeaveConn`] (BACKLOG B40): the
    /// keep-the-socket sibling of [`Self::on_close_conn`]. The room ended
    /// the membership on its own and the connection stays open, so the
    /// row must end up exactly where the member's own `LEAVE_ROOM_REQ`
    /// would have put it — authenticated, in no room — and the connection
    /// is told it is out ([`ConnIn::LeftRoom`], never awaited; no wire
    /// bytes), so its next join goes straight through.
    ///
    /// A PARK must keep holding its slot and stay resumable, but it can
    /// no longer live on the connection's own row: that row now belongs
    /// to a session that may join again, anywhere. The room re-keyed the
    /// parked row to the connection's park key, and the registry mirrors
    /// it: the membership moves to a detached row under that key — the
    /// very shape a transport death leaves behind, released by the same
    /// two events (the hold's `DetachDespawned`, a resume's
    /// re-affiliation cleanup) and by the room ending.
    pub(super) fn on_leave_conn(&mut self, req: LeaveRequest) {
        let LeaveRequest {
            conn,
            room,
            entity,
            park,
        } = req;
        let Some(info) = self.conns.get(&conn) else {
            debug!(%conn, room = %room, "leave request for a released connection: no-op");
            return;
        };
        if info.room != Some(room) || info.entity != Some(entity) {
            debug!(%conn, room = %room, entity, "stale leave request: no-op");
            return;
        }
        let transport_gone = info.detached;
        let inbox = if transport_gone {
            None
        } else {
            info.inbox.clone()
        };
        match park.filter(|key| !self.conns.contains_key(key)) {
            Some(key) => self.move_to_park_row(conn, key),
            // Despawned — or a park whose key a still-open park row of
            // the same session holds (another room): the membership lets
            // go of its slot like a leave; the park itself stays
            // resumable room-side.
            None => self.settle_ended(conn, room),
        }
        debug!(%conn, room = %room, ?park, "room ended the membership; the connection stays");
        if let Some(inbox) = inbox {
            tokio::spawn(async move {
                let _ = inbox.send(ConnIn::LeftRoom { room }).await;
            });
        }
    }

    /// Move `conn`'s (guarded) affiliation to a new detached row under
    /// the park `key`. No count changes: the park carries the member slot
    /// the membership held. A row whose transport is already gone had
    /// nothing left but that affiliation, so it goes.
    fn move_to_park_row(&mut self, conn: ConnectionId, key: ConnectionId) {
        let Some(info) = self.conns.get_mut(&conn) else {
            return;
        };
        let parked = ConnInfo {
            room: info.room.take(),
            entity: info.entity.take(),
            inbox: None,
            identity: info.identity.clone(),
            authed: true,
            detached: true,
        };
        if info.detached {
            self.conns.remove(&conn);
        }
        self.conns.insert(key, parked);
        self.emit_metrics();
    }

    /// A membership of `conn` in `room` ended in a despawn (guarded by
    /// the caller): the affiliation goes, the sharded member slot comes
    /// back and the leave is counted (the despawn went through the room's
    /// ordinary leave funnel). A row whose transport already died — its
    /// `ConnClosed` came first and kept the row for a park that never
    /// was — is removed: nothing else would ever release it.
    fn settle_ended(&mut self, conn: ConnectionId, room: RoomId) {
        if let Some(info) = self.conns.get_mut(&conn) {
            let transport_gone = info.detached;
            info.room = None;
            info.entity = None;
            if transport_gone {
                self.conns.remove(&conn);
            }
        }
        if let Some(e) = self.rooms.get_mut(&room).and_then(|e| e.shards.as_mut()) {
            e.members = e.members.saturating_sub(1);
        }
        self.reg_leaves += 1;
        self.emit_metrics();
    }
}
