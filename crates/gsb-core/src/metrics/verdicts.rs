//! The session verdicts the server's stop kept from being carried out
//! (BACKLOG F56, re-deciding B57).
//!
//! A room (or shard) that ends a member's membership on its own tells
//! the registry, which carries the verdict out: `CloseConn` (a game's
//! kick, the input-idle ceiling's disconnect) closes the connection with
//! an `ERROR` 9 and its reason, booked in `server_closes{reason}`;
//! `LeaveConn` (the ceiling's default leave) settles the row and tells
//! the connection it is out; `DetachDespawned` (a detach that ended in a
//! despawn) releases the parked row. At the server's stop a verdict can
//! be caught on its way, in exactly one of these places, each counted
//! there:
//!
//! - still queued in the room at its stop (the registry's mailbox was
//!   full) — the room's stop;
//! - refused by the registry's mailbox, closed by its `Shutdown` arm
//!   (F53) — the room's flush;
//! - unread in the registry's mailbox behind its `Shutdown` — the
//!   registry's drain;
//! - carried out, but the connection had taken the stop's
//!   `ConnIn::Shutdown` first and the verdict was left in its inbox —
//!   the connection's end (any server verdict left there: a pump's too);
//! - posted to the connection while its inbox was full, the stop's notice
//!   reached it first and it closed its inbox before a slot freed: the
//!   send was refused — the registry's fallback sender (F58,
//!   `registry/actor/tell.rs`).
//!
//! The client then gets the stop's `ERROR` 14 instead of the verdict,
//! and `server_closes` never books it. B57 had left these uncounted
//! "because everything is torn down"; the verdict is lost all the same.
//! A verdict whose effect would have been a no-op (its connection
//! already gone) is counted too: the counters count verdicts, and no
//! place but the registry could tell.
//!
//! Each place sends what it counted as one
//! [`crate::metrics::MetricsEvent::VerdictsLost`], stop-message idiom;
//! the collector sums them into the registry slice.

use crate::conn::ServerClose;
use crate::metrics::ServerCloses;

/// Verdicts the stop kept from being carried out, by kind (see the
/// module docs).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VerdictsLost {
    /// Close verdicts, by the reason the connection would have booked in
    /// `server_closes`.
    pub closes: ServerCloses,
    /// Memberships a room ended of a connection that stays open
    /// (`LeaveConn`): the row was never settled, the connection never
    /// told.
    pub leaves: u64,
    /// Detaches that ended in a despawn (`DetachDespawned`): the parked
    /// row was never released by its report.
    pub detach_despawns: u64,
}

impl VerdictsLost {
    /// Count one lost close verdict.
    pub fn close(&mut self, reason: ServerClose) {
        self.closes.add(reason);
    }

    /// Add another set (the collector's sum).
    pub fn add(&mut self, o: &Self) {
        self.closes.add_all(&o.closes);
        self.leaves = self.leaves.saturating_add(o.leaves);
        self.detach_despawns = self.detach_despawns.saturating_add(o.detach_despawns);
    }

    /// Nothing counted: no event needs to be sent.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}
