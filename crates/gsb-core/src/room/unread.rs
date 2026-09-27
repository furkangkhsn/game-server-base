//! The requests a session sent that its room never read (BACKLOG B36).
//!
//! A session's action channel can still hold requests when the session
//! ends: the CONTROL phase runs before READ, so a leave landing in the
//! same tick window as the client's last requests removes the row
//! before the pull sees them (and a parked row is not pulled at all).
//! Those requests were neither processed nor answered — the session is
//! gone, so an answer has nowhere to go and running the request would
//! apply a side effect for a player who left — but before this counter
//! they ended in no bucket at all, and the RPC ledger
//! (`docs/RPC-CONTROL-PLANE.md` §8.2) could not close.

use crate::channel::Inbox;
use crate::room::Action;

/// Close a session's action channel for good and count the RPC
/// requests still unread in it (the plain game actions beside them go
/// uncounted, as they always did). Closing first stops any late send,
/// so the drain is bounded by the channel's capacity. The caller adds
/// the count to its `requests_dropped_unread` and drops the channel.
pub(crate) fn drop_unread_requests(actions: &mut Inbox<Action>) -> u64 {
    actions.close();
    let mut unread = 0;
    while let Ok(a) = actions.try_recv() {
        if a.op == crate::rpc::RPC_REQ_OP {
            unread += 1;
        }
    }
    unread
}
