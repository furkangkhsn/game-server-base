//! The input-idle clock: WHEN each member last sent an action-bearing
//! frame, and the read-only view the game logic sees through
//! [`TickCtx`](crate::room::TickCtx).
//!
//! **What "action-bearing" means.** It is a *structural* boundary, not a
//! list of opcodes — the base does not know the game's opcodes. A frame
//! is action-bearing exactly when the connection actor forwards it to the
//! room as an [`Action`](crate::room::Action) (`forward_to_room`): every
//! REGISTERED game-band opcode, plus the base-band RPC request envelope.
//! Everything the connection actor answers or rejects by itself — AUTH,
//! JOIN/LEAVE, HEARTBEAT, an unknown opcode, a frame that fails the
//! violation budget — never becomes an action and therefore never moves
//! this clock. A heartbeat keeps the *transport* alive (the reader pump's
//! idle window) and leaves the *input* clock exactly where it was.
//!
//! The clock is stamped in the room's READ phase, where those actions are
//! actually pulled: one stamp per member per tick in which the room
//! pulled at least one of its actions. A member holding actions that this
//! tick's pull budget did not reach is stamped on the tick that reaches
//! them (the READ rotation guarantees that happens within one rotation),
//! never later than the input is ingested.
//!
//! **Who is NOT on the clock.** A parked (detached) row and a bot-fed
//! (AI-handover) row hold no entry at all: neither has a live input
//! source, the READ phase pulls nothing for them, and a bot's synthesized
//! input is produced INSIDE the game logic's `ingest` — it never crosses
//! the action channel, so by the definition above it is not input. Their
//! clock restarts when a resume re-attaches a human session.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::id::PlayerId;

/// Per-tick slots the bounded ceiling sweep examines. The sweep is a
/// ROTATION over the clock's slot vector (the same discipline as the READ
/// phase's roster rotation): it walks at most this many entries per step
/// and resumes where it stopped, so its per-tick cost is constant instead
/// of scaling with the member count. The price is detection latency —
/// every member is examined within `ceil(members / SWEEP_BUDGET)` steps
/// (a 10 000-member room at 30 Hz: ~157 steps ≈ 5 s) — which is noise
/// against a ceiling measured in tens of seconds, and it is a CEILING:
/// acting late is safe, acting early would not be.
pub(crate) const SWEEP_BUDGET: usize = 64;

/// The room's (or shard's) input-idle clock. Non-generic on purpose: the
/// member table is generic over the game's group key, and the tick
/// context has to lend this out to the logic without dragging that
/// parameter through every hook signature.
///
/// Storage is the roster pattern — a slot vector in join order plus a
/// position index — so that a stamp is O(1), a removal is a swap-remove
/// plus one index fix, and the ceiling sweep has a stable order it can
/// rotate over. (A bare `HashMap` gives the first two but no resumable
/// iteration, which is what makes the sweep's cost bounded.)
#[derive(Debug, Default)]
pub(crate) struct IdleClock {
    /// `(player, last action-bearing frame)`, in join order.
    slots: Vec<(PlayerId, Instant)>,
    /// `player → index into slots`.
    pos: HashMap<PlayerId, usize>,
    /// Rotating sweep cursor, kept as an absolute count (not a raw index)
    /// so membership changes between sweeps degrade to a shifted start
    /// offset, never an out-of-range index — the READ cursor's rule.
    cursor: usize,
}

impl IdleClock {
    /// Start (or restart) a member's clock: a fresh join, a migrate-in, a
    /// resume. Idempotent — an existing entry is re-stamped.
    pub(crate) fn start(&mut self, player: PlayerId, now: Instant) {
        match self.pos.get(&player) {
            Some(&i) => self.slots[i].1 = now,
            None => {
                self.pos.insert(player, self.slots.len());
                self.slots.push((player, now));
            }
        }
    }

    /// Record an action-bearing pull. Does nothing for a player without a
    /// clock (parked/bot-fed/unknown): an entry is created only by
    /// [`Self::start`], so a row that is deliberately off the clock
    /// cannot be put back on it by a stray action.
    pub(crate) fn touch(&mut self, player: PlayerId, now: Instant) {
        if let Some(&i) = self.pos.get(&player) {
            self.slots[i].1 = now;
        }
    }

    /// Take a member off the clock: a despawn, a migrate-out, a detach
    /// (the park has no input source) or an AI handover.
    pub(crate) fn stop(&mut self, player: PlayerId) {
        let Some(i) = self.pos.remove(&player) else {
            return;
        };
        let relocated = self.slots.last().expect("pos entry implies a slot").0;
        self.slots.swap_remove(i);
        if relocated != player {
            // `insert`, not `get_mut`: rewriting the relocated entry keeps
            // this correct even under partial drift (the roster's rule).
            self.pos.insert(relocated, i);
        }
    }

    /// The raw stamp, for the shard's migration payload (the clock is
    /// per-actor, so a crossing has to carry it). `None` = off the clock.
    pub(crate) fn last(&self, player: PlayerId) -> Option<Instant> {
        let &i = self.pos.get(&player)?;
        Some(self.slots[i].1)
    }

