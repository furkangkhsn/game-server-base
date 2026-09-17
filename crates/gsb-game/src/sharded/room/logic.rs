//! The sharded room's shared game-logic contract.
//!
//! NOT split further: a trait impl is one block.

use std::collections::HashMap;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::{ConnectionId, EntityId, PlayerId};
use gsb_core::room::{Action, Admission, Detach, GameLogic, ResumeFound, TickCtx};
use gsb_core::rpc::RequestDecision;
use gsb_core::shard::BorderRecord;
use prost::Message;

use crate::components::{DEFAULT_SPEED, MoveTarget, Position, Speed, WireId};
use crate::op;
use crate::room::spawn_pos;
use crate::sharded::*;

impl GameLogic<World> for ShardedRoom {
    type GroupKey = ();
    type Strip = StripPos;

    fn snapshot_op(&self) -> u16 {
        op::WORLD_SNAPSHOT
    }

    fn private_op(&self) -> u16 {
        op::PRIVATE
    }

    /// One group per shard (see module docs, "Group key").
    fn group_of(&self, _world: &World, _player: PlayerId) -> Self::GroupKey {
        Default::default()
    }

    fn snapshot(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        _group: &Self::GroupKey,
        borrowed: &[BorderRecord<StripPos>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        // The shard's own world (wire id, truncated position), collected
        // while the query holds the world borrow.
        let mut own: Vec<(u64, i32, i32)> = Vec::new();
        {
            let mut query = world.query::<(&WireId, &Position)>();
            for (wire, pos) in query.iter(world) {
                own.push((wire.get(), pos.x as i32, pos.y as i32));
            }
        }

        // The content is the own world plus the borrowed boundary records
        // (the core has already sorted them by wire and filtered out any
        // that are this shard's own — the own record wins over the
        // neighbor's one-tick-stale copy of an entity that just crossed
        // in). The frame filter keeps only the records actually near this
        // shard (module docs, "Visibility model"): a neighbor's export
        // covers the neighbor's WHOLE boundary, and the parts of it far
        // from this shard (the neighbor's other edges) are not visible
        // here. "No change" includes the borrowed content: a neighbor's
        // boundary entity moving is a content change for this shard.
        let mut content: HashMap<u64, (i32, i32)> = HashMap::with_capacity(own.len());
        for (w, x, y) in &own {
            content.insert(*w, (*x, *y));
        }
        for rec in borrowed {
            if self.in_border_frame(rec.state.x, rec.state.y) {
                content
                    .entry(rec.wire)
                    .or_insert((rec.state.x, rec.state.y));
            }
        }

        if self.last == content {
            return false;
        }

        let mut snap = crate::game::WorldSnapshot {
            sequence: ctx.tick,
            entities: Vec::with_capacity(content.len()),
            removed: Vec::new(),
            cell_exits: Vec::new(),
            delta: false,
        };
        // Deterministic payload order (sort by wire — the own records are
        // in query order and the borrowed are already sorted; a single
        // sort over the merged content keeps the snapshot stable so the
        // ledger and the wire bytes are reproducible tick-to-tick).
        let mut entries: Vec<(&u64, &(i32, i32))> = content.iter().collect();
        entries.sort_unstable_by_key(|(w, _)| **w);
        for (w, &(x, y)) in entries {
            snap.entities
                .push(crate::game::EntityRecord { entity: *w, x, y });
        }
        snap.encode(out)
            .expect("protobuf encode into an in-memory buffer failed");

        self.encoded += content.len() as u64;
        self.last = content;
        true
    }

    fn encoded_records(&mut self) -> u64 {
        let n = self.encoded;
        self.encoded = 0;
        n
    }

    fn on_join(&mut self, world: &mut World, conn: ConnectionId) -> Admission {
        // The spawn point derives from the TRANSPORT session id (as it
        // always has — the load generator's home distribution pairs with
        // it); the stable player identity comes from this shard's
        // range-partitioned counter.
        let (x, y) = spawn_pos(conn, self.half);
        let player = self.mint_player();
        let wire = self.mint();
        let entity = world
            .spawn((Position { x, y }, Speed(DEFAULT_SPEED), WireId::new(wire)))
            .id();
        self.player_entity.insert(player, entity);
        self.entity_player.insert(entity, player);
        self.wire_entity.insert(wire, entity);
        self.own_wires.insert(wire);
        self.input
            .insert(player, crate::common::InputState::default());
        Admission {
            player,
            entity: wire,
        }
    }

    fn on_leave(&mut self, world: &mut World, player: PlayerId) {
        if let Some(entity) = self.player_entity.remove(&player)
            && world.get_entity(entity).is_ok()
        {
            if let Some(wire) = world.get::<WireId>(entity).copied() {
                self.wire_entity.remove(&wire.get());
                self.own_wires.remove(&wire.get());
            }
            self.entity_player.remove(&entity);
            self.input.remove(&player);
            world.despawn(entity);
        }
    }

    fn ingest(&mut self, world: &mut World, ctx: &TickCtx, actions: &mut Vec<Action>) {
        // The bot's synthesized frames (RECONNECT §9), resolved through
        // this shard's own wire table — the same shared helper the
        // single-world rooms use.
        let bots = self
            .park_ledger
            .values()
            .filter(|e| e.bot)
            .filter_map(|e| self.wire_entity.get(&e.wire).map(|&en| (e.player, en)));
        crate::common::synthesize_bot_moves(bots, world, ctx, actions);
        crate::common::ingest(&self.player_entity, world, actions, &mut self.input)
    }

    // -- the disconnect policy (see `crate::room::OpenRoom`, the shared
    //    hook bodies live in `crate::common`; this shard-side mirror keys
    //    its ledger by identity like the others but tracks the WIRE id,
    //    because that is what survives migrations) ----------------------

    fn on_disconnect(&mut self, world: &mut World, player: PlayerId, identity: &str) -> Detach {
        if self.park.grace.is_zero() || identity.is_empty() {
            return Detach::Despawn;
        }
        match self.player_entity.get(&player) {
            Some(&entity) => {
                let wire = world
                    .entity(entity)
                    .get::<WireId>()
                    .map(|w| w.get())
                    .unwrap_or_default();
                if wire != 0 {
                    self.park_ledger.insert(
                        identity.to_string(),
                        ShardParkRecord {
                            identity: identity.to_string(),
                            player,
                            wire,
                            bot: false,
                        },
                    );
                }
                Detach::Hold {
                    grace: Some(self.park.grace),
                    to: gsb_core::room::ExpireTo::AiHandover,
                }
            }
            None => Detach::Despawn,
        }
    }

    fn on_detach_expired(
        &mut self,
        _world: &mut World,
        player: PlayerId,
        to: gsb_core::room::ExpireTo,
    ) {
        match to {
            gsb_core::room::ExpireTo::Despawn => {
                self.park_ledger.retain(|_, e| e.player != player);
            }
            gsb_core::room::ExpireTo::AiHandover => {
                for e in self.park_ledger.values_mut().filter(|e| e.player == player) {
                    e.bot = true;
                }
            }
        }
    }

    fn resume_lookup(&self, _world: &World, identity: &str) -> ResumeFound {
        match self.park_ledger.get(identity) {
            Some(e) => ResumeFound::Held(e.player),
            None => ResumeFound::Never,
        }
    }

    fn on_resume(
        &mut self,
        _world: &mut World,
        identity: &str,
        _conn: ConnectionId,
        player: PlayerId,
        _entity: EntityId,
    ) {
        // Faz 2 shrink: consume the ledger entry + seq/ack reset. Nothing
        // to re-key — every table is keyed by the STABLE player id.
        self.park_ledger.remove(identity);
        self.input.remove(&player);
    }

    /// The per-connection private frame: the pending input
    /// acknowledgment (Section A) and this tick's queued RPC answers
    /// (Faz 3 — same-tick local replies plus later-tick worker reports /
    /// timeout sweeps; the shard actor queues them per session). The
    /// shard's snapshots are full, self-contained (one group per shard),
    /// so there is nothing else per-connection to deliver.
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

        // Orphan stamping, range-aware (the demo game has no NPCs, but the
        // broadcast set stays structural — "has a Position" — like the
        // other rooms): entities with a `Position` but no `WireId` get
        // the next serial FROM THIS SHARD'S RANGE (a shared counter would
        // mint ids outside the range and break the disjointness invariant).
        let orphans: Vec<Entity> = world
            .query_filtered::<(Entity, &Position), bevy_ecs::prelude::Without<WireId>>()
            .iter(world)
            .map(|(e, _)| e)
            .collect();
        for entity in orphans {
            let wire = self.mint();
            world.entity_mut(entity).insert(WireId::new(wire));
            self.wire_entity.insert(wire, entity);
            self.own_wires.insert(wire);
        }

        // Rebuild the border cache (positions just changed in the movement
        // system; `collect_border` cannot query — it takes `&World`).
        let (x0, x1, y0, y1) = self.rect();
        let b = self.border;
        self.border_cache.clear();
        let mut query = world.query::<(&WireId, &Position)>();
        for (wire, pos) in query.iter(world) {
            let near = (pos.x - x0) < b || (x1 - pos.x) < b || (pos.y - y0) < b || (y1 - pos.y) < b;
            if near {
                self.border_cache.push(BorderRecord {
                    wire: wire.get(),
                    state: StripPos {
                        x: pos.x as i32,
                        y: pos.y as i32,
                    },
                });
            }
        }
    }

