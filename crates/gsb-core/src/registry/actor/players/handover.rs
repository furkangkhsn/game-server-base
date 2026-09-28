//! Double-session supersedence (`docs/RECONNECT.md` §5, "en son kazanan"
//! — latest wins): a newer session of an identity whose old session is
//! still LIVE in the same room takes that membership over.

use std::fmt::Debug;
use std::hash::Hash;

use tracing::{debug, warn};

use crate::channel::Mailbox;
use crate::conn::{ConnIn, ServerClose};
use crate::id::{ConnectionId, RoomId};
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
    /// Before `conn`'s join of `room` as `identity` dispatches: every
    /// LIVE (still-connected, as far as this table knows) session of the
    /// identity in the room is superseded. Its socket is closed with
    /// ERROR 9 (`ConnIn::ServerClosed`), and its membership is HANDED
    /// OVER, not left (BACKLOG F32): the room, seeing the resume while the
    /// identity is still live on the old connection, runs the old
    /// session's detach itself and the resume takes the park it leaves
    /// (`crate::room::live_session`). The live row is most often a
    /// reconnect that outran the old socket's close — a half-open socket,
    /// or a close still queued — and leaving it destroyed the entity the
    /// player came back for.
    ///
    /// A DETACHED session of the identity needs nothing here: its park IS
    /// the resume target (the re-affiliation cleanup in `SpawnDone`
    /// releases it).
    pub(super) fn supersede_live(&mut self, conn: ConnectionId, room: RoomId, identity: &str) {
        if identity.is_empty() {
            return;
        }
        let mut superseded: Vec<(ConnectionId, Option<Mailbox<ConnIn>>)> = Vec::new();
        for (&other, info) in &self.conns {
            if other != conn
                && !info.detached
                && info.room == Some(room)
                && info.identity == identity
            {
                superseded.push((other, info.inbox.clone()));
            }
        }
        for (old_conn, inbox) in superseded {
            warn!(
                %old_conn,
                %conn,
                room = %room,
                %identity,
                "double session: a newer session takes the live one over \
                 (ERROR 9 to the old socket)"
            );
            if let Some(inbox) = inbox {
                let reason = "a newer session for this player superseded this \
                     connection"
                    .to_string();
                tokio::spawn(async move {
                    let _ = inbox
                        .send(ConnIn::ServerClosed {
                            cause: ServerClose::Superseded,
                            reason,
                        })
                        .await;
                });
            }
            self.hand_over(old_conn, room);
        }
    }

    /// The table half of a handover: the old row leaves the room WITHOUT a
    /// room-side leave (the entity is the newer session's now). The grid's
    /// member count loses it here, as a leave's would, and the newer
    /// session's `SpawnDone` counts the membership again — one member
    /// throughout. Not a leave (`reg_leaves` untouched): the membership
    /// goes on.
    ///
    /// The old dispatcher still knows the membership: when the old socket
    /// closes it routes a DETACH as every close does. Landing before the
    /// resume, it parks the entity (the policy's answer) and the resume
    /// consumes the park; landing after, it finds the binding moved and is
    /// a no-op. Its `DetachDone` and any `DetachDespawned` for the old
    /// connection find this row out of the room: stale echoes.
    fn hand_over(&mut self, old_conn: ConnectionId, room: RoomId) {
        let Some(info) = self.conns.get_mut(&old_conn) else {
            return;
        };
        info.room = None;
        info.entity = None;
        if let Some(e) = self.rooms.get_mut(&room).and_then(|e| e.shards.as_mut()) {
            e.members = e.members.saturating_sub(1);
        }
        debug!(%old_conn, room = %room, "membership handed over to the newer session");
    }
}