    /// How long since this player's last action-bearing frame. `None` =
    /// the player has no clock (never joined here, parked, or bot-fed).
    pub(crate) fn since(&self, player: PlayerId, now: Instant) -> Option<Duration> {
        let &i = self.pos.get(&player)?;
        Some(now.saturating_duration_since(self.slots[i].1))
    }

    /// One bounded rotation of the ceiling sweep: append every player
    /// among the next [`SWEEP_BUDGET`] slots whose idle time has reached
    /// `limit`, and advance the cursor past every slot examined.
    ///
    /// `due` is the caller's reused buffer; it is NOT cleared here (the
    /// caller owns it). Cost: at most `SWEEP_BUDGET` `Instant`
    /// comparisons per call, whatever the member count.
    pub(crate) fn sweep_due(
        &mut self,
        now: Instant,
        limit: Duration,
        due: &mut Vec<PlayerId>,
    ) -> usize {
        let n = self.slots.len();
        if n == 0 {
            return 0;
        }
        let visits = SWEEP_BUDGET.min(n);
        let mut idx = self.cursor % n;
        for _ in 0..visits {
            let (player, last) = self.slots[idx];
            if now.saturating_duration_since(last) >= limit {
                due.push(player);
            }
            idx += 1;
            if idx == n {
                idx = 0;
            }
        }
        self.cursor = self.cursor.wrapping_add(visits);
        visits
    }

    /// Members currently on the clock (diagnostics/tests).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }
}

/// The read-only, per-tick view of the input-idle clock that the game
/// logic reaches through [`TickCtx`](crate::room::TickCtx).
///
/// It is `Copy` and carries the tick's own wall clock, so
/// [`Self::since_input`] is a lookup and a subtraction — no `Instant::now()`
/// inside a logic hook, no await, no new hook on the logic surface.
///
/// The default (and the value a hand-built `TickCtx` gets) is the EMPTY
/// view: every query answers `None`. A logic must therefore treat `None`
/// as "no input clock for this player" — which is also the answer for a
/// parked or bot-fed member — and never as "idle forever".
#[derive(Debug, Clone, Copy, Default)]
pub struct IdleView<'a> {
    inner: Option<(&'a IdleClock, Instant)>,
}

impl<'a> IdleView<'a> {
    pub(crate) fn new(clock: &'a IdleClock, now: Instant) -> Self {
        Self {
            inner: Some((clock, now)),
        }
    }

    /// Time since `player`'s last action-bearing frame, measured against
    /// this tick's clock. `None` when the player has no input clock: not
    /// a member here, parked (detached), or bot-fed.
    pub fn since_input(&self, player: PlayerId) -> Option<Duration> {
        let (clock, now) = self.inner?;
        clock.since(player, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(n: u64) -> PlayerId {
        PlayerId(n)
    }

    #[test]
    fn start_touch_and_stop_keep_the_index_consistent() {
        let t0 = Instant::now();
        let mut c = IdleClock::default();
        for i in 1..=4 {
            c.start(p(i), t0);
        }
        assert_eq!(c.len(), 4);
        // Remove from the middle: the relocated tail must still resolve.
        c.stop(p(2));
        assert_eq!(c.len(), 3);
        assert!(c.since(p(2), t0).is_none());
        let later = t0 + Duration::from_secs(5);
        c.touch(p(4), later);
        assert_eq!(c.since(p(4), later), Some(Duration::ZERO));
        assert_eq!(c.since(p(1), later), Some(Duration::from_secs(5)));
        // A player off the clock cannot be put back on it by a touch.
        c.touch(p(2), later);
        assert!(c.since(p(2), later).is_none());
    }

    #[test]
    fn the_sweep_rotates_and_reaches_every_slot() {
        let t0 = Instant::now();
        let mut c = IdleClock::default();
        let n = SWEEP_BUDGET * 2 + 5;
        for i in 0..n {
            c.start(p(i as u64), t0);
        }
        let now = t0 + Duration::from_secs(10);
        let limit = Duration::from_secs(1);
        let mut seen: Vec<PlayerId> = Vec::new();
        // One full rotation's worth of sweeps must reach every slot.
        let rounds = n.div_ceil(SWEEP_BUDGET);
        for _ in 0..rounds {
            c.sweep_due(now, limit, &mut seen);
        }
        seen.sort_by_key(|x| x.0);
        seen.dedup();
        assert_eq!(
            seen.len(),
            n,
            "every member is examined within one rotation"
        );
        // One sweep alone never examines more than the budget.
        let mut one: Vec<PlayerId> = Vec::new();
        c.sweep_due(now, limit, &mut one);
        assert_eq!(one.len(), SWEEP_BUDGET);
    }

    #[test]
    fn nothing_is_due_below_the_limit() {
        let t0 = Instant::now();
        let mut c = IdleClock::default();
        c.start(p(1), t0);
        let mut due = Vec::new();
        c.sweep_due(
            t0 + Duration::from_secs(1),
            Duration::from_secs(5),
            &mut due,
        );
        assert!(due.is_empty());
        c.sweep_due(
            t0 + Duration::from_secs(5),
            Duration::from_secs(5),
            &mut due,
        );
        assert_eq!(due, vec![p(1)]);
    }
}
