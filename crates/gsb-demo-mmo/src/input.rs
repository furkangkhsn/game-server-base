//! The MMO's input path (its `Game::ingest`): `MoveTo`, `Attack` and
//! `Travel` decoding under the kit's sequence rule
//! ([`InputSeq::admit`]). One sequence space per session across all
//! three messages (the client numbers every input it sends).

use std::collections::HashMap;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::PlayerId;
use gsb_core::room::Action;
use gsb_kit::game::InputSeq;
use gsb_kit::identity::WireId;
use prost::Message;

use crate::codec::from_dm;
use crate::components::{Mob, MoveTarget, Pos3, Vitals};
use crate::mmo::{Attack, MoveTo, Travel};
use crate::op;
use crate::world::{ATTACK_DAMAGE, ATTACK_RANGE, WAYSTONES};

/// One decoded input.
enum Input {
    Move { x: f32, z: f32 },
    Attack { target: u64 },
    Travel { waystone: usize },
}

/// Decode an action's payload into `(seq, input)`; `None` for an opcode
/// that is not the MMO's or an undecodable payload (a client bug: logged
/// and skipped, never a reason to drop the connection).
fn decode(action: &Action) -> Option<(u64, Input)> {
    let bytes = &action.payload[..];
    let decoded = match action.op {
        op::MMO_MOVE_TO => MoveTo::decode(bytes).map(|m| {
            let (x, z) = (from_dm(m.x), from_dm(m.z));
            (m.seq, Input::Move { x, z })
        }),
        op::MMO_ATTACK => {
            Attack::decode(bytes).map(|m| (m.seq, Input::Attack { target: m.target }))
        }
        op::MMO_TRAVEL => Travel::decode(bytes).map(|m| {
            let waystone = m.waystone as usize;
            (m.seq, Input::Travel { waystone })
        }),
        _ => return None,
    };
    decoded
        .inspect_err(|_| tracing::warn!(op = action.op, "undecodable MMO input ignored"))
        .ok()
}

/// Decode and apply the tick's actions for the players with a live
/// entity in this shard's world.
pub(crate) fn ingest(
    players: &HashMap<PlayerId, Entity>,
    world: &mut World,
    actions: &mut Vec<Action>,
    seq: &mut InputSeq,
) {
    for action in actions.drain(..) {
        let Some((n, input)) = decode(&action) else {
            continue;
        };
        // Actions are keyed by the STABLE player id the core stamped
        // (bot-synthesized frames carry it directly).
        let Some(&entity) = players.get(&action.player) else {
            continue; // not (or no longer) in this shard
        };
        if world.get_entity(entity).is_err() {
            continue;
        }
        // The kit's rule: a duplicate or reordered-late numbered input
        // is dropped silently; seq 0 always passes.
        if !seq.admit(action.player, n) {
            continue;
        }
        match input {
            Input::Move { x, z } => {
                let at = Pos3::new(x, 0.0, z).clamped();
                world
                    .entity_mut(entity)
                    .insert(MoveTarget { x: at.x, z: at.z });
            }
            Input::Attack { target } => attack(world, entity, target),
            Input::Travel { waystone } => travel(world, entity, waystone),
        }
    }
}

/// Teleport to a waystone: the position jumps (possibly into another
/// shard's region — the kit hands the entity to it, hop by hop were it
/// not a neighbour, §8.4; on the MMO's 2×2 grid with corners every
/// region is one hop away) and the pending walk is cancelled.
fn travel(world: &mut World, entity: Entity, waystone: usize) {
    let Some(&[x, z]) = WAYSTONES.get(waystone) else {
        return; // no such waystone
    };
    let mut e = world.entity_mut(entity);
    e.insert(Pos3::new(x, 0.0, z));
    e.remove::<MoveTarget>();
}

/// Hit the mob with wire id `target` if it is in THIS shard's world and
/// within reach; a mob brought to zero hit points is despawned — game
/// code, not a leave (the kit's ghost sweep, §8.2, tells every client
/// that saw it, on this shard and through the border strip).
fn attack(world: &mut World, attacker: Entity, target: u64) {
    let Some(&from) = world.get::<Pos3>(attacker) else {
        return;
    };
    let mut q = world.query::<(Entity, &WireId, &Pos3, &Mob)>();
    let Some((victim, at)) = q
        .iter(world)
        .find(|(_, w, _, _)| w.get() == target)
        .map(|(e, _, p, _)| (e, *p))
    else {
        return; // not a mob of this shard (e.g. one lent by a neighbour)
    };
    if from.dist(&at) > ATTACK_RANGE {
        return;
    }
    let Some(mut vitals) = world.get_mut::<Vitals>(victim) else {
        return;
    };
    vitals.hp = vitals.hp.saturating_sub(ATTACK_DAMAGE);
    if vitals.hp == 0 {
        world.despawn(victim);
    }
}
