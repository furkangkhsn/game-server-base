//! The input a session sent that its room never read (BACKLOG B36, B54).
//!
//! A session's action channel can still hold input when the session
//! ends: the CONTROL phase runs before READ, so a leave landing in the
//! same tick window as the client's last frames removes the row before
//! the pull sees them (and a parked row is not pulled at all). That
//! input was never processed — the session is gone, so an answer has
//! nowhere to go and running a request would apply a side effect for a
//! player who left. B36 counted the RPC requests (the RPC ledger,
//! `docs/RPC-CONTROL-PLANE.md` §8.3, could not close without them); B54
//! counts the plain game actions beside them too, apart.
//!
//! READ's binding translation has its own drop, counted the same two
//! ways ([`RoomCounters::count_unbound`]): an action pulled under a
//! connection that no longer has a binding row (a stale session).

use crate::channel::Inbox;
use crate::room::{Action, RoomCounters};
use crate::rpc::RPC_REQ_OP;

/// What a session left unread in its action channel, by kind: RPC
/// requests and plain game actions (disjoint).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(crate) struct Unread {
    pub(crate) requests: u64,
    pub(crate) actions: u64,
}

/// Close a session's action channel for good and count what is still
/// unread in it, requests apart from plain actions. Closing first stops
/// any late send, so the drain is bounded by the channel's capacity.
/// The caller adds the count to its counters
/// ([`RoomCounters::count_unread`]) and drops the channel.
pub(crate) fn drop_unread(actions: &mut Inbox<Action>) -> Unread {
    actions.close();
    let mut unread = Unread::default();
    while let Ok(a) = actions.try_recv() {
        if a.op == RPC_REQ_OP {
            unread.requests += 1;
        } else {
            unread.actions += 1;
        }
    }
    unread
}

impl RoomCounters {
    /// Add what a session left unread: `requests_dropped_unread` (B36)
    /// and `actions_dropped_unread` (B54).
    pub(crate) fn count_unread(&mut self, u: Unread) {
        self.requests_dropped_unread += u.requests;
        self.actions_dropped_unread += u.actions;
    }

    /// Count one action READ pulled under a connection with no binding
    /// row, by kind (B54): an RPC request is `requests_dropped_unbound`
    /// (a term of the RPC ledger), anything else
    /// `actions_dropped_unbound`.
    pub(crate) fn count_unbound(&mut self, op: u16) {
        if op == RPC_REQ_OP {
            self.requests_dropped_unbound += 1;
        } else {
            self.actions_dropped_unbound += 1;
        }
    }
}
