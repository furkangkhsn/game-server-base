//! Phase 1 — RESPONSES (`docs/SERVICES.md` §3): the shard's call table
//! absorbs what finished since the last tick (the room actor's phase,
//! through the same shared code).

use std::fmt::Debug;
use std::hash::Hash;

use crate::calls::Owner;
use crate::id::PlayerId;
use crate::shard::actor::ShardActor;

impl<W, G, St, Sp> ShardActor<W, G, St, Sp>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
    St: Debug + Send + 'static,
    // The strip rides every exchange and view; the bounds mirror what
    // the delta protocol does with it (diff via PartialEq, clone into
    // each neighbor's message, store in the actor's maps).
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Tick phase 1 — RESPONSES: the refusals the last FLUSH decided, the
    /// completion channel (non-blocking), the External deadline sweep on
    /// the tick clock. An owner is alive by the logic's word or, by
    /// default, while this shard holds the player (a migrated-away
    /// owner's result is `responses_owner_gone` here).
    pub(crate) fn phase_completions(&mut self) {
        let now = crate::ticker::now();
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
