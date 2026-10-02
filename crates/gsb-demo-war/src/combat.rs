//! The war game's combat: a player attacks an ENEMY player within
//! [`ATTACK_RANGE`] on the ground. A target of THIS shard is struck here;
//! a target a neighbour lends through the border strip is validated here
//! (range and side, against the lent record — the anti-cheat locality
//! rule of `docs/CROSS-SHARD.md` §2) and sent to its owner as a remote
//! effect, which the owner re-checks and applies
//! (`Combat::apply_remote`) — the MMO's pattern. Which of the two a
//! wire id is, and the one view both are checked on, the kit's
//! `Seam::find` answers (`foe`: the kit's precedence — own over a lent
//! copy, a unit handed on last tick lent by its new owner). One function
//! (`Combat::strike`) changes hit points, on whichever shard owns the
//! victim: a player at zero FALLS and is back on its feet at its
//! faction's base, full health (a respawn that is a teleport: across the
//! map, the kit hands the unit to the base's shard).
//!
//! Kill credit: every landed hit is published, with the attacker's wire
//! identity, on the optional combat feed ([`Hit`]) by the shard that
//! applied it — the authority, once. The same shard counts the kill
//! ([`KILLS`], the game's own metric counter: `logic_war_kills=` on its
//! `gsb-metric` line, `gsb_room_logic_war_kills_total` in the
//! exposition), and every hit the feed could not take (`feed`:
//! [`HITS_DROPPED_FULL`], [`HITS_DROPPED_CLOSED`]). Towers and capture
//! points cannot be struck (the game keeps its fight between players).

use bevy_ecs::prelude::{Entity, World};
use gsb_core::metrics::LogicCounter;
use gsb_core::shard::{EffectOutcome, RemoteEffect};
use gsb_kit::identity::WireId;
use gsb_kit::sharded::{Found, Holder, Seam};

use crate::codec::WarWire;
use crate::components::{Kind, MoveTarget, Pos3, Unit};
use crate::effect::WarEffect;
use crate::world::{ATTACK_DAMAGE, ATTACK_RANGE, PLAYER_HP, base};

mod feed;
mod foe;

pub(crate) use feed::Feed;
pub use feed::{HITS_DROPPED_CLOSED, HITS_DROPPED_FULL};
use foe::{Foe, local_foe};

/// The owner refuses a strike older than this (ticks since the attacker
/// swung): a melee hit that took longer to arrive is not a hit.
pub const STRIKE_MAX_AGE: u64 = 3;

/// The owner's range re-check allows this much (metres) beyond
/// [`ATTACK_RANGE`]: it compares two positions that are each up to two
/// ticks old (a runner covers under half a metre in that time).
pub const RANGE_SLACK: f32 = 2.0;

/// One landed hit, as the shard that applied it reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    /// The shard that applied the hit (the victim's owner).
    pub shard: usize,
    /// The attacker's wire id — who gets the credit.
    pub attacker: u64,
    /// The victim's wire id.
    pub target: u64,
    /// The victim's hit points after the hit (0: it fell).
    pub hp: u16,
    /// The hit felled the victim.
    pub killed: bool,
    /// The applying shard's tick.
    pub tick: u64,
}

/// Whether `unit` is a player still standing.
fn standing(unit: &Unit) -> bool {
    unit.kind == Kind::Player && unit.hp > 0
}

/// The war's one metric counter (F9): players felled, counted by the
/// shard that applied the killing blow (the victim's owner — once per
/// kill across the room).
pub const KILLS: LogicCounter = LogicCounter::sum(
    "war_kills",
    "Players felled, counted by the shard that applied the killing blow, cumulative.",
);

/// One shard's combat state: its index, the optional kill feed (with
/// its loss counts) and the kill count.
pub(crate) struct Combat {
    pub(crate) shard: usize,
    pub(crate) feed: Feed,
    /// Players this shard felled, cumulative ([`KILLS`]).
    pub(crate) kills: u64,
}

