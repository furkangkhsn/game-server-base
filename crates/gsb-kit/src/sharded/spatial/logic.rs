//! The composite's game-logic contract: a cell group key over the
//! shard's own region, with the borrowed border strip folded in.
//!
//! NOT split further: a trait impl is one block. (The spatial half of
//! its `update`, shared with the sharded path's `update_seam`, follows
//! it as an inherent method.)

use bevy_ecs::prelude::World;
use gsb_core::id::{ConnectionId, EntityId, PlayerId};
use gsb_core::room::{Action, Admission, Detach, GameLogic, ResumeFound, TickCtx};
use gsb_core::rpc::RequestDecision;
use gsb_core::shard::BorderRecord;

use crate::codec::RecordCodec;
use crate::common::assemble_group_packet;
use crate::game::{Game, ShardGame, Wire};
use crate::sharded::*;
use crate::space::{CellSpace, Partition};

/// The game's broadcast marker (the codec's `Marker`).
type Marker<G> = <<G as Game>::Codec as RecordCodec>::Marker;
/// The game's record query (the codec's `Query`).
type RecordQuery<G> = <<G as Game>::Codec as RecordCodec>::Query;

impl<G, P, S> GameLogic<World> for ShardedSpatialRoom<G, P, S>
where
    G: ShardGame,
    P: Partition<Wire<G>>,
    S: CellSpace<Wire<G>>,
{
    type GroupKey = S::Cell;
    type Strip = Wire<G>;

    fn snapshot_op(&self) -> u16 {
        G::SNAPSHOT_OP
    }

    fn private_op(&self) -> u16 {
        G::PRIVATE_OP
    }

    /// The connection's group is the cell its entity's records land in —
    /// read from the O(1) `last_cell` table (written by the dirty pass),
    /// with the fallback of the entity's current record for the
    /// pre-first-update window (a join or migration-in processed in the
    /// CONTROL phase of the very tick being broadcast); the space's
    /// default cell when there is no entity or no record.
    fn group_of(&self, world: &World, player: PlayerId) -> S::Cell {
        let Some(&entity) = self.inner.player_entity.get(&player) else {
            return S::Cell::default();
        };
        if let Some(c) = self.book.cell_of_entity(&entity) {
            return c;
        }
        match world.entity(entity).get_components::<RecordQuery<G>>() {
            Ok(item) => self.space.cell_of(&self.inner.game.codec().wire(item)),
            Err(_) => S::Cell::default(),
        }
    }

    /// This cell's packet over the shard's own region content PLUS the
    /// integrated borrowed strip: a fresh group gets the 3×3 full; an
    /// established group gets the delta passes — all through the shared
    /// engine (the strip's enters/exits/moves sit in the same change
    /// lists the own movers wrote, so nothing here knows the difference).
    fn snapshot(
        &mut self,
        _world: &mut World,
        ctx: &TickCtx,
        cell: &S::Cell,
        borrowed: &[BorderRecord<Wire<G>>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        self.ensure_ready(ctx, borrowed);
        assemble_group_packet(
            &mut self.pieces,
            &self.book,
            self.inner.game.codec(),
            &self.space,
            cell,
            &mut self.group_full_emitted,
            out,
        )
    }

    /// Keep-alive on the cadence tick: a freshly encoded FULL of the
    /// group's view (a cached payload would be a delta — meaningless to
    /// re-send), whether the group emitted this tick or not. Same
    /// recovery contract as [`crate::aoi::AoiRoom::keepalive`].
    fn keepalive(
        &mut self,
        _world: &mut World,
        _ctx: &TickCtx,
        group: &S::Cell,
        _last: Option<&bytes::Bytes>,
        out: &mut bytes::BytesMut,
    ) -> bool {
        self.ensure_rolled();
        self.group_full_emitted.insert(*group);
        let full = self.pieces.full_view(
            self.inner.game.codec(),
            &self.space,
            &self.book.buckets,
            group,
        );
        out.extend_from_slice(&full);
        true
    }

    fn encoded_records(&mut self) -> u64 {
        self.pieces.take_encoded()
    }

    fn on_join(&mut self, world: &mut World, conn: ConnectionId) -> Admission {
        self.on_join_as(world, conn, "")
    }

    fn on_join_as(&mut self, world: &mut World, conn: ConnectionId, identity: &str) -> Admission {
        let admission = self.inner.on_join_as(world, conn, identity);
        // Member bookkeeping for the birth arithmetic (the wrapped shard
        // owns the tables; the spatial layer owns membership).
        if let Some(&entity) = self.inner.player_entity.get(&admission.player) {
            self.book.members.insert(entity);
        }
        admission
    }

    fn on_leave(&mut self, world: &mut World, player: PlayerId) {
        // Park the despawn removal BEFORE delegating (the delegate
        // despawns): despawns are not component writes, so the dirty pass
        // cannot see them. A join+leave inside one tick parks nothing
        // (never bucketed — the `last_cell` guard).
        if let Some(&entity) = self.inner.player_entity.get(&player) {
            self.book.members.remove(&entity);
            if self.book.last_cell.contains_key(&entity) {
                self.book.pending_removals.push((entity, true));
            }
        }
        self.inner.on_leave(world, player);
        self.baselines.forget(player);
    }

    fn ingest(&mut self, world: &mut World, ctx: &TickCtx, actions: &mut Vec<Action>) {
        self.inner.ingest(world, ctx, actions)
    }

    fn on_disconnect(&mut self, world: &mut World, player: PlayerId, identity: &str) -> Detach {
        self.inner.on_disconnect(world, player, identity)
    }

    fn may_release(&mut self, world: &mut World, player: PlayerId) -> bool {
        self.inner.may_release(world, player)
    }

    fn on_detach_expired(
        &mut self,
        world: &mut World,
        player: PlayerId,
        to: gsb_core::room::ExpireTo,
    ) {
        self.inner.on_detach_expired(world, player, to)
    }

    fn resume_lookup(&self, world: &World, identity: &str) -> ResumeFound {
        self.inner.resume_lookup(world, identity)
    }

    fn on_resume(
        &mut self,
        world: &mut World,
        identity: &str,
        conn: ConnectionId,
        player: PlayerId,
        entity: EntityId,
    ) {
        self.inner.on_resume(world, identity, conn, player, entity);
        // The resumed SESSION has no view baseline: the next private frame
        // delivers a fresh one-shot full (same contract as a re-join).
        self.baselines.forget(player);
    }

    /// The per-connection private frame: the one-shot FULL view for a
    /// connection without a baseline for its CURRENT cell (join, resume,
    /// cell crossing, migration arrival) — skipped when the group's own
    /// emission this batch was already a full — plus the ordinary input
    /// ack / queued RPC answers.
    fn private(
        &mut self,
        world: &mut World,
        player: PlayerId,
        group: &S::Cell,
        responses: &[gsb_core::rpc::RpcReply],
        out: &mut bytes::BytesMut,
    ) -> bool {
        self.ensure_rolled();
        let c = *group;
        // The group's own full ahead of this frame in the same batch
        // already baselines the connection.
        let group_full = &self.group_full_emitted;
        if self
            .baselines
            .owed(player, c, self.tick, || group_full.contains(&c))
        {
            // The one-shot private full (or its re-send after a fan-out
            // drop took the baseline — F11): pre-encoded WorldSnapshot
            // bytes inside the Private message's snapshot oneof; queued
            // RPC answers ride the SAME frame (the shared frame writer,
            // `crate::common::emit_private_full`).
            let full =
                self.pieces
                    .full_view(self.inner.game.codec(), &self.space, &self.book.buckets, &c);
            return crate::common::emit_private_full(
                &mut self.inner.game,
                world,
                &self.inner.player_entity,
                &mut self.inner.input,
                player,
                &full,
                responses,
                out,
            );
        }
        crate::common::emit_private_frame(
            &mut self.inner.game,
            world,
            &self.inner.player_entity,
            &mut self.inner.input,
            player,
            responses,
            out,
        )
    }

    /// A dropped batch (F11): the ack and the session payload its
    /// private frame carried are owed again, and when it carried view
    /// content (the group frame, or the one-shot full) the baseline is
    /// taken back — the next frame re-sends a one-shot full, paced
    /// against a storm (`Baselines`).
    fn on_batch_dropped(&mut self, _world: &mut World, player: PlayerId, snapshot: bool) {
        let full = self.inner.input.dropped(player);
        self.baselines.dropped(player, self.tick, snapshot || full);
    }

    /// The channel took a batch again (F11): a re-send the storm
    /// pacing held back goes out on the next frame.
    fn on_batch_resumed(&mut self, _world: &mut World, player: PlayerId) {
        self.baselines.resumed(player);
    }

    fn update(&mut self, world: &mut World, ctx: &TickCtx) {
        // Grid half: systems, shard-aware orphan stamping, border-cache
        // rebuild (positions just changed).
        self.inner.step(world, ctx, None);
        self.spatial_step(world, ctx);
    }

    fn handle_request(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        req: &gsb_core::rpc::RpcRequest,
    ) -> Option<RequestDecision> {
        self.inner.handle_request(world, ctx, req)
    }

    fn match_result(&mut self, world: &mut World) -> Option<bytes::Bytes> {
        self.inner.match_result(world)
    }
}

impl<G, P, S> ShardedSpatialRoom<G, P, S>
where
    G: ShardGame,
    P: Partition<Wire<G>>,
    S: CellSpace<Wire<G>>,
{
    /// The spatial half of the tick body, after the grid half
    /// (`ShardedRoom::step`) — shared by `update` and the sharded path's
    /// `update_seam`.
    pub(in crate::sharded) fn spatial_step(&mut self, world: &mut World, ctx: &TickCtx) {
        // Spatial half: clear the per-tick state, run the own-entity
        // dirty pass, apply parked removals — but do NOT roll yet: the
        // borrowed strip arrives later than `update` (module docs, "why
        // the occupancy/birth roll cannot live in `update`"); the first
        // broadcast-phase call integrates it and rolls against the final
        // content.
        self.book.begin_tick();
        self.pieces.begin_tick(ctx.tick);
        self.group_full_emitted.clear();
        self.tick = ctx.tick;
        self.book
            .dirty_pass(world, self.inner.game.codec(), &self.space);
        self.book.apply_removals();
        // Every despawn nobody parked (game code despawning an NPC —
        // §8.2), read before this tick's change-window close.
        self.book.sweep_removed::<Marker<G>>(world);
        // The tick's ONE change-window close (§4.4: the kit owns it; a
        // game hook never calls it — the core has no system scheduler
        // that would; see `docs/DESIGN.md` §7).
        crate::common::close_change_window(world);
    }
}

// The sharding seam: pure delegation — the composite changes WHAT a
