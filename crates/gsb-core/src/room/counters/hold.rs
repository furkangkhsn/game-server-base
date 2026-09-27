//! The detach-hold clock of one member row (`docs/RECONNECT.md` §14.4):
//! the park that starts it, when the phase-0c sweep asks the logic's
//! `may_release` veto, and the ceiling that bounds a standing veto.
//! Shared by the room and the shard actor, so their two sweeps (and
//! their two park arms) cannot drift apart.

use std::time::{Duration, Instant};

use super::RoomConn;
use crate::room::ExpireTo;

/// What the sweep does with a held row after asking its veto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HoldEnd {
    /// The veto stands and the ceiling has not passed: keep holding, ask
    /// again on the next sweep.
    Extend,
    /// No veto: the hold ends toward the policy's `ExpireTo` — the only
    /// outcome for a logic that never vetoes, on the same sweep as before
    /// the veto was asked at a deadline.
    Release(ExpireTo),
    /// The veto still stands at the ceiling
    /// ([`crate::room::RoomConfig::max_detach_hold`]): the hold ends
    /// toward its `ExpireTo` anyway (the harass-lock bound).
    Forced(ExpireTo),
}

impl<G> RoomConn<G> {
    /// Start the hold (the `Detach::Hold` arm of both actors). The clock
    /// is CORE-owned (§14.4): the grace becomes an absolute deadline and
    /// the ceiling an absolute instant, both measured from `now` — the
    /// detach. A ceiling too far out to represent is no ceiling.
    pub(crate) fn park(
        &mut self,
        grace: Option<Duration>,
        to: ExpireTo,
        ceiling: Option<Duration>,
        now: Instant,
    ) {
        self.detached = true;
        self.expire_to = to;
        self.detach_deadline = grace.map(|g| now + g);
        self.detach_ceiling = ceiling.and_then(|c| now.checked_add(c));
    }

    /// Let go of the row's outbound half (BACKLOG E6). A member the
    /// input-idle ceiling PARKED under `afk_action = disconnect` keeps its
    /// row for the park, but its still-live socket is being closed — and
    /// the writer pump ends (closing the socket) only once every sender of
    /// its queue is gone, this row's clone included. A parked row ships
    /// nothing (BROADCAST skips it) and a resume binds the new session's
    /// queue, so a closed stand-in costs nothing. (A transport death needs
    /// none of this: its writer is already gone.)
    pub(crate) fn release_outbound(&mut self) {
        let (closed, _) = tokio::sync::mpsc::channel(1);
        self.out = closed;
    }

    /// Let go of the row's input half (BACKLOG B40). A member the
    /// input-idle ceiling PARKED under `afk_action = leave_room` keeps its
    /// connection open, and that connection still holds the sender of
    /// this row's action channel: dropping the receiver closes it, so the
    /// connection's forwards stop landing in a park that never reads them
    /// (READ skips a detached row) and the connection can tell its
    /// membership is over. A resume binds a fresh channel, as always.
    ///
    /// Returns what is still unread in the released channel — RPC
    /// requests and plain actions apart — for the caller's counters (B36's
    /// ledger, B54: the idle sweep runs before READ, so input sent right
    /// before the ceiling is still there — as a despawn's `drop_unread`
    /// counts it).
    pub(crate) fn release_actions(&mut self) -> crate::room::Unread {
        let (_, closed) = tokio::sync::mpsc::channel(1);
        let mut released = std::mem::replace(&mut self.actions, closed);
        crate::room::drop_unread(&mut released)
    }

    /// Stop the hold clock: a resume re-binds the row, and an AI handover
    /// ends the hold for good (the bot-fed row is never swept again).
    pub(crate) fn clear_hold_clock(&mut self) {
        self.detach_deadline = None;
        self.detach_ceiling = None;
    }

    /// Whether the sweep asks this row's `may_release` veto now: a held
    /// row (detached, not yet bot-fed) whose grace has run out — a timed
    /// hold is asked from its deadline on, every sweep — or that has no
    /// grace at all (combat-held: asked from the first sweep).
    pub(crate) fn hold_asks(&self, now: Instant) -> bool {
        self.detached && !self.bot_fed && self.detach_deadline.is_none_or(|dl| now >= dl)
    }

    /// The held row's fate, given the logic's answer (`released` = what
    /// `may_release` returned). The ceiling is consulted only for a veto:
    /// it never ends a hold the logic would keep for its grace, and it
    /// never turns a release into a forced one.
    pub(crate) fn hold_end(&self, released: bool, now: Instant) -> HoldEnd {
        if released {
            HoldEnd::Release(self.expire_to)
        } else if self.detach_ceiling.is_some_and(|c| now >= c) {
            HoldEnd::Forced(self.expire_to)
        } else {
            HoldEnd::Extend
        }
    }
}
