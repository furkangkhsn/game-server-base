//! A connection's path as its transport measures it, carried to the room
//! that plays the member (DESIGN §6 "Tıkanıklık tepkisi", BACKLOG B103).
//!
//! ```text
//! transport ──ConnIn::Path──▶ connection actor ──(member's action channel)──▶ room / shard
//!   (on change, latest wins)     (latest wins)          READ diverts it ─▶ PathTable ─▶ TickCtx
//! ```
//!
//! **Transport-agnostic.** [`PathState`] says what a transport knows about
//! one session's path; every field but the phase is optional, so each
//! transport fills what it measures: rUDP everything (its congestion
//! response), QUIC what quinn's statistics give (round trip, congestion
//! window, loss), TCP / TLS / WebSocket nothing yet — a member whose
//! transport sends no state has no path (`None`: unknown, the game sends
//! as it always did). A kernel `TCP_INFO` source for the stream doors is
//! a possible later addition, not part of this one.
//!
//! **The room's primary question is a byte budget**
//! ([`crate::room::TickCtx::budget`]): how many bytes this member's path
//! takes per tick while the transport limits it ([`PathPhase::Paced`]);
//! the whole state is readable too ([`crate::room::TickCtx::path`]).
//!
//! **Sent on change, latest wins.** A state moves only when it
//! [`PathState::moved_from`] the last one delivered (the phase changed,
//! or the rate moved by 10 % or more); a mailbox that is full keeps the
//! newest state owed ([`PathSignal`]) instead of queueing old ones — a
//! path is a state, not an event: a superseded state was never news.
//! Nothing on this path ever awaits a mailbox.
//!
//! The actor-to-room hop rides the member's own action channel as an
//! internal marker (`gsb_protocol::op::base::MEMBER_PATH`, never on the
//! wire — the connection actor refuses it from a client like any unknown
//! base opcode): the channel is the member's alone (a full one delays
//! only this member's state), it moves with the member when a shard
//! hands it over, and the room pulls it anyway — no new channel to poll.
//! The room's READ diverts the marker before the input-idle stamp and
//! before the game's ingest (`carry`).

mod carry;
mod table;

#[cfg(test)]
mod tests;

pub(crate) use carry::{path_action, read_path, settle};
pub use table::{PathTable, PathView};

use std::time::Duration;

/// Where a session's path stands, as its transport judges it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PathPhase {
    /// The path keeps up (or the transport cannot tell): nothing limits
    /// what the room sends.
    #[default]
    Open,
    /// One congestion signal: watched more closely, nothing limited yet.
    Suspect,
    /// The transport limits the session to [`PathState::rate`].
    Paced,
}

/// One session's path, as its transport last measured it: small and
/// `Copy`, so it rides a message. `None` fields are what this transport
/// does not measure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PathState {
    pub phase: PathPhase,
    /// The bytes per second the path carries while the transport limits
    /// it — `Some` only in [`PathPhase::Paced`].
    pub rate: Option<u32>,
    /// The bytes per second the session offered its path over the last
    /// measured interval.
    pub demand: Option<u32>,
    /// The loss over the last measured interval, per mille.
    pub loss_permille: Option<u16>,
    /// The latest round trip.
    pub rtt: Option<Duration>,
    /// The round trip over the path's floor: the queue the path holds.
    pub queue_delay: Option<Duration>,
}

impl PathState {
    /// The bytes the path carries per `period` (a tick, a snapshot
    /// interval) while the transport limits it; `None` — not limited, no
    /// budget known.
    pub fn budget(&self, period: Duration) -> Option<usize> {
        self.rate
            .map(|r| (f64::from(r) * period.as_secs_f64()) as usize)
    }

    /// Whether this state is news against `prev`, the last one delivered:
    /// the phase changed, a rate appeared or went, or the rate moved by
    /// at least a tenth of the old one. The measurements alone (loss,
    /// round trip, demand) ride the next state that is news.
    pub fn moved_from(&self, prev: &PathState) -> bool {
        if self.phase != prev.phase {
            return true;
        }
        match (self.rate, prev.rate) {
            (Some(a), Some(b)) => a != b && u64::from(a.abs_diff(b)) * 10 >= u64::from(b),
            (None, None) => false,
            _ => true,
        }
    }
}

/// The sending side of the change rule, for a sender whose mailbox can
/// be full (a transport into its connection actor's inbox, the actor
/// into the member's action channel): offer every new state, send the
/// one it says is owed, and report how that went. Latest wins — a state
/// that could not be sent is replaced by the next one offered, and a
/// state that is owed stays owed (retried without a new offer) until a
/// send succeeds.
#[derive(Debug, Clone, Copy, Default)]
pub struct PathSignal {
    /// The last state the receiver took.
    delivered: Option<PathState>,
    /// The newest state offered.
    newest: Option<PathState>,
    /// `newest` has not reached the receiver and is news.
    owed: bool,
}

impl PathSignal {
    /// A new state: it replaces any state still owed, and is owed itself
    /// when it is news against the last one DELIVERED (a path that went
    /// back to what the receiver already knows owes nothing).
    pub fn offer(&mut self, state: PathState) {
        self.newest = Some(state);
        self.owed = self.delivered.is_none_or(|d| state.moved_from(&d));
    }

    /// The state to send now, if one is owed.
    pub fn owed(&self) -> Option<PathState> {
        self.newest.filter(|_| self.owed)
    }

    /// The receiver took the owed state.
    pub fn delivered(&mut self) {
        self.delivered = self.newest;
        self.owed = false;
    }

    /// A new receiver (a new membership): it knows nothing, so the newest
    /// state, if any, is owed to it.
    pub fn reset(&mut self) {
        self.delivered = None;
        self.owed = self.newest.is_some();
    }
}
