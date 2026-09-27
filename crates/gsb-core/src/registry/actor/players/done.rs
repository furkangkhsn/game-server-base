//! Settling a dispatched join when the room answers: the capacity
//! reservation, the member count, and the resume-supersedence cleanup
//! that releases the parked row an identity just replaced.

use crate::conn::ConnIn;
use crate::id::{ConnectionId, EntityId, RoomId};
use crate::registry::actor::Registry;
use std::fmt::Debug;
use std::hash::Hash;
use tracing::debug;

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
    pub(in crate::registry) async fn on_spawn_done(
        &mut self,
        conn: ConnectionId,
        room: RoomId,
        entity: EntityId,
        generation: u64,
    ) {
        // Ordering note (supervision): a settled join races the
        // death report of the room incarnation it was dispatched
        // against — both travel to us over our own mailbox, in
        // no guaranteed order. The dispatcher echoes the
        // generation its handle was stamped with, so one
        // comparison decides: an ABSENT room, or a room of a
        // DIFFERENT incarnation (destroyed and even rebuilt in
        // the meantime), must not receive this affiliation.
        // Recording it would resurrect the exact zombie the
        // death watch exists to kill (status `Running` forever,
        // a member that never learns the room is gone) or pin a
        // dead join onto the rebuilt room. Either order of the
        // two messages now converges to the same end state: the
        // connection is told `RoomGone` (it holds senders into
        // a dead task) and stays unaffiliated (it may rejoin).
        if self.rooms.get(&room).map(|e| e.generation) != Some(generation) {
            let notify = match self.conns.get_mut(&conn) {
                Some(info) => {
                    info.room = None;
                    info.entity = None;
                    info.inbox.clone()
                }
                None => None,
            };
            if let Some(inbox) = notify {
                tokio::spawn(async move {
                    let _ = inbox.send(ConnIn::RoomGone(room)).await;
                });
            }
            debug!(
                %conn,
                room = %room,
                "join settled after its room died; affiliation dropped"
            );
            return;
        }
        // Ordered per-connection (from the dispatcher). If the
        // connection is unknown it died mid-join; the
        // dispatcher's Close already cleaned up the room side.
        // (The `info` borrow is scoped inside the `match` so the
        // `&mut self` `emit_metrics` call below does not conflict
        // with it.)
        let (new_affiliation, lost) = match self.conns.get_mut(&conn) {
            Some(info) => {
                let fresh = info.room != Some(room);
                let lost = info.room.filter(|&r| r != room);
                info.room = Some(room);
                info.entity = Some(entity);
                (fresh, lost)
            }
            None => (false, None),
        };
        // A join into ANOTHER room while the row is still affiliated
        // means a membership ended without the registry being told yet:
        // a leave always settles before the next join (one dispatcher,
        // in order), so only a room-side end whose report is still in
        // flight — the input-idle ceiling's `LeaveConn` behind a full
        // mailbox (B40) — gets here. That report will find the row
        // moved on and do nothing, so the ended membership's member slot
        // is handed back here; left alone, the grid's cap would count it
        // for the room's lifetime.
        if let Some(old) = lost {
            if let Some(e) = self.rooms.get_mut(&old).and_then(|e| e.shards.as_mut()) {
                e.members = e.members.saturating_sub(1);
            }
            self.reg_leaves += 1;
            debug!(%conn, room = %old, "join elsewhere settled an unreported end");
        }
        // Resume re-affiliation cleanup: the NEW session's
        // ConnectionId replaced the parked one — release the OLD
        // detached entry for this identity (the room-side park is
        // consumed by the rebind; holding the registry entry any
        // longer would leak a slot). Exactly one such entry can
        // exist (one identity, one park); a scan keeps this
        // correct without a second index.
        if new_affiliation {
            let identity = self
                .conns
                .get(&conn)
                .map(|i| i.identity.clone())
                .unwrap_or_default();
            if !identity.is_empty() {
                let mut released = 0u64;
                let stale_keys: Vec<ConnectionId> = self
                    .conns
                    .iter()
                    .filter(|(c, i)| {
                        **c != conn && i.detached && i.identity == identity && i.room == Some(room)
                    })
                    .map(|(c, _)| *c)
                    .collect();
                for old in stale_keys {
                    self.conns.remove(&old);
                    released += 1;
                    debug!(
                        %old,
                        %conn,
                        room = %room,
                        %identity,
                        "resume re-affiliation released the detached \
                         entry"
                    );
                }
                // The sharded member counter nets out here: each
                // released entry held a count; the resumed session's
                // `fresh` increment below replaces exactly one of
                // them. (A fallback FRESH join after an expiry also
                // lands here with zero releases — its +1 is the
                // genuine new member.)
                if released > 0
                    && let Some(e) = self.rooms.get_mut(&room).and_then(|e| e.shards.as_mut())
                {
                    e.members = e.members.saturating_sub(released);
                }
            }
        }
        // Sharded room: settle the capacity reservation (the
        // join was dispatched against the cap) and, when the
        // affiliation is new, count the member (a re-join of an
        // already-affiliated connection is not a new member —
        // the same semantics as a single room's cap).
        if let Some(e) = self.rooms.get_mut(&room).and_then(|e| e.shards.as_mut()) {
            e.pending = e.pending.saturating_sub(1);
            if new_affiliation {
                e.members += 1;
            }
        }
        if self.conns.contains_key(&conn) {
            // (Counted per SpawnDone for a live connection —
            // the pre-sharding semantics; a re-join re-counts,
            // as before.)
            self.reg_joins += 1;
            self.emit_metrics();
            debug!(%conn, room = %room, %entity, "player spawned");
        }
    }
}
