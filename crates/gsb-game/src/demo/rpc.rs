//! The demo's RPC request handlers (the future `Game::handle_request`,
//! KIT-ARCHITECTURE §4.3): one body, shared by every room that answers
//! requests (the open room and the sharded rooms).

use std::collections::HashMap;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::PlayerId;
use gsb_core::rpc::RequestDecision;
use prost::Message;

use crate::demo::components::{MoveTarget, Position};
use crate::demo::economy::EconomyService;
use crate::demo::op;

/// The demo's two request kinds (the RPC pattern's two halves, see
/// `game.proto`), resolved against the calling room's world and its
/// player→entity table:
///
/// - `ABILITY` (room-local): the answer is computed in this tick —
///   a range check against the requester's current position and a
///   real world mutation (the entity gets a `MoveTarget` toward the
///   requested point) — and returned in the same tick's private
///   frame. The same-tick snapshot the requester already receives
///   reflects the mutation: the request and its effect share a tick.
///   The requester is looked up by its STABLE player id, so a session
///   that resumed (or, on the sharded path, whose shard hosts it)
///   resolves identically.
/// - `ECONOMY` (external I/O): the answer needs a round trip to the
///   economy service (see `crate::demo::economy`), which the room cannot
///   await. The decision is `External` with an owning future; the
///   core registers the request as pending, runs the future in a
///   worker task, and delivers the answer on a later tick through
///   the same private path (the client sees one shape for both).
///   `economy = None` answers with a normal "not configured" rejection.
pub(crate) fn handle_request(
    player_entity: &HashMap<PlayerId, Entity>,
    economy: Option<&EconomyService>,
    world: &mut World,
    req: &gsb_core::rpc::RpcRequest,
) -> Option<RequestDecision> {
    match req.op {
        op::ABILITY => {
            let Ok(use_msg) = <crate::demo::game::AbilityUse as Message>::decode(&req.payload[..])
            else {
                return Some(RequestDecision::Reject(
                    "undecodable AbilityUse payload".into(),
                ));
            };
            let Some(entity) = player_entity.get(&req.player).copied() else {
                return Some(RequestDecision::Reject(
                    "no entity for this connection".into(),
                ));
            };
            let Ok(he) = world.get_entity(entity) else {
                return Some(RequestDecision::Reject("entity already gone".into()));
            };
            let Some(pos) = he.get::<Position>().copied() else {
                return Some(RequestDecision::Reject("entity has no position".into()));
            };
            // Room-local validation (a demo rule: the ability reaches
            // 10 world units). Runs synchronously in this tick.
            let dx = use_msg.x as f32 - pos.x;
            let dy = use_msg.y as f32 - pos.y;
            const RANGE: f32 = 10.0;
            if dx * dx + dy * dy > RANGE * RANGE {
                return Some(RequestDecision::Reject(format!(
                    "target out of range ({} > {RANGE})",
                    (dx * dx + dy * dy).sqrt()
                )));
            }
            // The effect: a real mutation, applied in this tick (it
            // rides the same tick's snapshot out to the group).
            world.entity_mut(entity).insert(MoveTarget {
                x: use_msg.x as f32,
                y: use_msg.y as f32,
            });
            let res = crate::demo::game::AbilityResult {
                ok: true,
                reason: String::new(),
            };
            Some(RequestDecision::Reply(res.encode_to_vec().into()))
        }
        op::ECONOMY => {
            let Ok(buy) = <crate::demo::game::BuyItem as Message>::decode(&req.payload[..]) else {
                return Some(RequestDecision::Reject(
                    "undecodable BuyItem payload".into(),
                ));
            };
            let Some(economy) = economy.cloned() else {
                return Some(RequestDecision::Reject(
                    "economy service not configured".into(),
                ));
            };
            // The room captures a CLONE of the service handle (a
            // cheap sender clone) — the future owns everything it
            // needs and borrows nothing from the room (see the
            // `RequestDecision::External` contract).
            let fut = async move {
                match economy.buy(buy.kind).await {
                    Ok(price) => {
                        let res = crate::demo::game::BuyResult {
                            ok: true,
                            reason: String::new(),
                            price,
                        };
                        Ok(res.encode_to_vec().into())
                    }
                    Err(reason) => Err(reason),
                }
            };
            Some(RequestDecision::External(Box::pin(fut)))
        }
        // Not a request op this logic handles: the core answers with
        // a normal "no handler" rejection (no waiting on a timeout).
        _ => None,
    }
}
