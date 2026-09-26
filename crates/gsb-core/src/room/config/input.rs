//! The input port a room hands each member: its bounded action channel,
//! and the rate the connection actor admits input into it at.

use std::num::NonZeroU32;

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

/// A per-connection input rate limit — [`RoomConfig::input_rate`]: a
/// token bucket of `burst` actions, refilled continuously at `per_sec`
/// actions a second (docs/SECURITY.md, "post-auth input volume").
///
/// Both numbers are positive by construction ([`InputRate::new`]): a
/// zero burst would admit nothing and a zero rate would never refill —
/// neither is a limit, and "no limit" is spelled `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputRate {
    per_sec: NonZeroU32,
    burst: NonZeroU32,
}

impl InputRate {
    /// `per_sec` actions a second, at most `burst` at once; `None` when
    /// either is zero.
    pub fn new(per_sec: u32, burst: u32) -> Option<Self> {
        Some(Self {
            per_sec: NonZeroU32::new(per_sec)?,
            burst: NonZeroU32::new(burst)?,
        })
    }

    /// The sustained rate: actions a second.
    pub fn per_sec(self) -> u32 {
        self.per_sec.get()
    }

    /// The bucket's size: the most actions admitted at one instant (and
    /// what a connection holds on its first limited join).
    pub fn burst(self) -> u32 {
        self.burst.get()
    }
}

#[cfg(test)]
mod tests;
