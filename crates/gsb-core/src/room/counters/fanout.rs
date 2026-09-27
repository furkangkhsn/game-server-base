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
