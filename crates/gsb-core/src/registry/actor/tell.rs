//! The registry's notices to a connection, and how a verdict among them
//! that the stop overtakes is counted once (BACKLOG F58, F60).
//!
//! Every notice goes the stop-message way (`crate::channel::post`, F57):
//! in place when the connection's inbox has room — ahead of anything the
//! registry sends it later, the stop's `ConnIn::Shutdown` above all —
//! and from a spawned sender only when the inbox is full. That sender
//! waits for a slot. At the stop, the stop's own notice (spawned too)
//! can reach the connection first: the verdict — decided before the
//! stop, since the registry handles its mailbox in order — then never
//! reaches the client.
//!
//! Where is that loss counted, once? F58 counted it where the spawned
//! send was refused, when the registry had stopped by then. But the
//! refusing side does not know how the session ended: a connection whose
//! end ALSO found a verdict behind the stop counted that one too (two for
//! one session), and a session whose client ended first (or that another
//! verdict ended) lost nothing, yet was counted (BACKLOG F60). Only the
//! connection knows its end. So the registry records the verdict it could
//! not queue in place (`ConnInfo::verdict_in_flight`) and its stop says
//! so ([`ConnIn::ShutdownOvertaking`]); the connection that reads THAT
//! stop counts the verdict as its one lost verdict (its
//! `abandon_inbox`). A connection that read the verdict first ended with
//! it (booked, not lost) and never reads the stop; one that ended on its
//! own reads neither. A refused spawned send is therefore never counted:
//! whichever way the session ended, its end has counted what it lost.

use std::fmt::Debug;
use std::hash::Hash;

use crate::channel::{Mailbox, Posted};
use crate::conn::ConnIn;
use crate::id::ConnectionId;
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
    /// Post the notice `msg` to `conn`'s `inbox`, never awaited. A
    /// verdict the full inbox could not take in place is recorded on the
    /// connection's row, for the stop's notice (module docs). A
    /// connection with no row (refused at birth) is never told of the
    /// stop: it reads the verdict or ends on its own, losing nothing.
    pub(super) fn tell(&mut self, conn: ConnectionId, inbox: &Mailbox<ConnIn>, msg: ConnIn) {
        let verdict = msg.verdict();
        if crate::channel::post_where(inbox, msg) == Posted::Spawned
            && let Some(verdict) = verdict
            && let Some(info) = self.conns.get_mut(&conn)
        {
            info.verdict_in_flight.get_or_insert(verdict);
        }
    }
}
