//! The arena's input path (its `Game::ingest`): `arena.MoveTo` decoding
//! under the kit's sequence rule ([`InputSeq::admit`]).

use std::collections::HashMap;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::PlayerId;
use gsb_core::room::Action;
use gsb_kit::game::InputSeq;
use prost::Message;

use crate::arena::MoveTo;
use crate::codec::from_cm;
use crate::components::{MoveTarget3, Pos3};
use crate::op;

/// Decode and apply the tick's actions: every `ARENA_MOVE_TO` whose
/// player still has a live unit and whose sequence number the kit
/// admits becomes the unit's [`MoveTarget3`] (clamped into the arena's
/// volume). Other opcodes are not the arena's and are dropped; an
/// undecodable payload is a client bug, logged and skipped (never a
/// reason to drop the connection).
pub(crate) fn ingest(
    players: &HashMap<PlayerId, Entity>,
    world: &mut World,
    actions: &mut Vec<Action>,
    seq: &mut InputSeq,
) {
    for action in actions.drain(..) {
        if action.op != op::ARENA_MOVE_TO {
            continue;
        }
        let Ok(msg) = MoveTo::decode(&action.payload[..]) else {
            tracing::warn!(op = action.op, "undecodable arena MoveTo ignored");
            continue;
        };
        // Actions are keyed by the STABLE player id the core stamped
        // (bot-synthesized frames carry it directly).
        let Some(&entity) = players.get(&action.player) else {
            continue; // not (or no longer) in this room
        };
        if world.get_entity(entity).is_err() {
            continue; // the unit is gone
        }
        // The kit's rule: a duplicate or reordered-late numbered input
        // is dropped silently; seq 0 always passes.
        if !seq.admit(action.player, msg.seq) {
            continue;
        }
        let target = Pos3::new(from_cm(msg.x), from_cm(msg.y), from_cm(msg.z)).clamped();
        world.entity_mut(entity).insert(MoveTarget3(target));
    }
}
