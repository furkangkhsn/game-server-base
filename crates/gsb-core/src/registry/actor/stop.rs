//! Telling a room (or every shard of one) to stop, without ever parking
//! the registry on the room's mailbox.
//!
//! WHY this exists (BACKLOG §1 row 4a, the shutdown hang): a room drains
//! its control channel only on a tick. Server stop aborts the ticker right
//! after enqueueing the registry's `Shutdown`, so a room whose channel was
//! full at that moment (a disconnect burst larger than `control_capacity`
//! routes one DETACH per member into it) never drains it again. The old
//! teardown awaited a bounded `send` of `Shutdown` into exactly that
//! channel and waited forever; since the registry holds a `Ticker`, the
//! tick broadcast then never closed either, so the rooms never saw their
//! global stop signal and `ServerHandle::stop` (which waits for the
//! metrics collector, which waits for that broadcast) hung.
//!
//! The rule now: the registry itself never awaits a room's mailbox on the
//! stop paths. A `try_send` that fits is delivered in place (the room
//! processes it on its next tick, as before). A full mailbox gets the
//! message from a spawned sender instead — the same fire-and-forget idiom
//! the leave/detach routes already use — so the registry moves on and, on
//! the whole-server stop, exits and drops its `Ticker`: the broadcast
//! closes and every room exits through `Closed` (running its teardown
//! hooks exactly as a processed `Shutdown` does). The spawned sender then
//! fails against the dropped receiver and ends: no task outlives the room.
//! While the ticker keeps running (a runtime destroy, or a library user
//! that stops the registry without aborting its ticker), the room drains
//! its channel and the spawned sender delivers the `Shutdown` in order.

use std::fmt::Debug;
use std::hash::Hash;

use tokio::sync::mpsc::error::TrySendError;

use crate::channel::Mailbox;
use crate::registry::*;
use crate::room::RoomControl;
use crate::shard::ShardMsg;

use crate::registry::actor::Registry;

/// Deliver `msg` without awaiting: in place when the mailbox has room,
/// from a spawned sender when it is full, not at all when the actor is
/// already gone (a dead room needs no stop).
pub(in crate::registry) fn post_stop<T: Send + 'static>(tx: &Mailbox<T>, msg: T) {
    match tx.try_send(msg) {
        Ok(()) | Err(TrySendError::Closed(_)) => {}
        Err(TrySendError::Full(msg)) => {
            let tx = tx.clone();
            tokio::spawn(async move {
                let _ = tx.send(msg).await;
            });
        }
    }
}

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
    /// Stop the actor(s) behind a table entry the caller already removed:
    /// a single room gets one `RoomControl::Shutdown`, a sharded room one
    /// `ShardMsg::Shutdown` per shard. Synchronous — see the module docs.
    pub(in crate::registry) fn stop_room(entry: RoomEntry<St, Sp>) {
        match (entry.control, entry.shards) {
            (Some(control), _) => post_stop(&control, RoomControl::Shutdown),
            (None, Some(group)) => {
                for tx in &group.mailboxes {
                    post_stop(tx, ShardMsg::Shutdown);
                }
            }
            (None, None) => {}
        }
    }
}

#[cfg(test)]
mod tests;
