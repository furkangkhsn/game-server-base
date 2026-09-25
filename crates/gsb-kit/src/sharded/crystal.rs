//! Crystallization (`docs/CROSS-SHARD.md` §4 layer 4, "C2 sonucu"): a
//! fight that keeps going across a seam moves onto ONE shard, through
//! the existing migration, and stays there until it is over.
//!
//! **Signal.** The kit sees every contact that crosses the seam in both
//! directions — the effects this shard's hooks emit ([`Seam::emit`]) and
//! the ones it applies (`apply_remote_effect`) — and records them per
//! unordered pair of wire ids in a bounded fight table ([`FightBook`]).
//! A pair is RIPE when its streak (contacts no more than `window` ticks
//! apart) has spanned `after` ticks and both directions are live.
//!
//! **Who moves.** The pair's HIGHER wire id moves to the shard that
//! lends the lower one. Wire ids never change and both shards know
//! both, so the two sides agree without a message; only the shard that
//! owns the higher id acts. A held entity is never a mover.
//!
//! **Hold.** Moving the owner alone would not stick: the mover still
//! stands in its old region, and the partition would hand it straight
//! back. So the kit PINS it — the receiving shard holds it (and its
//! partner) regardless of the region, until the fight has been quiet
//! for `release` ticks, the partner is gone, or it strays out of the
//! band ([`Partition::holds`] with `margin`; entering needs half the
//! margin — the spatial hysteresis). Then the region owns it again.
//!
//! [`Seam::emit`]: crate::sharded::Seam::emit
//! [`Partition::holds`]: crate::space::Partition::holds

use std::collections::HashMap;

use bevy_ecs::prelude::Entity;

use crate::sharded::ShardPin;

mod book;
mod tick;

#[cfg(test)]
pub(in crate::sharded) use book::FIGHT_CAP;
use book::FightBook;

/// A sharded room's crystallization policy — opt-in
/// ([`ShardedRoom::with_crystallize`](crate::sharded::ShardedRoom::with_crystallize));
/// a room without one never moves an entity but by its region. Ticks are
/// the room's ticks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Crystallize {
    /// K: how long (ticks) a cross-seam fight lasts, both directions
    /// live, before one party moves.
    pub after: u64,
    /// The longest silence (ticks) a fight survives: a pair with no
    /// contact for longer is forgotten, and a direction older than this
    /// is not live.
    pub window: u64,
    /// How long (ticks) a held fight must be quiet before the region
    /// owns its entities again.
    pub release: u64,
    /// How far (the position's unit) a held entity may stand outside the
    /// holding shard's region; a mover must be within half of it. The
    /// partition clamps it (`GridPartition2`: to its border margin).
    pub margin: f32,
}

impl Default for Crystallize {
    /// K = 1 s, window = 1 s, release = 3 s at 30 Hz; the band as wide as
    /// the partition allows.
    fn default() -> Self {
        Self {
            after: 30,
            window: 30,
            release: 90,
            margin: f32::INFINITY,
        }
    }
}

/// Why a hold ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::sharded) enum Release {
    /// The fight was quiet for `release` ticks.
    Quiet,
    /// The entity left the band.
    Band,
    /// The partner is no longer on this shard (dead, gone, moved on).
    Partner,
}

/// One held (or leaving) entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::sharded) struct Pin {
    /// The shard that holds it: this one, or — for a mover whose
    /// migration is under way — the partner's.
    pub(in crate::sharded) anchor: usize,
    /// The wire id the fight is with.
    pub(in crate::sharded) partner: u64,
    /// The latest contact involving the entity.
    pub(in crate::sharded) last: u64,
}

/// A room's crystallization state: the policy, the fight table and the
/// pins. Bounded: the table by [`book::FIGHT_CAP`], the pins by this
/// shard's entities (one per wire, dropped when it leaves or dies).
#[derive(Debug)]
pub(in crate::sharded) struct Crystal {
    pub(in crate::sharded) policy: Crystallize,
    pub(in crate::sharded) book: FightBook,
    pub(in crate::sharded) pins: HashMap<u64, Pin>,
}

impl Crystal {
    pub(in crate::sharded) fn new(policy: Crystallize) -> Self {
        Self {
            policy,
            book: FightBook::default(),
            pins: HashMap::new(),
        }
    }

    /// `source` acted on `target` at `tick` (either may be a wire this
    /// shard owns — `own`). A held entity's clock restarts; a pair that
    /// crosses the seam goes in the fight table (a local pair cannot
    /// crystallize — there is nothing to move).
    pub(in crate::sharded) fn contact(
        &mut self,
        source: u64,
        target: u64,
        tick: u64,
        own: &HashMap<u64, Entity>,
    ) {
        if source == target || source == 0 || target == 0 {
            return;
        }
        for wire in [source, target] {
            if let Some(pin) = self.pins.get_mut(&wire) {
                pin.last = pin.last.max(tick);
            }
        }
        if !(own.contains_key(&source) && own.contains_key(&target)) {
            self.book.touch(source, target, tick, self.policy.window);
        }
    }

    /// Where `wire` belongs while a fight holds it (`None`: its region).
    pub(in crate::sharded) fn anchor(&self, wire: u64) -> Option<usize> {
        self.pins.get(&wire).map(|p| p.anchor)
    }

    /// The pin a mover carries to its anchor (`None` unless `wire` is
    /// leaving `index` because of a fight).
    pub(in crate::sharded) fn carry(&self, wire: u64, index: usize) -> Option<ShardPin> {
        self.pins
            .get(&wire)
            .filter(|p| p.anchor != index)
            .map(|p| ShardPin {
                partner: p.partner,
                last: p.last,
            })
    }

    /// A mover arrived with `pin` on `index`: hold it and its partner
    /// here — unless the partner is not here (it moved on in the same
    /// tick): then the region owns the arrival again.
    pub(in crate::sharded) fn arrive(
        &mut self,
        wire: u64,
        pin: ShardPin,
        index: usize,
        own: &HashMap<u64, Entity>,
    ) {
        if !own.contains_key(&pin.partner) {
            return;
        }
        let last = pin.last;
        self.pins.insert(
            wire,
            Pin {
                anchor: index,
                partner: pin.partner,
                last,
            },
        );
        self.pins.entry(pin.partner).or_insert(Pin {
            anchor: index,
            partner: wire,
            last,
        });
    }

    /// `wire` left this shard (its migration committed).
    pub(in crate::sharded) fn leave(&mut self, wire: u64) {
        self.pins.remove(&wire);
    }
}
