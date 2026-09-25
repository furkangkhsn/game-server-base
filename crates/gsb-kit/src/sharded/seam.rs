//! [`Seam`]: the game's view across a shard seam — the core's
//! [`CrossSeam`] (the borrowed strip, read in place, and the remote-effect
//! outbox) joined with the kit's knowledge of which wire ids are THIS
//! shard's own entities.

use std::collections::HashMap;

use bevy_ecs::prelude::Entity;
use bytes::Bytes;
use gsb_core::shard::{CrossSeam, EffectId, EmitRefused, Lent};

use crate::sharded::crystal::Crystal;

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
/// within r of p" is therefore a world query plus
/// `lent_iter().filter(..)` — no merged copy is built.
///
/// **Acting across.** A lent entity cannot be written: [`Self::emit`]
/// sends an effect to its authority, which applies it in its own world
/// (CROSS-SHARD §2). Validate (range, angle) against the lent record
/// first — that is the anti-cheat locality rule.
pub struct Seam<'s, 'a, V> {
    cross: &'s mut CrossSeam<'a, V>,
    own: &'s HashMap<u64, Entity>,
    /// The room's crystallization state, when it has opted in: the seam
    /// records the contacts it sees.
    crystal: Option<&'s mut Crystal>,
}

impl<'s, 'a, V> Seam<'s, 'a, V> {
    pub(in crate::sharded) fn new(
        cross: &'s mut CrossSeam<'a, V>,
        own: &'s HashMap<u64, Entity>,
        crystal: Option<&'s mut Crystal>,
    ) -> Self {
        Self {
            cross,
            own,
            crystal,
        }
    }

    /// This shard's entity with wire id `wire`, if it owns one. (During a
    /// hook the table can trail the hook's own spawns and despawns: a
    /// despawned entity may still be named — check the world.)
    pub fn local(&self, wire: u64) -> Option<Entity> {
        self.own.get(&wire).copied()
    }

    /// The record a neighbour lends under `wire` — `None` when nobody
    /// lends it, or when it is this shard's own entity.
    pub fn lent(&self, wire: u64) -> Option<Lent<'_, V>> {
        if self.own.contains_key(&wire) {
            return None;
        }
        self.cross.lent(wire)
    }

    /// Every lent record that is not this shard's own entity
    /// (unspecified order — sort if a decision depends on it).
    pub fn lent_iter(&self) -> impl Iterator<Item = Lent<'_, V>> + '_ {
        self.cross
            .iter()
            .filter(|l| !self.own.contains_key(&l.wire))
    }

    /// The tick the hook runs in.
    pub fn tick(&self) -> u64 {
        self.cross.tick()
    }

    /// Send an effect on the lent entity `target` to its authority (see
    /// [`CrossSeam::emit`]): `source` is the acting entity's wire id,
    /// `payload` the game's bytes. [`EmitRefused::Local`] when `target`
    /// is this shard's own entity — write it directly.
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
        let sent = if self.own.contains_key(&target) {
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
        if let Some(crystal) = self.crystal.as_deref_mut() {
            crystal.contact(source, target, tick, self.own);
        }
    }
}