    /// The demo's two request kinds on the SHARDED path (Faz 3 — the same
    /// contract as [`crate::room::OpenRoom::handle_request`], resolved
    /// against THIS shard's world):
    ///
    /// - `ABILITY` (room-local): range check + a real world mutation (the
    ///   entity gets a `MoveTarget`), answered in the same tick's private
    ///   frame. The requester is looked up by its STABLE player id, so a
    ///   session that resumed onto this shard resolves identically.
    /// - `ECONOMY` (external I/O): delegated to the economy service via
    ///   an owning future; the shard actor registers it pending and the
    ///   answer rides a later tick's private path. A migration of the
    ///   requesting session mid-flight drops its pending state at
    ///   migrate-out (`gsb_core::shard` module docs) — the answer is
    ///   forfeited by design, exactly like a detach.
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
                let dx = use_msg.x as f32 - pos.x;
                let dy = use_msg.y as f32 - pos.y;
                const RANGE: f32 = 10.0;
                if dx * dx + dy * dy > RANGE * RANGE {
                    return Some(RequestDecision::Reject(format!(
                        "target out of range ({} > {RANGE})",
                        (dx * dx + dy * dy).sqrt()
                    )));
                }
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
                // An OWNING future (a cheap sender clone inside): borrows
                // nothing from the shard (the `External` contract).
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
            // a normal "no handler" rejection.
            _ => None,
        }
    }

    /// This shard's match result (the Faz 3 promotion; the per-shard
    /// sibling of [`crate::room::OpenRoom::match_result`]): the FINAL
    /// snapshot of this shard's own region at teardown. One logical room
    /// therefore yields one such payload PER SHARD through the shared
    /// sink (all under the logical room id — the platform adapter
    /// concatenates/filters); the shards' ranges are disjoint, so the
    /// concatenated entity set is collision-free by construction.
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
