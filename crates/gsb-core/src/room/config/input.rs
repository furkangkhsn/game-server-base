//! The input port a room hands each member: its bounded action channel.

use crate::channel::{Inbox, Mailbox};
use crate::room::{Action, RoomConfig};

impl RoomConfig {
    /// A fresh per-connection action channel of
    /// [`RoomConfig::action_capacity`] — the one constructor the room's
    /// join and resume paths and the shard's use.
    ///
    /// A capacity of `0` is one slot (BACKLOG F21): a zero-capacity
    /// bounded channel has no meaning (tokio refuses it with a panic,
    /// which killed the room actor at its first join), so it is read the
    /// way the room's control channel always read it — through
    /// [`crate::channel::channel`], which clamps to one. The server
    /// refuses `conn_action = 0` at startup; the clamp keeps a hand-built
    /// config (library use) from panicking a room.
    pub(crate) fn action_channel(&self) -> (Mailbox<Action>, Inbox<Action>) {
        crate::channel::channel(self.action_capacity)
    }
}

#[cfg(test)]
mod tests;
