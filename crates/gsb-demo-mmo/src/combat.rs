//! The MMO's combat: an `Attack` on an entity of THIS shard lands here;
//! an attack on an entity a neighbour lends through the border strip is
//! validated here (range, against the lent record — the anti-cheat
//! locality rule of `docs/CROSS-SHARD.md` §2) and sent to its owner as a
//! remote effect, which the owner applies (`Combat::apply_remote`).
//! Which of the two a wire id is, and the one view both are checked on,
//! the kit's `Seam::find` answers (`target`: the seam's precedence — own
//! over a lent copy, an entity handed on last tick lent by its new
//! owner). One function (`Combat::strike`) changes hit points, on whichever shard
//! owns the victim: a mob at zero dies there; a player at zero is
//! DEFEATED (back at the nearest waystone, full health). Both put a
//! player victim in combat — the owner marks it, so a parked character
//! hit across a seam still does not log out mid-fight.
//!
//! Kill credit: every landed hit is published, with the attacker's wire
//! identity, on the optional combat feed ([`Hit`]) by the shard that
//! applied it — the authority credits the kill. A hit the feed cannot
//! take is counted (`feed`: [`HITS_DROPPED_FULL`],
//! [`HITS_DROPPED_CLOSED`] — the game's own metric counters).

use bevy_ecs::prelude::{Entity, World};
use gsb_core::shard::{EffectOutcome, RemoteEffect};
use gsb_kit::identity::WireId;
use gsb_kit::sharded::{Found, Holder, Seam};

use crate::codec::MmoWire;
use crate::components::{InCombat, Kind, MoveTarget, Pos3, Vitals};
use crate::effect::MmoEffect;
use crate::world::{ATTACK_DAMAGE, ATTACK_RANGE, COMBAT_TICKS, PLAYER_HP, nearest_waystone};

mod feed;
mod target;

pub(crate) use feed::Feed;
pub use feed::{HITS_DROPPED_CLOSED, HITS_DROPPED_FULL};
use target::{Target, local_target};

/// The owner refuses a strike older than this (ticks since the attacker
/// swung): a melee hit that took longer to arrive is not a hit. The
/// game's staleness policy, well inside the core's transport envelope.
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
    /// The victim's hit points after the hit (0: killed / defeated).
    pub hp: u16,
    /// The hit killed a mob or defeated a player.
    pub killed: bool,
    /// The applying shard's tick.
    pub tick: u64,
}

/// One shard's combat state: its index and the optional kill feed
/// (with its loss counts).
pub(crate) struct Combat {
    pub(crate) shard: usize,
    pub(crate) feed: Feed,
}

impl Combat {
    /// `attacker` (this shard's entity) attacks wire id `target`: found
    /// through the seam — local or lent, the kit's precedence — or, with
    /// no seam, in this world; checked once, on the one view
    /// ([`Target`]); then struck here or sent to its owner.
    pub(crate) fn attack(
        &mut self,
        world: &mut World,
        seam: Option<&mut Seam<'_, '_, MmoWire>>,
        attacker: Entity,
        target: u64,
        tick: u64,
    ) {
        let (Some(&from), Some(me)) = (world.get::<Pos3>(attacker), world.get::<WireId>(attacker))
        else {
            return;
        };
        let me = me.get();
        if target == me {
            return;
        }
        let found = match seam.as_deref() {
            Some(seam) => seam.find::<Target>(world, target),
            None => local_target(world, target),
        };
        let Some(Found { holder, view, .. }) = found else {
            return;
        };
        if !view.up || from.dist(&view.at) > ATTACK_RANGE {
            return;
        }
        match (holder, seam) {
            (Holder::Local(victim), seam) => {
                if self.strike(world, victim, me, ATTACK_DAMAGE, tick) {
                    enter_combat(world, attacker, tick);
                    // A local blow is invisible to the kit: report it, so
                    // a fight that crystallized onto this shard is held
                    // here while it lasts (`world::CRYSTALLIZE`).
                    if let Some(seam) = seam {
                        seam.contact(me, target);
                    }
                }
            }
            // Not ours: a neighbour's entity we see through the strip.
            (Holder::Lent { .. }, Some(seam)) => {
                let strike = MmoEffect::Strike {
                    damage: ATTACK_DAMAGE,
                };
                match seam.emit(target, me, strike.encode()) {
                    // Swinging is being in combat, wherever it lands.
                    Ok(_) => enter_combat(world, attacker, tick),
                    Err(why) => tracing::debug!(target, ?why, "cross-seam attack not sent"),
                }
            }
            (Holder::Lent { .. }, None) => {}
        }
    }

    /// The owner's half of a cross-seam hit: a neighbour's `effect` on
    /// `target`, this shard's entity. Game policy on top of the core's
    /// checks: a stale strike or one from out of range (re-checked
    /// against the attacker's position as THIS shard sees it, when it
    /// does) is refused; the damage is capped.
    pub(crate) fn apply_remote(
        &mut self,
        world: &mut World,
        target: Entity,
        effect: &RemoteEffect,
        tick: u64,
        seam: &Seam<'_, '_, MmoWire>,
    ) -> EffectOutcome {
        let Some(MmoEffect::Strike { damage }) = MmoEffect::decode(&effect.payload) else {
            return EffectOutcome::Rejected;
        };
        if tick.saturating_sub(effect.at_tick) > STRIKE_MAX_AGE {
            return EffectOutcome::Rejected;
        }
        let Some(&at) = world.get::<Pos3>(target) else {
            return EffectOutcome::NoTarget;
        };
        let source = seam.find::<Target>(world, effect.source);
        if source.is_some_and(|f| f.view.at.dist(&at) > ATTACK_RANGE + RANGE_SLACK) {
            return EffectOutcome::Rejected;
        }
        let damage = damage.min(ATTACK_DAMAGE);
        if self.strike(world, target, effect.source, damage, tick) {
            EffectOutcome::Applied
        } else {
            EffectOutcome::Rejected
        }
    }

    /// Land `damage` on `victim` (this shard's entity) from `attacker`
    /// (a wire id); `false` when there was nothing to hit.
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
        let Some(mut vitals) = world.get_mut::<Vitals>(victim) else {
            return false;
        };
        if vitals.hp == 0 {
            return false;
        }
        vitals.hp = vitals.hp.saturating_sub(damage);
        let (kind, hp) = (vitals.kind, vitals.hp);
        let killed = hp == 0;
        if kind == Kind::Player {
            enter_combat(world, victim, tick);
            if killed {
                defeat(world, victim);
            }
        } else if killed {
            world.despawn(victim);
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

/// `entity` is in combat until [`COMBAT_TICKS`] after `tick`.
fn enter_combat(world: &mut World, entity: Entity, tick: u64) {
    world.entity_mut(entity).insert(InCombat {
        until: tick.saturating_add(COMBAT_TICKS),
    });
}

/// A defeated player: back on its feet at the nearest waystone.
fn defeat(world: &mut World, player: Entity) {
    let Some(&pos) = world.get::<Pos3>(player) else {
        return;
    };
    let [x, z] = nearest_waystone(&pos);
    let mut e = world.entity_mut(player);
    e.insert(Pos3::new(x, 0.0, z));
    e.remove::<MoveTarget>();
    if let Some(mut v) = e.get_mut::<Vitals>() {
        v.hp = PLAYER_HP;
    }
}

#[cfg(test)]
mod tests;
