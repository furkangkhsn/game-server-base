//! The shard actor's remote-effect state: the outbox the tick hooks
//! write into, the received effects waiting for their apply tick, the
//! retry buffer, the forwarding table, the duplicate windows and the
//! counters. Every table here is bounded (the bound is stated per
//! field).

use std::collections::{HashMap, VecDeque};

use crate::shard::border::NeighborView;
use crate::shard::effect::window::{Seen, Window};
use crate::shard::effect::{EFFECT_BUDGET_PER_TICK, EffectId, RemoteEffect};
use crate::shard::seam::CrossSeam;

/// Effect counters of one shard, cumulative since birth (read by the
/// tests, logged by the ~1 s summary line when they moved).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EffectStats {
    /// Effects minted here (accepted by `emit`).
    pub(crate) emitted: u64,
    /// `emit` refusals (budget spent or target not lent).
    pub(crate) refused: u64,
    /// Effects queued into a neighbour's link (first tries, retries and
    /// forwards alike).
    pub(crate) sent: u64,
    /// Sends that met a full link and went to the retry buffer.
    pub(crate) retried: u64,
    /// Sends dropped because the retry buffer was full.
    pub(crate) dropped_full: u64,
    /// Sends dropped because the link is closed (the peer is gone).
    pub(crate) dropped_closed: u64,
    /// Effects dropped for age (sender, forwarder or authority).
    pub(crate) expired: u64,
    /// Effects received over the links.
    pub(crate) received: u64,
    /// Effects the game applied.
    pub(crate) applied: u64,
    /// Effects the game refused (its policy).
    pub(crate) rejected: u64,
    /// Effects whose target is not here and was not handed on (dead).
    pub(crate) orphaned: u64,
    /// Effects refused as already applied (or older than the window).
    pub(crate) duplicates: u64,
    /// Effects handed on to the target's new owner.
    pub(crate) forwarded: u64,
    /// Effects dropped at the hop bound.
    pub(crate) dropped_hops: u64,
    /// Effects of another room incarnation, or naming an origin outside
    /// the room.
    pub(crate) foreign: u64,
}

/// What the tick hooks emit into (through [`crate::shard::CrossSeam`]):
/// the minting state and this tick's queue. Flushed in phase 3b.
#[derive(Debug)]
pub(crate) struct EffectOutbox {
    /// This shard's index (every minted id's `origin`).
    pub(crate) origin: usize,
    /// The room incarnation (every minted id's `epoch`).
    pub(crate) epoch: u64,
    /// The last sequence number minted.
    pub(crate) next_seq: u64,
    /// Emissions left this tick (reset at every tick's start).
    pub(crate) budget_left: usize,
    /// The current tick (the `at_tick` stamp).
    pub(crate) tick: u64,
    /// This tick's sends: `(neighbour link index, effect)` — emissions
    /// (≤ the budget) and forwards (≤ what was received).
    pub(crate) queue: Vec<(usize, RemoteEffect)>,
    /// `emit` acceptances and refusals since the last fold into the
    /// stats ([`EffectBook::fold_outbox`]).
    pub(crate) emitted: u64,
    pub(crate) refused: u64,
}

impl EffectOutbox {
    pub(crate) fn new(origin: usize) -> Self {
        Self {
            origin,
            epoch: 0,
            next_seq: 0,
            budget_left: EFFECT_BUDGET_PER_TICK,
            tick: 0,
            queue: Vec::new(),
            emitted: 0,
            refused: 0,
        }
    }

    /// A new tick: stamp and a fresh budget.
    pub(crate) fn begin_tick(&mut self, tick: u64) {
        self.tick = tick;
        self.budget_left = EFFECT_BUDGET_PER_TICK;
    }

    /// Mint the next id (the caller has already charged the budget).
    pub(crate) fn mint(&mut self) -> EffectId {
        self.next_seq += 1;
        EffectId {
            origin: self.origin,
            epoch: self.epoch,
            seq: self.next_seq,
        }
    }
}

/// All of one shard actor's remote-effect state.
#[derive(Debug)]
pub(crate) struct EffectBook {
    pub(crate) out: EffectOutbox,
    /// Received effects not yet due (arrived before their apply tick —
    /// at most one tick's arrivals: anything further in the future is
    /// dropped as foreign).
    pub(crate) pending: Vec<RemoteEffect>,
    /// Failed sends awaiting the next flush, oldest first; at most
    /// [`crate::shard::EFFECT_RETRY_CAP`].
    pub(crate) retry: VecDeque<(usize, RemoteEffect)>,
    /// `wire → (the neighbour it migrated to, the tick the entry lapses)`
    /// — written when a migration commits, swept at the TTL, so it holds
    /// at most the migrations of the last
    /// [`crate::shard::EFFECT_FORWARD_TTL_TICKS`] ticks.
    pub(crate) forwarded: HashMap<u64, (usize, u64)>,
    /// One duplicate window per origin shard, indexed by shard index:
    /// `shard_count` fixed-size windows, whatever the traffic.
    pub(crate) windows: Vec<Window>,
    pub(crate) stats: EffectStats,
    /// The stats as last logged (the summary logs only on change).
    pub(crate) logged: EffectStats,
}

impl EffectBook {
    pub(crate) fn new(origin: usize, shard_count: usize) -> Self {
        Self {
            out: EffectOutbox::new(origin),
            pending: Vec::new(),
            retry: VecDeque::new(),
            forwarded: HashMap::new(),
            windows: vec![Window::default(); shard_count],
            stats: EffectStats::default(),
            logged: EffectStats::default(),
        }
    }

    /// Move the outbox's emit counts into the stats.
    pub(crate) fn fold_outbox(&mut self) {
        self.stats.emitted += std::mem::take(&mut self.out.emitted);
        self.stats.refused += std::mem::take(&mut self.out.refused);
    }

    /// The seam the tick hooks receive: the borrowed strip (`views`,
    /// read in `lenders` order) with this book's forwarding table and
    /// outbox.
    pub(crate) fn seam<'a, S>(
        &'a mut self,
        views: &'a HashMap<usize, NeighborView<S>>,
        lenders: &'a [usize],
    ) -> CrossSeam<'a, S> {
        CrossSeam::new(views, lenders, &self.forwarded, &mut self.out)
    }

    /// The duplicate check for `id` (its origin already validated).
    pub(crate) fn admit(&mut self, id: EffectId) -> Seen {
        self.windows[id.origin].admit(id.seq)
    }
}
