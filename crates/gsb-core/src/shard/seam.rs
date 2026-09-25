//! [`CrossSeam`]: what a shard's tick hooks see of — and may do across —
//! the seam. The read half is the borrowed border strip, read IN PLACE
//! (the actor's per-neighbour views; no per-tick copy); the write half
//! is the remote-effect outbox (`docs/CROSS-SHARD.md` §2).

use std::collections::HashMap;

use bytes::Bytes;

use crate::shard::*;

/// A record a neighbour lends this shard: its wire identity, the
/// neighbour that lent it (the entity's authority as of the lender's
/// last exchange — where an effect on it is routed), and the strip
/// payload. Read-only: the entity lives in the lender's world, and the
/// only way to affect it is [`CrossSeam::emit`].
#[derive(Debug, Clone, Copy)]
pub struct Lent<'a, S> {
    pub wire: u64,
    pub lender: usize,
    pub state: &'a S,
}

/// The cross-seam view a sharded tick hook receives
/// ([`ShardLogic::ingest_seam`], [`ShardLogic::update_seam`],
/// [`ShardLogic::apply_remote_effect`]).
///
/// **Read.** The borrowed strip exactly as the broadcast phase folds it
/// into this tick's snapshots: every neighbour's latest exchange, minus
/// a quarantined view (a rejected delta — its content may be wrong by an
/// unknown amount). Staleness ≤ 1 tick: a record is the lender's state
/// at the end of its previous tick (or of the current one, when the
/// lender's tick body ran first). NOT filtered against this shard's own
/// entities — an entity that just migrated in can be both own and lent
/// for one tick (the own record wins; a caller that knows its own set
/// skips such a wire, as the kit's seam does). Iteration order is
/// unspecified: sort if a decision depends on it.
///
/// **Write.** [`Self::emit`] queues one effect for the target's
/// authority (the lender), stamped and sequenced by the core; the actor
/// sends it after the systems phase.
///
/// **Handed on.** [`Self::departed`] names the neighbour an entity of
/// THIS shard went to, while the old owner remembers the move (the
/// forwarding table: written when the migration commits, kept
/// [`EFFECT_FORWARD_TTL_TICKS`]). In the tick after the move the entity
/// is still in this world — the doomed copy the migrate phase despawns
/// — and nobody lends it yet: the only party that knows the move
/// committed is this actor (a refused send leaves the entity here).
/// [`Self::emit`] routes a target nobody lends to that neighbour.
pub struct CrossSeam<'a, S> {
    views: &'a HashMap<usize, NeighborView<S>>,
    /// The lenders' indices, ascending: the deterministic lookup order
    /// (an entity handed between two neighbours can be lent by both for
    /// a tick; the lower index answers, and forwards if it is stale).
    lenders: &'a [usize],
    /// The forwarding table: `wire → (new owner, lapse tick)`.
    forwarded: &'a HashMap<u64, (usize, u64)>,
    out: &'a mut EffectOutbox,
}

impl<'a, S> CrossSeam<'a, S> {
    pub(crate) fn new(
        views: &'a HashMap<usize, NeighborView<S>>,
        lenders: &'a [usize],
        forwarded: &'a HashMap<u64, (usize, u64)>,
        out: &'a mut EffectOutbox,
    ) -> Self {
        Self {
            views,
            lenders,
            forwarded,
            out,
        }
    }

