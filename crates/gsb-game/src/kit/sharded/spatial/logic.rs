//! The composite's game-logic contract: a cell group key over the
//! shard's own region, with the borrowed border strip folded in.
//!
//! NOT split further: a trait impl is one block.

use bevy_ecs::prelude::World;
use bytes::BufMut;
use gsb_core::id::{ConnectionId, EntityId, PlayerId};
use gsb_core::room::{Action, Admission, Detach, GameLogic, ResumeFound, TickCtx};
use gsb_core::rpc::RequestDecision;
use gsb_core::shard::BorderRecord;
use prost::encoding::varint::encode_varint;

use crate::kit::common::{Cell, assemble_group_packet, cell_of};
use crate::kit::identity::WireId;
use crate::kit::seam;
use crate::kit::seam::Position;
use crate::kit::sharded::*;

impl GameLogic<World> for ShardedSpatialRoom {
    type GroupKey = Cell;
    type Strip = StripPos;

    fn snapshot_op(&self) -> u16 {
        seam::WORLD_SNAPSHOT
    }

    fn private_op(&self) -> u16 {
        seam::PRIVATE
    }

    /// The connection's group is the cell its entity's records land in —
    /// read from the O(1) `last_cell` table (written by the dirty pass),
    /// world-position fallback for the pre-first-update window (a join or
    /// migration-in processed in the CONTROL phase of the very tick being
    /// broadcast).
    fn group_of(&self, world: &World, player: PlayerId) -> Cell {
        let Some(&entity) = self.inner.player_entity.get(&player) else {
            return Cell(0, 0);
        };
        if let Some(&c) = self.book.last_cell.get(&entity) {
            return c;
        }
        let pos = world
            .entity(entity)
            .get::<Position>()
            .copied()
            .unwrap_or_default();
        cell_of(pos.x as i32, pos.y as i32, self.cell_size)
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
        cell: &Cell,
        borrowed: &[BorderRecord<StripPos>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        self.ensure_ready(ctx, borrowed);
        assemble_group_packet(
            &mut self.pieces,
            &self.book,
            cell,
            &mut self.group_full_emitted,
            ctx.tick,
            out,
        )
    }

    /// Keep-alive on the cadence tick: a freshly encoded FULL of the
    /// group's view (a cached payload would be a delta — meaningless to
    /// re-send), whether the group emitted this tick or not. Same
    /// recovery contract as [`crate::kit::aoi::AoiRoom::keepalive`].
    fn keepalive(
        &mut self,
        _world: &mut World,
        _ctx: &TickCtx,
        group: &Cell,
        _last: Option<&bytes::Bytes>,
        out: &mut bytes::BytesMut,
    ) -> bool {
        self.ensure_rolled();
        self.group_full_emitted.insert(*group);
        let full = self.pieces.full_view(&self.book.buckets, self.tick, group);
        out.extend_from_slice(&full);
        true
    }

    fn encoded_records(&mut self) -> u64 {
        self.pieces.take_encoded()
    }

    fn on_join(&mut self, world: &mut World, conn: ConnectionId) -> Admission {
        let admission = self.inner.on_join(world, conn);
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
            let wire = world.entity(entity).get::<WireId>().map(|w| w.get());
            let cell = self.book.last_cell.get(&entity).copied();
            if let (Some(wire), Some(cell)) = (wire, cell) {
                self.book.pending_removals.push((entity, wire, cell));
            }
        }
        self.inner.on_leave(world, player);
        self.conn_view.remove(&player);
    }

    fn ingest(&mut self, world: &mut World, ctx: &TickCtx, actions: &mut Vec<Action>) {
        self.inner.ingest(world, ctx, actions)
    }

    fn on_disconnect(&mut self, world: &mut World, player: PlayerId, identity: &str) -> Detach {
        self.inner.on_disconnect(world, player, identity)
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
        self.conn_view.remove(&player);
    }

    /// The per-connection private frame: the one-shot FULL view for a
    /// connection without a baseline for its CURRENT cell (join, resume,
    /// cell crossing, migration arrival) — skipped when the group's own
    /// emission this batch was already a full — plus the ordinary input
    /// ack / queued RPC answers.
    fn private(
        &mut self,
        _world: &mut World,
        player: PlayerId,
        group: &Cell,
        responses: &[gsb_core::rpc::RpcReply],
        out: &mut bytes::BytesMut,
    ) -> bool {
        self.ensure_rolled();
        let c = *group;
        if self.conn_view.get(&player).copied() != Some(c) {
            if self.group_full_emitted.contains(&c) {
                // The group's own full is ahead of this frame in the same
                // batch: it already baselined the connection.
                self.conn_view.insert(player, c);
            } else {
                // The one-shot private full: pre-encoded WorldSnapshot
                // bytes inside the Private message's snapshot oneof
                // (field 2, length-delimited); queued RPC answers ride
                // the SAME frame (field 3).
                let full = self.pieces.full_view(&self.book.buckets, self.tick, &c);
                out.put_u8(0x12); // Private field 2 (snapshot), LEN
                encode_varint(full.len() as u64, out);
                out.extend_from_slice(&full);
                crate::kit::common::append_responses(responses, out);
                self.conn_view.insert(player, c);
                return true;
            }
        }
        crate::kit::common::emit_private(&mut self.inner.input, player, responses, out)
    }

    fn update(&mut self, world: &mut World, ctx: &TickCtx) {
        // Grid half: systems, range-aware orphan stamping, border-cache
        // rebuild (positions just changed).
        self.inner.update(world, ctx);
        // Spatial half: clear the per-tick state, run the own-entity
        // dirty pass, apply parked removals — but do NOT roll yet: the
        // borrowed strip arrives later than `update` (module docs, "why
        // the occupancy/birth roll cannot live in `update`"); the first
        // broadcast-phase call integrates it and rolls against the final
        // content.
        self.book.begin_tick();
        self.pieces.begin_tick();
        self.group_full_emitted.clear();
        self.tick = ctx.tick;
        self.book.dirty_pass(world, self.cell_size);
        self.book.apply_removals();
        // Close this tick's bevy change window (the core never calls
        // this — there is no system scheduler here; see
        // `docs/DESIGN.md` §7).
        world.clear_trackers();
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

// The sharding seam: pure delegation — the composite changes WHAT a
