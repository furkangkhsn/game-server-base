//! Connection lifecycle: the birth cap (total and unauthenticated)
//! and the death that routes a DETACH rather than deciding the
//! entity's fate here.

use std::fmt::Debug;
use std::hash::Hash;

use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tracing::{debug, warn};

use crate::channel::Mailbox;
use crate::conn::{ConnIn, ServerClose};
use crate::id::{ConnectionId, EntityId, RoomId};
use crate::registry::*;
use crate::source::Source;

use crate::registry::actor::Registry;

mod ops;
mod source;
#[cfg(test)]
mod tests;

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
    pub(super) async fn on_conn_opened(
        &mut self,
        conn: ConnectionId,
        inbox: Mailbox<ConnIn>,
        source: Option<Source>,
    ) {
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
            // Posted: in place when the inbox has room (F57). No row is
            // recorded, so no stop notice follows (F60, `Self::tell`).
            self.tell(
                conn,
                &inbox,
                ConnIn::ServerClosed {
                    cause: ServerClose::ConnCap,
                    reason: "server at connection capacity".into(),
                },
            );
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
        //
        // One source's share of that pool (D12, `source`) first: a
        // source over its own cap is refused under its own reason
        // even when the pool is full too.
        let unauthed = self.unauthed(source);
        if self.refused_per_source(conn, &inbox, source, &unauthed) {
            return;
        }
        if let Some(cap) = self.max_unauth_conns
            && unauthed.total >= cap
        {
            warn!(
                %conn,
                capacity = cap,
                "server at unauthenticated capacity; new connection rejected"
            );
            self.tell(
                conn,
                &inbox,
                ConnIn::ServerClosed {
                    cause: ServerClose::UnauthCap,
                    reason: "server at unauthenticated capacity".into(),
                },
            );
            return;
        }
        let info = self.conns.entry(conn).or_default();
        info.inbox = Some(inbox);
        info.source = source;
        self.source_cap_warned = false;
        self.reg_opens += 1;
        self.emit_metrics();
    }

    pub(super) async fn on_conn_closed(
        &mut self,
        conn: ConnectionId,
        verdict: Option<ServerClose>,
    ) {
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
            // slot cannot outlive the connection. (With no row, a
            // gone dispatcher leaves nothing to detach from here.)
            if let Some((_, op_tx)) = self.conn_ops.remove(&conn)
                && self.route_close(conn, op_tx, None, verdict)
            {
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
            Some((_, op_tx)) => {
                // The dispatcher serializes the detach behind
                // any in-flight join and reports `DetachDone`.
                // Refused: counted (B57), sampled below.
                self.route_close(conn, op_tx, room.zip(entity), verdict);
            }
            None => {
                if let (Some(room), Some(entity)) = (room, entity) {
                    self.send_detach_direct(conn, room, entity, verdict);
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

    /// Hand a closing connection's dispatcher its `Close` — and, when
    /// its queue refuses it, make sure the membership still ends (B61).
    /// `op_tx` is the dispatcher's only sender and is dropped here in
    /// every case, so its queue closes behind whatever it holds.
    ///
    /// - Full: the dispatcher is alive and will run every op queued ahead
    ///   — a join among them may leave a membership this table has not
    ///   seen yet. It treats its queue closing as the `Close` (see
    ///   `spawn_conn_ops`), so the detach still comes after them, from
    ///   the one task that knows the membership. A direct detach from
    ///   here would target a stale membership and race those ops.
    /// - Closed: the task is gone, and its view of the membership with
    ///   it. Nothing else will detach, so the table's affiliation gets
    ///   the dispatcher-less DETACH — a spawned send, never awaited here.
    ///
    /// Both are counted (B57): `close_ops_dropped` still means "the close
    /// op was not queued"; the fallback is what keeps it from being a
    /// leak. Returns whether the op was refused.
    fn route_close(
        &mut self,
        conn: ConnectionId,
        op_tx: mpsc::Sender<RoomOp<St, Sp>>,
        affiliation: Option<(RoomId, EntityId)>,
        verdict: Option<ServerClose>,
    ) -> bool {
        let Err(refused) = op_tx.try_send(RoomOp::Close { verdict }) else {
            return false;
        };
        self.reg_close_ops_dropped += 1;
        match refused {
            TrySendError::Full(_) => {
                warn!(%conn, "close op queue full; the dispatcher detaches once it drains");
            }
            TrySendError::Closed(_) => {
                warn!(%conn, "close op dispatcher gone; detaching directly");
                if let Some((room, entity)) = affiliation {
                    self.send_detach_direct(conn, room, entity, verdict);
                }
            }
        }
        true
    }
}
