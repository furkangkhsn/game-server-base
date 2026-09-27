//! [`Seam`]: the game's view across a shard seam — the core's
//! [`CrossSeam`] (the borrowed strip, read in place, and the remote-effect
//! outbox) joined with the kit's knowledge of which wire ids are THIS
//! shard's own entities.

use std::collections::HashMap;

use bevy_ecs::prelude::Entity;
use bytes::Bytes;
use gsb_core::shard::{CrossSeam, EffectId, EmitRefused, Lent};

use crate::sharded::crystal::Crystal;
use crate::sharded::departing::Departures;

mod area;

pub use area::{Found, Holder, SeamView};

/// What a sharded game's seam hooks receive
/// ([`ShardGame::ingest_seam`](crate::game::ShardGame::ingest_seam),
/// [`ShardGame::systems_seam`](crate::game::ShardGame::systems_seam),
/// [`ShardGame::apply_remote_effect`](crate::game::ShardGame::apply_remote_effect)),
/// over the game's wire value `V`.
///
/// **Local ∪ lent.** The world the hook already holds is the local half
/// (query it as always); [`Self::lent`] / [`Self::lent_iter`] are the
/// other half — the neighbours' boundary records, ≤ 1 tick stale, the
/// same records this tick's snapshots carry. The two never overlap: a
/// wire id that is this shard's own (an entity that just migrated in is
/// still lent by its old shard for a tick) is answered by
/// [`Self::local`] only — own wins, as in the snapshot. "Everything
/// within r of p" is a world query plus `lent_iter().filter(..)` — or,
/// merged by the kit with the same rules, [`Self::within`] /
/// [`Self::area`] (and [`Self::find`] for one wire id), read into one
/// game type ([`SeamView`]).
///
/// **The migration tick.** An entity this shard handed on in the last
/// tick is still in the world (the core despawns it at the end of this
/// tick) but no longer its own: during the game's hooks it is disabled
/// — no world query finds it — and it is lent here by its NEW owner,
/// with the record it left with; [`Self::emit`] reaches it there
/// (`docs/CROSS-SHARD.md` "D sonucu"). A hit resolved "local, else
/// lent" lands once, where the entity lives.
///
/// **Acting across.** A lent entity cannot be written: [`Self::emit`]
/// sends an effect to its authority, which applies it in its own world
/// (CROSS-SHARD §2). Validate (range, angle) against the lent record
/// first — that is the anti-cheat locality rule.
pub struct Seam<'s, 'a, V> {
    cross: &'s mut CrossSeam<'a, V>,
    own: &'s HashMap<u64, Entity>,
    /// The entities handed on last tick whose copies are still here.
    departing: &'s Departures<V>,
    /// The room's crystallization state, when it has opted in: the seam
    /// records the contacts it sees.
    crystal: Option<&'s mut Crystal>,
}

impl<'s, 'a, V> Seam<'s, 'a, V> {
    pub(in crate::sharded) fn new(
        cross: &'s mut CrossSeam<'a, V>,
        own: &'s HashMap<u64, Entity>,
        departing: &'s Departures<V>,
        crystal: Option<&'s mut Crystal>,
    ) -> Self {
        Self {
            cross,
            own,
            departing,
            crystal,
        }
    }

    /// This shard's entity with wire id `wire`, if it owns one — not an
    /// entity it handed on last tick (type docs). (During a hook the
    /// table can trail the hook's own spawns and despawns: a despawned
    /// entity may still be named — check the world.)
    pub fn local(&self, wire: u64) -> Option<Entity> {
        self.own
            .get(&wire)
            .copied()
            .filter(|_| self.departing.departing(wire, self.cross).is_none())
    }

    /// The record a neighbour lends under `wire` — `None` when nobody
    /// lends it, or when it is this shard's own entity. An entity this
    /// shard handed on last tick is lent by its new owner, with the
    /// record it left with (whether or not that owner's first export has
    /// arrived yet — the same answer on every run).
    pub fn lent(&self, wire: u64) -> Option<Lent<'_, V>> {
        if let Some((lender, state)) = self.departing.departing(wire, self.cross) {
            return Some(Lent {
                wire,
                lender,
                state,
            });
        }
        if self.own.contains_key(&wire) {
            return None;
        }
        self.cross.lent(wire)
    }

    /// Every lent record that is not this shard's own entity, the
    /// entities it handed on last tick included — each once, as
    /// [`Self::lent`] answers (unspecified order — sort if a decision
    /// depends on it). An entity handed between two neighbours can be
    /// lent by both for a tick (the old owner still exports the copy it
    /// is about to despawn): only the lower lender's record is yielded.
    pub fn lent_iter(&self) -> impl Iterator<Item = Lent<'_, V>> + '_ {
        let cross = &*self.cross;
        // A handed-on wire is still in the owned table: its lent copies
        // (the new owner's, a stale one) drop here, and its one record
        // comes from the departures. A wire two neighbours lend is
        // yielded from the lender `lent` answers with.
        let lent = cross.iter().filter(|l| {
            !self.own.contains_key(&l.wire)
                && cross.lent(l.wire).is_some_and(|a| a.lender == l.lender)
        });
        let handed_on = self
            .departing
            .iter(cross)
            .map(|(wire, lender, state)| Lent {
                wire,
                lender,
                state,
            });
        lent.chain(handed_on)
    }

    /// `wire` is this shard's own entity (handed-on copies are not).
    fn is_own(&self, wire: u64) -> bool {
        self.own.contains_key(&wire) && self.departing.departing(wire, self.cross).is_none()
    }

    /// The tick the hook runs in.
    pub fn tick(&self) -> u64 {
        self.cross.tick()
    }

    /// Send an effect on the lent entity `target` to its authority (see
    /// [`CrossSeam::emit`]): `source` is the acting entity's wire id,
    /// `payload` the game's bytes. [`EmitRefused::Local`] when `target`
    /// is this shard's own entity — write it directly. An entity handed
    /// on last tick is not: the effect goes to its new owner.
    ///
    /// A sent effect — or a `Local` refusal, which the caller answers by
    /// writing the target directly — counts as a contact
    /// ([`Self::contact`]).
    pub fn emit(
        &mut self,
        target: u64,
        source: u64,
        payload: Bytes,
    ) -> Result<EffectId, EmitRefused> {
        let sent = if self.is_own(target) {
            Err(EmitRefused::Local)
        } else {
            self.cross.emit(target, source, payload)
        };
        if matches!(sent, Ok(_) | Err(EmitRefused::Local)) {
            self.contact(source, target);
        }
        sent
    }

    /// Report that `source` acted on `target` this tick, where the kit
    /// cannot see it — a hit the game lands on its OWN entity directly.
    /// Crystallization's clock (`docs/CROSS-SHARD.md` §4 layer 4): once
    /// a fight has moved onto one shard its blows are local, and a held
    /// fight is released when it has been quiet — so a game that opts
    /// in reports its local hits here. A no-op for a room that has not
    /// opted in; [`Self::emit`] and applied remote effects are counted
    /// by the kit itself.
    pub fn contact(&mut self, source: u64, target: u64) {
        let tick = self.cross.tick();
        let (own, departing, cross) = (self.own, self.departing, &*self.cross);
        if let Some(crystal) = self.crystal.as_deref_mut() {
            let own = |wire| own.contains_key(&wire) && departing.departing(wire, cross).is_none();
            crystal.contact(source, target, tick, own);
        }
    }
}