impl Combat {
    /// `attacker` (this shard's unit) attacks wire id `target`: found
    /// through the seam — local or lent, the kit's precedence — or, with
    /// no seam, in this world; checked once, on the one view
    /// ([`Foe`]); then struck here or sent to its owner.
    pub(crate) fn attack(
        &mut self,
        world: &mut World,
        seam: Option<&mut Seam<'_, '_, WarWire>>,
        attacker: Entity,
        target: u64,
        tick: u64,
    ) {
        let e = world.entity(attacker);
        let (Some(&from), Some(me), Some(&unit)) =
            (e.get::<Pos3>(), e.get::<WireId>(), e.get::<Unit>())
        else {
            return;
        };
        let me = me.get();
        if target == me || !standing(&unit) {
            return;
        }
        let found = match seam.as_deref() {
            Some(seam) => seam.find::<Foe>(world, target),
            None => local_foe(world, target),
        };
        let Some(Found { holder, view, .. }) = found else {
            return;
        };
        let enemy = view.standing && view.side != unit.faction;
        if !enemy || from.ground_dist(&view.at) > ATTACK_RANGE {
            return;
        }
        match (holder, seam) {
            (Holder::Local(victim), _) => {
                self.strike(world, victim, me, ATTACK_DAMAGE, tick);
            }
            // Not ours: an enemy a neighbour lends through the strip.
            (Holder::Lent { .. }, Some(seam)) => {
                let strike = WarEffect::Strike {
                    damage: ATTACK_DAMAGE,
                };
                if let Err(why) = seam.emit(target, me, strike.encode()) {
                    tracing::debug!(target, ?why, "cross-seam attack not sent");
                }
            }
            (Holder::Lent { .. }, None) => {}
        }
    }

    /// The owner's half of a cross-seam hit: a neighbour's `effect` on
    /// `target`, this shard's unit. Game policy on top of the core's
    /// checks: a stale strike, one from an ally, or one from out of range
    /// (re-checked against the attacker as THIS shard sees it, when it
    /// does) is refused; the damage is capped.
    pub(crate) fn apply_remote(
        &mut self,
        world: &mut World,
        target: Entity,
        effect: &RemoteEffect,
        tick: u64,
        seam: &Seam<'_, '_, WarWire>,
    ) -> EffectOutcome {
        let Some(WarEffect::Strike { damage }) = WarEffect::decode(&effect.payload) else {
            return EffectOutcome::Rejected;
        };
        if tick.saturating_sub(effect.at_tick) > STRIKE_MAX_AGE {
            return EffectOutcome::Rejected;
        }
        let (Some(&at), Some(&victim)) = (world.get::<Pos3>(target), world.get::<Unit>(target))
        else {
            return EffectOutcome::NoTarget;
        };
        let source = seam.find::<Foe>(world, effect.source).map(|f| f.view);
        if let Some(foe) = source
            && (foe.side == victim.faction || foe.at.ground_dist(&at) > ATTACK_RANGE + RANGE_SLACK)
        {
            return EffectOutcome::Rejected;
        }
        if self.strike(
            world,
            target,
            effect.source,
            damage.min(ATTACK_DAMAGE),
            tick,
        ) {
            EffectOutcome::Applied
        } else {
            EffectOutcome::Rejected
        }
    }

    /// Land `damage` on `victim` (this shard's unit) from `attacker` (a
    /// wire id); `false` when there was no standing player to hit.
    fn strike(
        &mut self,
        world: &mut World,
        victim: Entity,
        attacker: u64,
        damage: u16,
        tick: u64,
    ) -> bool {
        let Some(target) = world.get::<WireId>(victim).map(|w| w.get()) else {
            return false;
        };
        let Some(mut unit) = world.get_mut::<Unit>(victim) else {
            return false;
        };
        if !standing(&unit) {
            return false;
        }
        unit.hp = unit.hp.saturating_sub(damage);
        let (hp, faction) = (unit.hp, unit.faction);
        let killed = hp == 0;
        if killed {
            self.kills += 1;
            respawn(world, victim, faction);
        }
        self.feed.publish(Hit {
            shard: self.shard,
            attacker,
            target,
            hp,
            killed,
            tick,
        });
        true
    }
}

/// A fallen player: back on its feet at its faction's base.
fn respawn(world: &mut World, player: Entity, faction: Option<gsb_kit::team::Team>) {
    let Some(faction) = faction else { return };
    let mut e = world.entity_mut(player);
    e.insert(base(faction));
    e.remove::<MoveTarget>();
    if let Some(mut u) = e.get_mut::<Unit>() {
        u.hp = PLAYER_HP;
    }
}

#[cfg(test)]
mod tests;
