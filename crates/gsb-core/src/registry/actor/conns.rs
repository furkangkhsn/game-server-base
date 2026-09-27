//! Connection lifecycle: the birth cap (total and unauthenticated)
//! and the death that routes a DETACH rather than deciding the
//! entity's fate here.

use std::fmt::Debug;
use std::hash::Hash;

use tracing::{debug, warn};

use crate::channel::Mailbox;
use crate::conn::{ConnIn, ServerClose};
use crate::id::ConnectionId;
use crate::registry::*;

use crate::registry::actor::Registry;

mod ops;

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
    pub(super) async fn on_conn_opened(&mut self, conn: ConnectionId, inbox: Mailbox<ConnIn>) {
        // Connection cap, enforced at birth: the count lives
        // here (the connection table is the only place that
        // sees both opens and closes), so the guardrail is
        // enforced here, not in the accept loop. A rejected
        // connection is never recorded — no table entry, no
        // `reg_opens`, no room involvement — and its actor is
        // told to close itself gently (`ERROR` frame, code 9).
        if let Some(cap) = self.max_connections
            && self.conns.len() as u64 >= cap
        {
            warn!(
                %conn,
                capacity = cap,
                "server at connection capacity; new connection rejected"
            );
            tokio::spawn(async move {
                let _ = inbox
                    .send(ConnIn::ServerClosed {
                        cause: ServerClose::ConnCap,
                        reason: "server at connection capacity".into(),
                    })
                    .await;
            });
            return;
        }
        // Unauthenticated-session cap (docs/SECURITY.md §4),
        // enforced exactly like the total cap above — same
        // table, same gentle birth rejection, no room
        // involvement, no entry recorded. Only entries still in
        // `WaitingAuth` count: an authenticated entry left the
        // pool via `Authed`, and a detached entry is always
        // authenticated by construction (an affiliation
        // requires auth), so parked/resumed sessions never
        // consume unauthenticated capacity. The scan is
        // O(connections) on the open path only — a
        // control-plane-rate event, like the member-count
        // queries above.
        if let Some(cap) = self.max_unauth_conns
            && self.conns.values().filter(|i| !i.authed).count() as u64 >= cap
        {
            warn!(
                %conn,
                capacity = cap,
                "server at unauthenticated capacity; new connection rejected"
            );
            tokio::spawn(async move {
                let _ = inbox
                    .send(ConnIn::ServerClosed {
                        cause: ServerClose::UnauthCap,
                        reason: "server at unauthenticated capacity".into(),
                    })
                    .await;
            });
            return;
        }
        let info = self.conns.entry(conn).or_default();
        info.inbox = Some(inbox);
        self.reg_opens += 1;
        self.emit_metrics();
    }

    pub(super) async fn on_conn_closed(&mut self, conn: ConnectionId) {
        // The connection actor is gone for good. The entity's
        // fate is no longer decided HERE (the old code removed
        // the affiliation and sent a despawn-causing leave):
        // the registry only reports the fact and routes a
        // DETACH — the ROOM's policy (`on_disconnect`)
        // decides despawn-vs-hold (§3/§4). Until the policy
        // answers, this entry stays with `detached` set and
        // its inbox dropped (nothing can notify a dead
        // socket): a parked player keeps holding its cap slot
        // in this table exactly as it does in the room's.
        //
        // An UNAFFILIATED close still removes the entry
        // outright (nothing to park, nothing to hold).
        let Some(info) = self.conns.get_mut(&conn) else {
            // The registry never recorded this connection — it
            // was rejected at connection capacity. Its actor
            // still reports the close; there is no entry to
            // remove and nothing to count. A dispatcher slot
            // can still exist if the client raced a JOIN in
            // before its `ServerClosed` was processed (the room
            // may have accepted it for a tick): drain it so the
            // slot cannot outlive the connection.
            if let Some(op_tx) = self.conn_ops.remove(&conn)
                && op_tx.try_send(RoomOp::Close).is_err()
            {
                self.close_op_dropped(conn);
                self.emit_metrics();
            }
            debug!(%conn, "close of unregistered connection");
            return;
        };
        let affiliated = info.room.is_some();
        info.detached = affiliated;
        info.inbox = None;
        let (room, entity) = (info.room, info.entity);
        match self.conn_ops.remove(&conn) {
            Some(op_tx) => {
                // The dispatcher serializes the detach behind
                // any in-flight join and reports `DetachDone`.
                // Refused (its queue full or the task gone), the
                // detach is lost: counted (B57), sampled below.
                if op_tx.try_send(RoomOp::Close).is_err() {
                    self.close_op_dropped(conn);
                }
            }
            None => {
                if let (Some(room), Some(entity)) = (room, entity) {
                    self.send_detach_direct(conn, room, entity);
                    // Sharded room: NO member decrement — the
                    // parked entity still holds its slot (§4).
                }
            }
        }
        if !affiliated {
            // Nothing to park: the entry has no future reader
            // (no resume can target it), so it goes, exactly
            // like the old code's unconditional removal.
            self.conns.remove(&conn);
        }
        self.reg_closes += 1;
        self.emit_metrics();
        debug!(
            %conn,
            ?room,
            "connection closed (detach routed; affiliation held for \
             the park)"
        );
    }

    /// A close op the dispatcher never received (B57): counted and
    /// warned. The dispatcher drains what it has and exits without the
    /// detach, so the room keeps the row until the room itself ends —
    /// pathological (a 16-deep per-connection op queue), and a follow-up
    /// in BACKLOG, not handled here.
    fn close_op_dropped(&mut self, conn: ConnectionId) {
        self.reg_close_ops_dropped += 1;
        warn!(%conn, "close op queue full or dispatcher gone; the detach is lost");
    }
}
