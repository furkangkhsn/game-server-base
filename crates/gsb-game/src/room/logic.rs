//! The open room's game-logic implementation: everyone sees the whole
//! world, so the snapshot is one group and the seams are the plain
//! ones.
//!
//! NOT split further: a trait impl is one block.

use bevy_ecs::prelude::World;
use gsb_core::id::{ConnectionId, EntityId, PlayerId};
use gsb_core::room::{Action, Admission, Detach, GameLogic, ResumeFound, RoomLogic, TickCtx};
use gsb_core::rpc::RequestDecision;
use prost::Message;

use crate::components::{MoveTarget, Position, WireId};
use crate::op;
use crate::room::*;

impl GameLogic<World> for OpenRoom {
    // One group per room: everyone sees the whole world. (The interface
    // supports finer groupings, e.g. `GroupKey = ConnectionId` — but then
    // the `last` ledger above must be keyed by group; see the module
    // docs and `RoomLogic::snapshot`.)
    type GroupKey = ();
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        op::WORLD_SNAPSHOT
    }
    fn private_op(&self) -> u16 {
        op::PRIVATE
    }

    fn group_of(&self, _world: &World, _player: PlayerId) -> Self::GroupKey {
        Default::default()
    }

    fn snapshot(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        _group: &Self::GroupKey,
        // Single-room execution: no boundary records exist here (the
        // sharded actor folds its border exchange into this same seam).
        _borrowed: &[gsb_core::shard::BorderRecord<()>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        // Collect the broadcastable state (wire id, truncated wire
        // position) while the query holds the world borrow.
        let mut current: Vec<(u64, i32, i32)> = Vec::new();
        {
            // Identity assignment (module docs, "Wire identity"): entities
            // with a `Position` but no `WireId` — spawned outside
            // `on_join` (bullets, NPCs, traps, …) — are stamped with the
            // next serial here, so the broadcast set is exactly "has a
            // `Position`" and no entity can be silently invisible (see
            // `common::stamp_orphans` for the two-pass pattern and
            // idempotence). The full query below runs *after* the
            // stamps, so it sees every broadcastable entity exactly once
            // (stamped and pre-stamped alike).
            crate::common::stamp_orphans(&mut self.next_wire_id, world);
            let mut query = world.query::<(&WireId, &Position)>();
            for (wire_id, pos) in query.iter(world) {
                current.push((wire_id.get(), pos.x as i32, pos.y as i32));
            }
        }

        // "No change" = identical wire content: the same set of entities
        // at the same (truncated) positions. A membership change
        // (join/leave) or any position change flips it. The comparison is
        // on exactly what the snapshot carries (see module docs).
        let changed = self.last.len() != current.len()
            || current.iter().any(|(entity, x, y)| {
                self.last
                    .get(entity)
                    .map(|(lx, ly)| *x != *lx || *y != *ly)
                    .unwrap_or(true)
            });
        if !changed {
            return false;
        }

        let mut snap = crate::game::WorldSnapshot {
            sequence: ctx.tick,
            entities: Vec::with_capacity(current.len()),
            removed: Vec::new(),
            cell_exits: Vec::new(),
            delta: false,
        };
        for (entity, x, y) in &current {
            snap.entities.push(crate::game::EntityRecord {
                entity: *entity,
                x: *x,
                y: *y,
            });
        }
        // Encoding into an in-memory buffer cannot fail (no I/O, unbounded
        // capacity); treat a failure as a bug rather than dropping the
        // snapshot.
        snap.encode(out)
            .expect("protobuf encode into an in-memory buffer failed");

        self.last.clear();
        for (entity, x, y) in &current {
            self.last.insert(*entity, (*x, *y));
        }
        self.encoded += current.len() as u64;
        true
    }

    fn encoded_records(&mut self) -> u64 {
        let n = self.encoded;
        self.encoded = 0;
        n
    }

    fn on_join(&mut self, world: &mut World, conn: ConnectionId) -> Admission {
        // Shared spawn path (`common::on_join`): deterministic spawn point
        // (this room's spawn map, see the `spawn_half` field), a fresh
        // stable player identity + wire identity through their minting
        // counters, and the player→entity table update. The entity value
        // is also returned to the joiner in `JOIN_ROOM_RESULT`, so both
        // paths share one space. No spawn event: membership is expressed
        // by presence in the next snapshot, which now includes the new
        // entity (the join happened in the control phase, before this
        // tick's broadcast).
        crate::common::on_join(
            &mut self.player_entity,
            &mut self.next_player_id,
            &mut self.next_wire_id,
            self.spawn_half,
            world,
            conn,
            &mut self.input,
        )
    }

    fn on_leave(&mut self, world: &mut World, player: PlayerId) {
        crate::common::on_leave(&mut self.player_entity, world, player, &mut self.input)
    }

    // -- the disconnect policy (docs/RECONNECT.md §3/§5/§9; the hook
    //    bodies are shared with every demo room — see `crate::common`) --

    fn on_disconnect(&mut self, _world: &mut World, player: PlayerId, identity: &str) -> Detach {
        crate::common::park_on_disconnect(
            &self.player_entity,
            player,
            identity,
            &self.park,
            &mut self.park_ledger,
        )
    }

    fn on_detach_expired(
        &mut self,
        _world: &mut World,
        player: PlayerId,
        to: gsb_core::room::ExpireTo,
    ) {
        crate::common::park_on_expire(&mut self.park_ledger, player, to);
    }

    fn resume_lookup(&self, world: &World, identity: &str) -> ResumeFound {
        crate::common::park_lookup(world, &self.park_ledger, identity)
    }

    fn on_resume(
        &mut self,
        _world: &mut World,
        identity: &str,
        _conn: ConnectionId,
        player: PlayerId,
        _entity: EntityId,
    ) {
        // Faz 2 shrink: ledger consume + seq/ack reset only — the
        // player-keyed tables kept their keys across the disconnect.
        crate::common::park_resume(&mut self.park_ledger, &mut self.input, identity, player);
    }

    fn ingest(&mut self, world: &mut World, ctx: &TickCtx, actions: &mut Vec<Action>) {
        // The bot's synthesized frames ride the SAME list as wire input
        // (RECONNECT §9: "bot = bağlantısız girdi kaynağı" — an input
        // source without a connection): one decode/sequence/move path for
        // both.
        crate::common::synthesize_bot_moves(
            self.park_ledger
                .values()
                .filter(|e| e.bot)
                .map(|e| (e.player, e.entity)),
            world,
            ctx,
            actions,
        );
        crate::common::ingest(&self.player_entity, world, actions, &mut self.input)
    }

    /// The per-connection input acknowledgment (the group snapshot is
    /// shared; the ack is not — `GameLogic::private` is the per-connection
    /// seam of the batch, so the ack rides the same delivery as the
    /// snapshot, a few bytes per advanced tick, zero otherwise).
    fn private(
        &mut self,
        _world: &mut World,
        player: PlayerId,
        _group: &(),
        responses: &[gsb_core::rpc::RpcReply],
        out: &mut bytes::BytesMut,
    ) -> bool {
        crate::common::emit_private(&mut self.input, player, responses, out)
    }

    fn update(&mut self, world: &mut World, ctx: &TickCtx) {
        crate::common::run_systems(&mut self.runner, world, ctx);
    }

    /// The demo's two request kinds (the RPC pattern's two halves, see
    /// `game.proto`):
    ///
    /// - `ABILITY` (room-local): the answer is computed in this tick —
    ///   a range check against the requester's current position and a
    ///   real world mutation (the entity gets a `MoveTarget` toward the
    ///   requested point) — and returned in the same tick's private
    ///   frame. The same-tick snapshot the requester already receives
    ///   reflects the mutation: the request and its effect share a tick.
    /// - `ECONOMY` (external I/O): the answer needs a round trip to the
    ///   economy service (see `crate::economy`), which the room cannot
    ///   await. The decision is `External` with an owning future; the
    ///   core registers the request as pending, runs the future in a
    ///   worker task, and delivers the answer on a later tick through
    ///   the same private path (the client sees one shape for both).
    fn handle_request(
        &mut self,
        world: &mut World,
        _ctx: &TickCtx,
        req: &gsb_core::rpc::RpcRequest,
    ) -> Option<RequestDecision> {
        match req.op {
            op::ABILITY => {
                let Ok(use_msg) = <crate::game::AbilityUse as Message>::decode(&req.payload[..])
                else {
                    return Some(RequestDecision::Reject(
                        "undecodable AbilityUse payload".into(),
                    ));
                };
                let Some(entity) = self.player_entity.get(&req.player).copied() else {
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
                let res = crate::game::AbilityResult {
                    ok: true,
                    reason: String::new(),
                };
                Some(RequestDecision::Reply(res.encode_to_vec().into()))
            }
            op::ECONOMY => {
                let Ok(buy) = <crate::game::BuyItem as Message>::decode(&req.payload[..]) else {
                    return Some(RequestDecision::Reject(
                        "undecodable BuyItem payload".into(),
                    ));
                };
                let Some(economy) = self.economy.clone() else {
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
                            let res = crate::game::BuyResult {
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

    /// The demo's match result (the control plane's result seam, feature
    /// A): the room's FINAL snapshot at shutdown — the complete,
    /// self-contained state the game considers "the result" (who was in
    /// the room, where they ended up). Encoded as the ordinary
    /// `WorldSnapshot` message (the platform's adapter decodes it
    /// against the same schema it uses for live snapshots).
    fn match_result(&mut self, world: &mut World) -> Option<bytes::Bytes> {
        let mut entities: Vec<crate::game::EntityRecord> = Vec::new();
        {
            let mut query = world.query::<(&WireId, &Position)>();
            for (wire_id, pos) in query.iter(world) {
                entities.push(crate::game::EntityRecord {
                    entity: wire_id.get(),
                    x: pos.x as i32,
                    y: pos.y as i32,
                });
            }
        }
        entities.sort_by_key(|e| e.entity);
        let snap = crate::game::WorldSnapshot {
            // The shutdown snapshot has no live ticker: sequence 0 marks
            // "terminal" (live snapshots are strictly positive ticks).
            sequence: 0,
            entities,
            removed: Vec::new(),
            cell_exits: Vec::new(),
            delta: false,
        };
        let mut out = bytes::BytesMut::new();
        snap.encode(&mut out)
            .expect("protobuf encode into an in-memory buffer failed");
        Some(out.freeze())
    }
}

// Faz 3 trait promotion: `handle_request` / `match_result` moved onto the
// shared `GameLogic` supertrait above; this impl remains the compile-time
// marker that OpenRoom targets the single-room actor.
impl RoomLogic<World> for OpenRoom {}
