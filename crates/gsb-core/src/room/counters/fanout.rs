//! The fan-out's failed sends, split by cause (BACKLOG B32). Shared by
//! the room and the shard actor, so their two BROADCAST phases cannot
//! drift apart on what a drop is.
//!
//! A per-connection `try_send` fails two ways. A FULL outbound channel is
//! the slow client — the batch is lost to a live connection, and that is
//! what `dropped_frames` (`gsb_room_dropped_total`) says. A CLOSED one is
//! a connection already gone (typically the client closed its socket
//! right after its LEAVE result, and the room learns of the leave on a
//! later tick): nothing the client wanted is lost, so it is counted apart
//! (`sends_closed`) instead of reading as a slow client. Either way the
//! actor's handling of the batch is the same.

use std::collections::HashMap;

use tokio::sync::mpsc::error::TrySendError;

use super::RoomCounters;
use crate::channel::FrameBatch;
use crate::id::ConnectionId;
use crate::rpc::RpcReply;

/// One BROADCAST phase's tally of failed per-connection sends: two
/// integer adds on the failure path, settled once at the end of the
/// phase.
#[derive(Debug, Default)]
pub(crate) struct SendFailures {
    full: u64,
    closed: u64,
}

impl SendFailures {
    /// Count one failed send by its cause and hand the batch back (its
    /// buffer is reused on the next tick).
    pub(crate) fn count(&mut self, e: TrySendError<FrameBatch>) -> FrameBatch {
        match e {
            TrySendError::Full(batch) => {
                self.full += 1;
                batch
            }
            TrySendError::Closed(batch) => {
                self.closed += 1;
                batch
            }
        }
    }

    /// Add the phase's tally to the actor's counters.
    pub(crate) fn settle(self, m: &mut RoomCounters) {
        m.dropped_frames += self.full;
        m.sends_closed += self.closed;
    }
}

/// The answers still owed in `queued` (B53): what the fan-out's closing
/// sweep discards when a connection left the table the same tick. Walks
/// only the leftover entries — the fan-out removed every visited one —
/// and runs only when any are left.
pub(crate) fn undelivered(queued: &HashMap<ConnectionId, Vec<RpcReply>>) -> u64 {
    queued.values().map(|owed| owed.len() as u64).sum()
}

/// One batch's shipped traffic, counted only once the outbound channel
/// takes the batch (B57): `shipped_frames` / `shipped_bytes` /
/// `private_frames` mean frames that left for the connection — a batch
/// the channel refused is a failed send ([`SendFailures`]), not traffic.
/// Integer adds on the stack; settled on the success path.
#[derive(Debug, Default)]
pub(crate) struct Shipped {
    frames: u64,
    private: u64,
    bytes: u64,
}

impl Shipped {
    /// One frame of `len` payload bytes rides the batch.
    pub(crate) fn frame(&mut self, len: usize, private: bool) {
        self.frames += 1;
        self.private += u64::from(private);
        self.bytes = self.bytes.saturating_add(len as u64);
    }

    /// The channel took the batch: count it.
    pub(crate) fn settle(self, m: &mut RoomCounters) {
        m.shipped_frames += self.frames;
        m.private_frames += self.private;
        m.shipped_bytes = m.shipped_bytes.saturating_add(self.bytes);
    }
}

/// The path budget's gate on one member's group frame (BACKLOG B103):
/// a member whose transport limits it (`TickCtx::budget` is known) asks
/// the logic ([`GameLogic::ship_snapshot`](crate::room::GameLogic::ship_snapshot))
/// whether this tick's frame of `bytes` goes; a frame the logic withholds
/// is counted (`snapshots_withheld`). Every other member ships without a
/// question — and a room with no measured path pays one empty-table
/// check per member. Shared by the room and the shard actor.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ships_group<W, L>(
    logic: &mut L,
    world: &mut W,
    ctx: &crate::room::TickCtx,
    player: crate::id::PlayerId,
    group: &L::GroupKey,
    bytes: usize,
    m: &mut RoomCounters,
) -> bool
where
    L: crate::room::GameLogic<W> + ?Sized,
{
    let Some(budget) = ctx.budget(player) else {
        return true;
    };
    let ships = logic.ship_snapshot(world, ctx, player, group, bytes, budget);
    m.snapshots_withheld += u64::from(!ships);
    ships
}
