//! The sharded room's shared game-logic contract.
//!
//! NOT split further: a trait impl is one block.

use std::collections::HashMap;

use bevy_ecs::prelude::World;
use gsb_core::id::{ConnectionId, EntityId, PlayerId};
use gsb_core::room::{Action, Admission, Detach, GameLogic, ResumeFound, TickCtx};
use gsb_core::rpc::RequestDecision;
use gsb_core::shard::BorderRecord;

use crate::common::{put_entity_records, write_full_header};
use crate::game::{ShardGame, Wire};
use crate::sharded::room::*;
use crate::space::Partition;

impl<G: ShardGame, P: Partition<Wire<G>>> GameLogic<World> for ShardedRoom<G, P> {
    type GroupKey = ();
    type Strip = Wire<G>;

    fn snapshot_op(&self) -> u16 {
        G::SNAPSHOT_OP
    }

    fn private_op(&self) -> u16 {
        G::PRIVATE_OP
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
        borrowed: &[BorderRecord<Wire<G>>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        // The shard's own world (wire id, wire value).
        let own = self.own_records(world);

        // The content is the own world plus the borrowed boundary records
        // (the core has already sorted them by wire and filtered out any
        // that are this shard's own — the own record wins over the
        // neighbor's one-tick-stale copy of an entity that just crossed
        // in). The frame filter (`Partition::admits`) keeps only the
        // records actually near this shard (module docs, "Visibility
        // model"): a neighbor's export covers the neighbor's WHOLE
        // boundary, and the parts of it far from this shard (the
        // neighbor's other edges) are not visible here. "No change"
        // includes the borrowed content: a neighbor's boundary entity
        // moving is a content change for this shard.
        let mut content: HashMap<u64, Wire<G>> = HashMap::with_capacity(own.len());
        content.extend(own);
        for rec in borrowed {
            if self.partition.admits(self.index, &rec.state) {
                content.entry(rec.wire).or_insert_with(|| rec.state.clone());
            }
        }

        if self.last == content {
            return false;
        }

        // Deterministic payload order (sort by wire — the own records are
        // in query order and the borrowed are already sorted; a single
        // sort over the merged content keeps the snapshot stable so the
        // ledger and the wire bytes are reproducible tick-to-tick).
        let mut entries: Vec<(&u64, &Wire<G>)> = content.iter().collect();
        entries.sort_unstable_by_key(|(w, _)| **w);
        write_full_header(out, ctx.tick);
        put_entity_records(
            self.game.codec(),
            entries.into_iter().map(|(w, wire)| (*w, wire)),
            out,
        );

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
        self.on_join_as(world, conn, "")
    }

    fn on_join_as(&mut self, world: &mut World, conn: ConnectionId, identity: &str) -> Admission {
        // The stable player identity and the wire identity both come from
        // this shard's range-partitioned counter (player first, as it
        // always was); the spawn is the game's, given the authenticated
        // identity (the demo derives the spawn point from the TRANSPORT
        // session id — the load generator's home distribution pairs with
        // it; the MMO places a saved character by its player — the
        // registry routed the join to that character's shard).
        let player = self.mint_player();
        let entity = self.game.spawn_player_as(world, conn, identity);
        debug_assert!(
            world.entity(entity).contains::<Marker<G>>(),
            "Game::spawn_player must spawn the codec's Marker (the broadcast set)"
        );
        let wire_id = self.minter.mint();
        let wire = wire_id.get();
        world.entity_mut(entity).insert(wire_id);
        self.player_entity.insert(player, entity);
        self.entity_player.insert(entity, player);
        self.wire_entity.insert(wire, entity);
        self.entity_wire.insert(entity, wire);
        self.input.begin(player);
        Admission {
            player,
            entity: wire,
        }
    }

    fn on_leave(&mut self, world: &mut World, player: PlayerId) {
        if let Some(entity) = self.player_entity.remove(&player)
            && world.get_entity(entity).is_ok()
        {
            if let Some(wire) = self.entity_wire.remove(&entity) {
                self.wire_entity.remove(&wire);
            }
            self.entity_player.remove(&entity);
            self.input.end(player);
            world.despawn(entity);
        }
    }

    fn ingest(&mut self, world: &mut World, ctx: &TickCtx, actions: &mut Vec<Action>) {
        // The bot-fed parked players' synthesized frames (RECONNECT §9)
        // ride the same list as the wire input — the shared kit path.
        crate::common::ingest(
            &mut self.game,
            world,
            ctx,
            actions,
            &self.player_entity,
            &self.park_ledger,
            &mut self.input,
        )
    }

    // -- the disconnect policy: the same ledger and hook bodies as every
    //    single-world room (`crate::common`); what is shard-specific
    //    is that a ledger entry travels with its entity's migration
    //    (`ShardLogic` impl) -----------------------------------------

    fn on_disconnect(&mut self, _world: &mut World, player: PlayerId, identity: &str) -> Detach {
        crate::common::park_on_disconnect(
            &self.player_entity,
            player,
            identity,
            &self.park,
            &mut self.park_ledger,
        )
    }

    fn may_release(&mut self, world: &mut World, player: PlayerId) -> bool {
        crate::common::park_may_release(&mut self.game, &self.player_entity, world, player)
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
        // Faz 2 shrink: consume the ledger entry + seq/ack reset. Nothing
        // to re-key — every table is keyed by the STABLE player id.
        crate::common::park_resume(&mut self.park_ledger, &mut self.input, identity, player);
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
        self.step(world, ctx, None);
        // The tick's ONE change-window close (§4.4, §8.3 — see
        // `TeamRoom::update`); the spatial composite runs `step` and
        // closes after its own dirty pass instead.
        crate::common::close_change_window(world);
    }

    /// The game's request handlers on the SHARDED path (Faz 3 — the same
    /// contract as [`crate::room::OpenRoom::handle_request`],
    /// resolved against THIS shard's world and player table; the demo:
    /// `ABILITY` answered in the same tick, `ECONOMY` delegated to the
    /// economy service). A migration of the requesting session
    /// mid-flight drops its pending state at migrate-out
    /// (`gsb_core::shard` module docs) — the answer is forfeited by
    /// design, exactly like a detach.
    fn handle_request(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        req: &gsb_core::rpc::RpcRequest,
    ) -> Option<RequestDecision> {
        self.game
            .handle_request(world, ctx, req, &self.player_entity)
    }

    /// This shard's match result (the Faz 3 promotion; the per-shard
    /// sibling of [`crate::room::OpenRoom::match_result`]): the FINAL
    /// snapshot of this shard's own region at teardown, ordered by wire
    /// id. One logical room therefore yields one such payload PER SHARD
    /// through the shared sink (all under the logical room id — the
    /// platform adapter concatenates/filters); the shards' ranges are
    /// disjoint, so the concatenated entity set is collision-free by
    /// construction.
    fn match_result(&mut self, world: &mut World) -> Option<bytes::Bytes> {
        let mut records = self.own_records(world);
        records.sort_by_key(|(id, _)| *id);
        let mut out = bytes::BytesMut::new();
        // The shutdown snapshot has no live ticker: sequence 0 marks
        // "terminal" (live snapshots are strictly positive ticks).
        write_full_header(&mut out, 0);
        put_entity_records(
            self.game.codec(),
            records.iter().map(|(id, wire)| (*id, wire)),
            &mut out,
        );
        Some(out.freeze())
    }
}