    fn live_views(&self) -> impl Iterator<Item = (usize, &NeighborView<S>)> + '_ {
        self.lenders.iter().filter_map(|&l| {
            self.views
                .get(&l)
                .filter(|v| !v.stale_until_full)
                .map(|v| (l, v))
        })
    }

    /// The record lent under `wire`, if a neighbour lends it.
    pub fn lent(&self, wire: u64) -> Option<Lent<'_, S>> {
        self.live_views().find_map(|(lender, v)| {
            v.recs.get(&wire).map(|r| Lent {
                wire,
                lender,
                state: &r.state,
            })
        })
    }

    /// Every lent record (unspecified order; see the type docs).
    pub fn iter(&self) -> impl Iterator<Item = Lent<'_, S>> + '_ {
        self.live_views().flat_map(|(lender, v)| {
            v.recs.values().map(move |r| Lent {
                wire: r.wire,
                lender,
                state: &r.state,
            })
        })
    }

    /// The number of lent records (a quarantined view counts nothing).
    pub fn len(&self) -> usize {
        self.live_views().map(|(_, v)| v.recs.len()).sum()
    }

    /// No record is lent.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The tick the hooks run in (the `at_tick` stamp of an emission).
    pub fn tick(&self) -> u64 {
        self.out.tick
    }

    /// The neighbour this shard handed its entity `wire` on to, if a
    /// migration of it committed less than [`EFFECT_FORWARD_TTL_TICKS`]
    /// ago and it has not come back (see the type docs, "Handed on").
    pub fn departed(&self, wire: u64) -> Option<usize> {
        self.forwarded.get(&wire).map(|&(to, _)| to)
    }

    /// Queue an effect on the lent entity `target` for its authority:
    /// `source` is the acting entity's wire id (attribution; `0` = none),
    /// `payload` the game's bytes. The core mints the idempotency key
    /// and stamps the tick; the answer is that key. The authority is the
    /// lender — or, for a target nobody lends that this shard just
    /// handed on, its new owner ([`Self::departed`]). Refused — nothing
    /// queued — when neither knows `target` ([`EmitRefused::NotLent`])
    /// or this tick's budget is spent ([`EmitRefused::Budget`]).
    ///
    /// Validation is the CALLER's (CROSS-SHARD §2, anti-cheat locality):
    /// check range/angle against [`Self::lent`]'s record before emitting;
    /// the authority may re-validate as game policy.
    pub fn emit(
        &mut self,
        target: u64,
        source: u64,
        payload: Bytes,
    ) -> Result<EffectId, EmitRefused> {
        let lender = self.lent(target).map(|l| l.lender);
        let Some(lender) = lender.or_else(|| self.departed(target)) else {
            self.out.refused += 1;
            return Err(EmitRefused::NotLent);
        };
        if self.out.budget_left == 0 {
            self.out.refused += 1;
            return Err(EmitRefused::Budget);
        }
        self.out.budget_left -= 1;
        self.out.emitted += 1;
        let id = self.out.mint();
        let at_tick = self.out.tick;
        self.out.queue.push((
            lender,
            RemoteEffect {
                target,
                source,
                id,
                at_tick,
                hops: 0,
                payload,
            },
        ));
        Ok(id)
    }
}

/// An owned stand-in for a shard actor's seam state — lent records and
/// an outbox — so a logic's seam hooks can be driven without an actor
/// (unit tests, tools). `seam()` lends a [`CrossSeam`] over it.
pub struct SeamStage<S> {
    views: HashMap<usize, NeighborView<S>>,
    lenders: Vec<usize>,
    forwarded: HashMap<u64, (usize, u64)>,
    out: EffectOutbox,
}

impl<S> SeamStage<S> {
    /// An empty stage for shard `origin`, at tick `tick`.
    pub fn new(origin: usize, tick: u64) -> Self {
        let mut out = EffectOutbox::new(origin);
        out.begin_tick(tick);
        Self {
            views: HashMap::new(),
            lenders: Vec::new(),
            forwarded: HashMap::new(),
            out,
        }
    }

    /// Record `record` as lent by neighbour `lender`.
    pub fn lend(&mut self, lender: usize, record: BorderRecord<S>) {
        if let Err(at) = self.lenders.binary_search(&lender) {
            self.lenders.insert(at, lender);
        }
        let view = self.views.entry(lender).or_default();
        view.recs.insert(record.wire, record);
    }

    /// Record this shard's entity `wire` as handed on to neighbour `to`
    /// (a committed migration — [`CrossSeam::departed`]).
    pub fn depart(&mut self, wire: u64, to: usize) {
        let lapse = self.out.tick + EFFECT_FORWARD_TTL_TICKS;
        self.forwarded.insert(wire, (to, lapse));
    }

    /// The seam the hooks receive.
    pub fn seam(&mut self) -> CrossSeam<'_, S> {
        CrossSeam::new(&self.views, &self.lenders, &self.forwarded, &mut self.out)
    }

    /// What was emitted so far: `(authority, effect)` in emission order.
    pub fn emitted(&self) -> &[(usize, RemoteEffect)] {
        &self.out.queue
    }
}
