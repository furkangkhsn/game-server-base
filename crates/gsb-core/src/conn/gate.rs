//! The per-connection input gate (docs/SECURITY.md, "post-auth input
//! volume"; BACKLOG E1): an opt-in token bucket over a connection's valid
//! game-band input, checked in the connection actor BEFORE the action
//! costs the room anything.
//!
//! **Where, and why here.** The connection actor already sees every frame
//! of exactly one connection, owns its state without sharing, and is the
//! point where an action becomes the room's cost (the `try_send` into the
//! room's bounded action channel). A frame refused here is never queued,
//! pulled, stamped or ingested; the room's per-tick pull budget
//! (`RoomConfig::max_actions_per_conn_per_tick`) is fairness among the
//! input that DID enter, not a volume limit — checking there would spend
//! the room's tick on the very input the limit exists to keep out.
//!
//! **Whose number.** The ROOM's: a limit is a gameplay parameter
//! (`RoomConfig::input_rate`), and a server's rooms may run different
//! modes and rates. The registry hands it to the connection with the
//! action channel on every join (`crate::registry::Seat`), and the gate
//! re-tunes — it never resets: tokens accrue with TIME, not with room
//! hops, so a leave/join loop through any room cannot buy a fresh burst.
//!
//! **Cost.** O(1), no allocation, no timer task: the level is computed
//! from the time elapsed since the last arrival, on arrival.

use std::time::Instant;

use crate::room::InputRate;

/// One action, in the bucket's unit (nano-tokens): refill is
/// `elapsed_ns × per_sec` of these — exact integer arithmetic, no float
/// and no rounding drift.
const TOKEN: u128 = 1_000_000_000;

/// A token bucket: `burst` actions, refilled at `per_sec`.
#[derive(Debug)]
pub(crate) struct InputBucket {
    rate: InputRate,
    /// Nano-tokens held, `<= burst × TOKEN`.
    level: u128,
    /// The latest arrival (or re-tune) the level is computed up to.
    last: Instant,
}

impl InputBucket {
    /// A bucket holding its whole burst at `now`.
    pub(crate) fn full(rate: InputRate, now: Instant) -> Self {
        Self {
            rate,
            level: capacity(rate),
            last: now,
        }
    }

    /// Take one action's token at `now`: `true` = admit, `false` = the
    /// bucket is empty (the arrival costs nothing, and takes nothing).
    pub(crate) fn admit(&mut self, now: Instant) -> bool {
        self.refill(now);
        if self.level >= TOKEN {
            self.level -= TOKEN;
            true
        } else {
            false
        }
    }

    /// Switch to `rate` at `now`: refill at the old rate up to `now`,
    /// then refill at the new one. Every refill clamps to the CURRENT
    /// burst, so a smaller burst takes tokens away at the next arrival
    /// and a larger one grants none.
    fn retune(&mut self, rate: InputRate, now: Instant) {
        self.refill(now);
        self.rate = rate;
    }

    /// Bring the level up to `now`. An earlier instant (never produced by
    /// a monotonic clock) refills nothing and does not rewind `last`.
    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_nanos();
        let gained = elapsed.saturating_mul(u128::from(self.rate.per_sec()));
        self.level = self.level.saturating_add(gained).min(capacity(self.rate));
        self.last = self.last.max(now);
    }
}

/// A full bucket, in nano-tokens.
fn capacity(rate: InputRate) -> u128 {
    u128::from(rate.burst()) * TOKEN
}

/// The connection's gate: off until a limited room is joined; then the
/// joined room's limit, over ONE bucket kept for the connection's life.
#[derive(Debug, Default)]
pub(crate) struct InputGate {
    /// Created on the first limited join, never dropped after it: an
    /// unlimited room turns the gate off without forgetting the level.
    bucket: Option<InputBucket>,
    /// The room joined last limits input.
    on: bool,
}

impl InputGate {
    /// Whether input is being limited (the caller skips the clock read
    /// when it is not — the default, unlimited path stays as it was).
    pub(crate) fn is_on(&self) -> bool {
        self.on
    }

    /// A join succeeded into a room limiting input at `rate` (`None` =
    /// the room does not limit).
    pub(crate) fn enter(&mut self, rate: Option<InputRate>, now: Instant) {
        self.on = rate.is_some();
        let Some(rate) = rate else {
            return;
        };
        match &mut self.bucket {
            Some(bucket) => bucket.retune(rate, now),
            None => self.bucket = Some(InputBucket::full(rate, now)),
        }
    }

    /// Admit one game-band action at `now`: always when off.
    pub(crate) fn admit(&mut self, now: Instant) -> bool {
        match &mut self.bucket {
            Some(bucket) if self.on => bucket.admit(now),
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests;
