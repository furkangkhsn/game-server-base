//! The game's kick verb (BACKLOG E8, `docs/RECONNECT.md` §16.3): a game
//! hook asks, through its tick context, that a member be removed from
//! the SERVER — its membership ended by the ordinary disconnect policy,
//! then its connection closed with the game's reason.
//!
//! ```text
//! game hook ──ctx.kick(player, reason)──▶ KickQueue (the tick's, local)
//!   ... the hook returns; the phase group ends ...
//! actor ── on_disconnect (Detach: park / AI / despawn) ── CloseRequest{Kicked}
//!   ... next tick, phase 0d: E6's flush ──▶ registry ──▶ ERROR 9 + close
//! ```
//!
//! **Await-free and re-entry-free.** The verb only QUEUES: the queue is
//! a `Cell` the tick context lends (the hooks take `&TickCtx`), so asking
//! costs a push and nothing in the hook's frame changes. The actor
//! applies the queue when the hooks that could ask have returned — never
//! inside the hook that asked.
//!
//! **Bounded.** The reason is cut to [`KICK_REASON_MAX_BYTES`] on a
//! `char` boundary when it is asked, so the queue and the `ERROR` frame
//! stay small whatever the game passes.

use std::cell::Cell;

use crate::conn::ServerClose;
use crate::id::{ConnectionId, EntityId, PlayerId, RoomId};
use crate::registry::CloseRequest;

/// The most bytes of a game's kick reason the engine keeps (cut on a
/// `char` boundary, so the kept text is valid UTF-8 and never splits a
/// character). The `ERROR` message is `kicked: <reason>`, at most
/// `8 + 256` bytes: a one-line, human-readable message like every other
/// `ERROR` code 9 text (the engine's own reasons run to ~100 bytes), and
/// a frame that fits one unfragmented rUDP datagram.
pub const KICK_REASON_MAX_BYTES: usize = 256;

/// One kick a game asked for: the member, and its reason as kept (already
/// bounded to [`KICK_REASON_MAX_BYTES`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kick {
    /// The member to kick (the stable key the logic's tables use).
    pub player: PlayerId,
    /// The game's reason, bounded; the client reads it after `kicked: `.
    pub reason: String,
}

/// A tick's kick requests. The room and the shard actor own one per tick
/// and lend it to the game's hooks through
/// [`TickCtx::kicks`](crate::room::TickCtx::kicks); a test that builds
/// its own context can own one too and read back what its logic asked
/// ([`Self::take`]).
#[derive(Default)]
pub struct KickQueue {
    asked: Cell<Vec<Kick>>,
}

impl KickQueue {
    /// The handle a tick context carries.
    pub fn kicks(&self) -> Kicks<'_> {
        Kicks { queue: Some(self) }
    }

    /// Everything asked so far, in order; the queue is empty afterwards.
    pub fn take(&self) -> Vec<Kick> {
        self.asked.take()
    }

    fn push(&self, kick: Kick) {
        let mut asked = self.asked.take();
        asked.push(kick);
        self.asked.set(asked);
    }
}

impl std::fmt::Debug for KickQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KickQueue").finish_non_exhaustive()
    }
}

/// The kick verb as the tick context lends it ([`TickCtx::kicks`]).
/// `Default` is the INERT handle a hand-built context carries: it
/// accepts every kick and keeps none.
///
/// [`TickCtx::kicks`]: crate::room::TickCtx::kicks
#[derive(Debug, Clone, Copy, Default)]
pub struct Kicks<'a> {
    queue: Option<&'a KickQueue>,
}

impl Kicks<'_> {
    /// Ask that `player` be kicked with `reason` (see
    /// [`TickCtx::kick`](crate::room::TickCtx::kick) for when it is
    /// applied and what the client sees).
    pub fn kick(&self, player: PlayerId, reason: impl Into<String>) {
        if let Some(queue) = self.queue {
            queue.push(Kick {
                player,
                reason: bound_reason(reason.into()),
            });
        }
    }
}

/// Cut `reason` to [`KICK_REASON_MAX_BYTES`] on a `char` boundary.
pub(crate) fn bound_reason(mut reason: String) -> String {
    if reason.len() > KICK_REASON_MAX_BYTES {
        let mut end = KICK_REASON_MAX_BYTES;
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        reason.truncate(end);
    }
    reason
}

/// The `ERROR` code 9 message of a kick: `kicked: <reason>`, or `kicked`
/// for an empty reason (the message is always populated). The prefix
/// names the verdict the way the engine's own code-9 texts do
/// (`input idle: …`, `stream rejected: …`), so a client can tell the
/// game's kick from the engine's closes.
pub fn kick_message(reason: &str) -> String {
    if reason.is_empty() {
        "kicked".to_owned()
    } else {
        format!("kicked: {reason}")
    }
}

/// The close request a kick sends for `conn` after the disconnect policy
/// ran (`parked` = the policy parked the entity).
pub(crate) fn kick_close(
    conn: ConnectionId,
    room: RoomId,
    entity: EntityId,
    parked: bool,
    reason: &str,
) -> CloseRequest {
    CloseRequest {
        conn,
        room,
        entity,
        parked,
        cause: ServerClose::Kicked,
        reason: kick_message(reason),
    }
}

#[cfg(test)]
mod tests;
