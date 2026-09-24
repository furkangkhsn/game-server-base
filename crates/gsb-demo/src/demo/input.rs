//! The demo's input path: `MOVE_TO` decoding and application (the future
//! `Game::ingest`, KIT-ARCHITECTURE §4.3). The sequence rule it applies
//! is the kit's ([`InputSeq::admit`]).

use std::collections::HashMap;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::PlayerId;
use gsb_core::room::Action;
use prost::Message;

use crate::demo::components::MoveTarget;
use crate::demo::op;
use gsb_kit::game::InputSeq;

/// `MOVE_TO` ingestion, shared by all rooms: decode the game message,
/// guard against stale actions (connection not in the room) and vanished
/// entities, enforce the per-connection input sequence rule (the kit's
/// [`InputSeq::admit`] — a duplicate or reordered numbered input is
/// dropped silently, an unnumbered one always processes), and write the
/// [`MoveTarget`]. Anything else is ignored with a warning (the op code is
/// the router; undecodable payloads are a client bug, not a reason to
/// drop the connection).
pub(crate) fn ingest(
    player_entity: &HashMap<PlayerId, Entity>,
    world: &mut World,
    actions: &mut Vec<Action>,
    input: &mut InputSeq,
) {
    for action in actions.drain(..) {
        if action.op != op::MOVE_TO {
            continue;
        }
        let Ok(msg) = <crate::demo::game::MoveTo as Message>::decode(&action.payload[..]) else {
            tracing::warn!(?action.op, "undecodable MOVE_TO payload ignored");
            continue;
        };
        // Faz 2: actions are keyed by the STABLE player id the core's
        // binding stamped at ingest (bot-synthesized frames carry it
        // directly). A stale/unbound action drops right here — the same
        // silent-skip posture this path always had.
        let Some(entity) = player_entity.get(&action.player).copied() else {
            continue; // not in a room (stale action)
        };
        if world.get_entity(entity).is_err() {
            continue; // entity already gone
        }
        // The sequence rule (kit).
        if !input.admit(action.player, msg.seq) {
            continue; // duplicate / reordered late: dropped (normal race)
        }
        world.entity_mut(entity).insert(MoveTarget {
            x: msg.x as f32,
            y: msg.y as f32,
        });
    }
}
