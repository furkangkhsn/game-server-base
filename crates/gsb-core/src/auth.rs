//! The ticket-validation hook (control-plane auth).
//!
//! A real deployment authenticates by *delegation*: the platform
//! services (matchmaking, party, inventory) set up the match, hand each
//! client a ticket and the game server's address, and the game server's
//! job is to **validate the ticket the platform issued** — not to run
//! its own identity system. This module defines that hook; it
//! deliberately ships **no** validator implementation (a signature
//! service call, a cache lookup, … is platform-specific — like the
//! transport and the visibility strategies, an adapter's concern).
//!
//! **The hook shape.** `TicketValidator` is a `Fn(Bytes) -> Future`:
//! each call takes the presented ticket (opaque bytes) and returns an
//! *owning* future resolving to the extracted identity. The function
//! form (rather than a trait with `&self`) is what makes the future
//! `'static`: the platform's implementation captures its handles
//! (channel senders, client connections, caches) by value in the
//! closure at composition time, so the worker task the connection actor
//! spawns for the round trip carries no borrows.
//!
//! **Where the pending state lives.** In the connection actor itself,
//! as the in-flight round trip of the `AUTH_REQ` frame handler — the
//! same idiom as the join (`SpawnPlayer`): the handler spawns the
//! worker and awaits its single oneshot reply. The actor's run loop
//! keeps its one await (the inbox `recv`); the park is bounded by the
//! hook's timeout, so a hung validator cannot park a connection forever
//! (it resolves to a timeout, which is a normal rejection).
//!
//! **Failure is a normal rejection, not a violation.** A rejected
//! ticket is an *expected* reject path (a stale client after a platform
//! restart, a client whose ticket expired, a misrouted connection) —
//! the frame is well-formed and the client can fix its state by
//! re-presenting a fresh ticket. Counting it against the protocol-
//! violation budget would punish a legitimate retry and would let a
//! platform-side outage burn clients' budgets for no client fault.
//! The connection stays alive (ERROR code 10) and the budget is spent
//! only on structural protocol errors, as before.
//!
//! **Amplification bound.** While validating, the connection actor is
//! parked on the round trip, so it can have **at most one** validation
//! in flight — structurally (there is no second frame handler running
//! concurrently). Queued frames wait in the bounded inbox and are
//! processed one at a time after the park ends. The server-wide
//! exposure is therefore bounded by the connection count (the
//! `max_connections` cap), not by the ticket rate.
//!
//! **Interaction with the connection cap.** The cap is enforced at
//! connection birth (`ConnOpened`), before any auth, so a
//! connected-but-unvalidated peer occupies exactly the same budget as
//! today's pre-auth peer (actor + pumps + channels + registry entry).
//! The only new resource a validating peer holds is its in-flight
//! worker (bounded to one, lifetime-bounded by the timeout).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::id::RoomId;

/// The identity a ticket resolves to: the player identity the platform
/// encoded in the ticket, and the room the ticket pins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedTicket {
    /// The player identity extracted from the ticket (it supersedes
    /// `Auth.name`; the hook is the authority).
    pub player: String,
    /// The room the ticket pins: a later `JOIN_ROOM_REQ` naming a
    /// different room is rejected (ERROR code 11, normal rejection).
    pub room: RoomId,
}

/// Why a ticket failed validation. `Debug`/`Display` only: the error
/// text is client-visible (it goes into the ERROR frame's message) and
/// is a *normal rejection*, never a violation signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TicketError {
    /// The validator rejected the ticket (unknown, malformed, expired,
    /// revoked — the validator decides; the base carries no format).
    Rejected(String),
    /// The validation did not finish within the hook's timeout.
    TimedOut,
}

impl std::fmt::Display for TicketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(reason) => write!(f, "ticket rejected: {reason}"),
            Self::TimedOut => write!(f, "ticket validation timed out"),
        }
    }
}

/// The validator function: ticket bytes in, an owning future with the
/// resolved identity out. `Send + Sync` because the closure is shared
/// across connections (cloned into each validating connection actor);
/// the returned future is `Send` because it runs in a spawned worker.
pub type TicketValidator = Arc<
    dyn Fn(bytes::Bytes) -> Pin<Box<dyn Future<Output = Result<ValidatedTicket, TicketError>> + Send>>
        + Send
        + Sync,
>;

/// The server's ticket hook (configured per server; `None` = the legacy
/// local-auth path, where `Auth.name` is accepted as-is and every
/// existing flow is unchanged).
#[derive(Clone)]
pub struct TicketAuth {
    /// The validation function (see the type docs).
    pub validator: TicketValidator,
    /// The validation deadline: a validation that does not resolve within
    /// this window is a timeout (normal rejection, ERROR code 10) and the
    /// worker task cannot outlive it. Sized against the platform's
    /// validator latency, not the game's tick: auth is off the tick path
    /// entirely (the connection actor is not a room), so a generous value
    /// costs nothing per tick — only per stuck connection (bounded by the
    /// timeout itself).
    pub timeout: Duration,
}

// Manual `Debug`: the validator is a trait object with no `Debug` bound
// (the platform's closure may hold anything); the hook's configuration
// identity is the timeout.
impl std::fmt::Debug for TicketAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TicketAuth")
            .field("validator", &"<fn>")
            .field("timeout", &self.timeout)
            .finish()
    }
}
