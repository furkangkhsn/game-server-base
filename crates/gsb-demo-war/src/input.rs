//! The war game's input path (its `Game::ingest`, and
//! `ShardGame::ingest_seam` on the sharded path — the same path with the
//! cross-seam view): `MoveTo` and `Attack` decoding under the kit's
//! sequence rule ([`InputSeq::admit`]), one sequence space per session
//! across both messages. `Attack` resolves in [`crate::combat`].

use std::collections::HashMap;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::PlayerId;
use gsb_core::room::Action;
use gsb_kit::game::InputSeq;
use gsb_kit::sharded::Seam;
use prost::Message;

use crate::codec::{WarWire, from_dm};
use crate::combat::Combat;
use crate::components::{MoveTarget, Pos3};
use crate::op;
use crate::war::{Attack, MoveTo};

/// One decoded input.
enum Input {
    Move { x: f32, z: f32 },
    Attack { target: u64 },
}

/// Decode an action's payload into `(seq, input)`; `None` for an opcode
/// that is not the game's or an undecodable payload (a client bug: logged
/// and skipped, never a reason to drop the connection).
fn decode(action: &Action) -> Option<(u64, Input)> {
    let bytes = &action.payload[..];
    let decoded = match action.op {
        op::WAR_MOVE_TO => MoveTo::decode(bytes).map(|m| {
            let (x, z) = (from_dm(m.x), from_dm(m.z));
            (m.seq, Input::Move { x, z })
        }),
        op::WAR_ATTACK => {
            Attack::decode(bytes).map(|m| (m.seq, Input::Attack { target: m.target }))
        }
        _ => return None,
    };
    decoded
        .inspect_err(|_| tracing::warn!(op = action.op, "undecodable war input ignored"))
        .ok()
}

/// Decode and apply the tick's actions (global tick `tick`) for the
/// players with a live unit in this shard's world. `seam` is the
/// cross-seam view on the sharded path (`None`: attacks reach this
/// shard's own units only).
pub(crate) fn ingest(
    players: &HashMap<PlayerId, Entity>,
    world: &mut World,
    actions: &mut Vec<Action>,
    seq: &mut InputSeq,
    tick: u64,
    combat: &mut Combat,
    mut seam: Option<&mut Seam<'_, '_, WarWire>>,
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
                let at = Pos3::ground(x, z).clamped();
                world
                    .entity_mut(entity)
                    .insert(MoveTarget { x: at.x, z: at.z });
            }
            Input::Attack { target } => {
                combat.attack(world, seam.as_deref_mut(), entity, target, tick)
            }
        }
    }
}
