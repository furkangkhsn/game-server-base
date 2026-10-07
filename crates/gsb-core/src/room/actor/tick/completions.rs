//! Phase 1 — RESPONSES (`docs/SERVICES.md` §3): the call table absorbs
//! what finished since the last tick.

use std::fmt::Debug;
use std::hash::Hash;

use crate::calls::Owner;
use crate::id::PlayerId;
use crate::room::actor::RoomActor;

impl<W, G, Sp> RoomActor<W, G, Sp>
where
    G: Eq + Hash + Clone + Debug,
    // The strip payload's trait bounds (`GameLogic::Strip`) — restated so
    // calls through the logic object type-check; the room itself never
    // touches the payloads.
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Tick phase 1 — RESPONSES (right after CONTROL, before READ: this
    /// tick's input is checked against the freshest world, and an answer
    /// given here frees its correlation id for this tick's READ). The
    /// refusals the last FLUSH decided, then the completion channel
    /// (non-blocking — the room never awaits a service or a worker; the
    /// same `try_recv` discipline as the control channel), then the
    /// External deadlines on the tick clock (`ticker::now()` — the
    /// deadline is set on it too, F16; TEST mode's pinned step stamp,
    /// [`crate::calls::Calls::now`]). Exactly one answer per request is
    /// structural: a report for a call no longer pending (already
    /// answered, timed out, or its session ended) is dropped and counted.
    /// Cost when quiet: one empty `try_recv` and two `is_empty` probes.
    pub(super) fn phase_completions(&mut self) {
        let now = self.calls.now();
        let logic = &self.logic;
        let world = &self.world;
        let conns = &self.conns;
        let alive = |o: Owner| {
            logic
                .owner_alive(world, o)
                .unwrap_or_else(|| conns.contains_key(&PlayerId(o.0)))
        };
        let mut p = crate::room::Parts {
            calls: &mut self.calls,
            m: &mut self.m,
            queued: &mut self.queued,
            conns,
            config: &self.config,
        };
        p.responses(now, &alive);
    }
}
