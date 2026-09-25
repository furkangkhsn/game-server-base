//! The AOI room's game-logic implementation: the group key is a cell,
//! and the snapshot is the per-cell delta ledger.
//!
//! NOT split further: a trait impl is one block.

use bevy_ecs::prelude::World;
use bytes::BufMut;
use gsb_core::id::{ConnectionId, EntityId, PlayerId};
use gsb_core::room::{Action, Admission, Detach, GameLogic, ResumeFound, RoomLogic, TickCtx};
use prost::encoding::varint::encode_varint;

use crate::aoi::*;
use crate::codec::RecordCodec;
use crate::common::assemble_group_packet;
use crate::game::{Game, Wire};
use crate::space::CellSpace;

/// The game's broadcast marker (the codec's `Marker`).
type Marker<G> = <<G as Game>::Codec as RecordCodec>::Marker;
/// The game's record query (the codec's `Query`).
type RecordQuery<G> = <<G as Game>::Codec as RecordCodec>::Query;

impl<G: Game, S: CellSpace<Wire<G>>> GameLogic<World> for AoiRoom<G, S> {
    type GroupKey = S::Cell;
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        G::SNAPSHOT_OP
    }
    fn private_op(&self) -> u16 {
        G::PRIVATE_OP
    }

    /// The connection's group is the cell its entity's records land in
    /// (re-evaluated every tick by the room — a crossing player changes
    /// cell and thus group, and starts receiving the new cell's packets;
    /// it receives a one-shot private full of the new view, see
    /// `private`). Read from the `last_cell` table (item (b): 4a calls
    /// this once per connection per tick — a world query per connection
    /// is O(N) ECS work per tick with no structural way to shrink it;
    /// the table makes it O(1) and it stays current because `update`
    /// always precedes the broadcast phase). The world fallback covers
    /// the window where the table has no entry yet — the join itself
    /// (the core asks for the joiner's group right after `on_join`,
    /// before any `update`): the cell of the entity's current record,
    /// read from the world. An entity without the record's components
    /// (defensive — the join always spawns them) falls back to the
    /// space's default cell, as does a player without an entity.
    fn group_of(&self, world: &World, player: PlayerId) -> S::Cell {
        let Some(&entity) = self.player_entity.get(&player) else {
            return S::Cell::default();
        };
        if let Some(c) = self.book.cell_of_entity(&entity) {
            return c;
        }
        match world.entity(entity).get_components::<RecordQuery<G>>() {
            Ok(item) => self.space.cell_of(&self.game.codec().wire(item)),
            Err(_) => S::Cell::default(),
        }
    }

    /// Assemble `cell`'s packet from this tick's pieces (module docs):
    /// a fresh group gets a FULL packet; an established group gets a
    /// DELTA packet (exits, then cell exits, then updates) — or nothing
    /// (returns `false`) when the whole 3×3 is silent for it. The
    /// assembly itself is the shared engine
    /// ([`crate::common::assemble_group_packet`]); this hook only feeds
    /// it this room's state.
    fn snapshot(
        &mut self,
        _world: &mut World,
        ctx: &TickCtx,
        cell: &S::Cell,
        // Single-room execution: no boundary records exist here.
        _borrowed: &[gsb_core::shard::BorderRecord<()>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        debug_assert_eq!(ctx.tick, self.tick, "update must precede snapshot");
        assemble_group_packet(
            &mut self.pieces,
            &self.book,
            self.game.codec(),
            &self.space,
            cell,
            &mut self.group_full_emitted,
            out,
        )
    }

    /// Keep-alive (on the cadence tick, whether this group emitted this
    /// tick or not): a freshly encoded FULL snapshot of the group's view
    /// (module docs, "Keep-alive"). The cached last payload is a delta —
    /// re-sending it would be meaningless (a client that missed it has no
    /// baseline; a current client would double-apply); the fresh full
    /// heals any client that lost one or more deltas, bounding the
    /// recovery to the keep-alive period whether the group is active or
    /// silent.
    fn keepalive(
        &mut self,
        _world: &mut World,
        _ctx: &TickCtx,
        group: &S::Cell,
        _last: Option<&bytes::Bytes>,
        out: &mut bytes::BytesMut,
    ) -> bool {
        debug_assert_eq!(_ctx.tick, self.tick, "update must precede keepalive");
        self.group_full_emitted.insert(*group);
        let full = self
            .pieces
            .full_view(self.game.codec(), &self.space, &self.book.buckets, group);
        out.extend_from_slice(&full);
        true
    }

    fn encoded_records(&mut self) -> u64 {
        self.pieces.take_encoded()
    }

    /// The per-connection private frame: a one-shot FULL view for a
    /// connection that (re)joined a group or crossed into a new cell's
    /// group (it has no baseline for the new view — a delta has nothing
    /// to apply against; module docs, "Late joiners and group
    /// crossings"), unless the group's own emission this tick was already
    /// a full (fresh group / keep-alive — that frame precedes the private
    /// frame in the same batch and baselines the connection). Otherwise:
    /// the pending input acknowledgment (Section A; a few bytes per
    /// advanced tick, zero otherwise).
    fn private(
        &mut self,
        _world: &mut World,
        player: PlayerId,
        group: &S::Cell,
        responses: &[gsb_core::rpc::RpcReply],
        out: &mut bytes::BytesMut,
    ) -> bool {
        // The room passes the player's current group (re-evaluated every
        // tick in phase 4a) — the previous re-derivation (`conn_cell`:
        // two table lookups per connection per tick) was a measured
        // slice of the idle floor.
        let c = *group;
        let baselined = self.conn_view.get(&player).copied() == Some(c);
        if !baselined {
            if self.group_full_emitted.contains(&c) {
                // The group's own full is in this batch (ahead of
                // this frame): the baseline is established there. The
                // ack (and any queued RPC answers — rare: a request
                // answered on the very tick of a join/crossing) still
                // get their normal frame below.
                self.conn_view.insert(player, c);
            } else {
                // The one-shot private full (one per join/crossing):
                // the frame is the `Private` message (the game's
                // `PRIVATE_OP`) — the pre-encoded WorldSnapshot bytes
                // ride in the `snapshot` oneof (field 2,
                // length-delimited). A queued RPC answer is appended to
                // the SAME frame
                // (field 3, one length-delimited `RpcResponse` each)
                // instead of a second frame — the per-connection
                // per-tick slot is one frame.
                let full =
                    self.pieces
                        .full_view(self.game.codec(), &self.space, &self.book.buckets, &c);
                out.put_u8(0x12); // Private field 2 (snapshot), LEN
                encode_varint(full.len() as u64, out);
                out.extend_from_slice(&full);
                crate::common::append_responses(responses, out);
                self.conn_view.insert(player, c);
                return true;
            }
        }
        crate::common::emit_private(&mut self.input, player, responses, out)
    }

    fn on_join(&mut self, world: &mut World, conn: ConnectionId) -> Admission {
        self.on_join_as(world, conn, "")
    }

    fn on_join_as(&mut self, world: &mut World, conn: ConnectionId, identity: &str) -> Admission {
        // Shared spawn path (input session reset included): deterministic
        // spawn point, fresh stable player + wire identity, player→entity
        // table. `conn_view` deliberately gets NO entry here: the first
        // `private` call (this tick's fan-out, or the next tick's)
        // delivers the one-shot full and records the baseline — via the
        // group's own fresh full when the group is born, or via the
        // private frame otherwise.
        let admission = crate::common::join(
            &mut self.game,
            &mut self.player_entity,
            &mut self.next_player_id,
            &mut self.minter,
            world,
            conn,
            identity,
            &mut self.input,
        );
        // Maintain the member-entity set (the dirty loop's O(1)
        // membership test — it selects which of the changed entities
        // count toward the per-cell member arithmetic).
        if let Some(&entity) = self.player_entity.get(&admission.player) {
            self.book.members.insert(entity);
        }
        admission
    }

    fn on_leave(&mut self, world: &mut World, player: PlayerId) {
        // The core guards stale leaves before calling this; a genuine
        // leave drops the entity, the input session, and the view
        // baseline (a re-join is a new session: fresh input state, fresh
        // one-shot full).
        if let Some(&entity) = self.player_entity.get(&player) {
            self.book.members.remove(&entity);
            // Despawns are NOT component writes: the codec's dirty query
            // in `update` cannot see the entity once it is gone,
            // so the removal must be parked here (module docs, "Dirty
            // cells") — applied against the wire id and the cell the
            // entity occupied at the end of the last `update`
            // (`last_cell`, written in `update` and nowhere else). A
            // join+leave inside one tick parks nothing: the entity never
            // made it into `last_cell` (no `update` ran between the join
            // and the leave), so it never entered the buckets — nothing
            // to remove, no member count to undo.
            if self.book.last_cell.contains_key(&entity) {
                self.book.pending_removals.push((entity, true));
            }
        }
        crate::common::on_leave(&mut self.player_entity, world, player, &mut self.input);
        self.conn_view.remove(&player);
    }

    // -- the disconnect policy (see `crate::room::OpenRoom`, the shared
    //    hook bodies live in `crate::common`) ---------------------------

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
        crate::common::park_resume(&mut self.park_ledger, &mut self.input, identity, player);
        // The resumed SESSION has no view baseline: clearing the entry
        // under the STABLE key makes `private` deliver a fresh one-shot
        // full (a resume is a new session — same contract as a re-join).
        // This is this strategy's one genuinely session-scoped table.
        self.conn_view.remove(&player);
    }

    fn ingest(&mut self, world: &mut World, ctx: &TickCtx, actions: &mut Vec<Action>) {
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

    fn update(&mut self, world: &mut World, ctx: &TickCtx) {
        crate::common::systems(&mut self.game, world, ctx);
        // Orphan stamping (idempotent, mirrors `OpenRoom`): entities with
        // the codec's marker but no `WireId` get the next serial, so the
        // broadcast set is exactly "has the marker". It runs BEFORE the
        // dirty query below: the query requires a `WireId`, and a
        // stamped orphan's record write (by the spawner, outside
        // `on_join`) is already inside this tick's change window — the
        // stamp adds only a `WireId`, so the query then sees the entity
        // exactly once (as new-to-buckets).
        crate::common::stamp_orphans::<Marker<G>>(&mut self.minter, world);
        // Clear the per-tick state (persistent containers, in place —
        // the pieces and the classification are computed lazily in the
        // broadcast phase; `tick` is current from here on).
        self.book.begin_tick();
        self.pieces.begin_tick(ctx.tick);
        self.group_full_emitted.clear();
        self.tick = ctx.tick;

        // The dirty set (module docs, "Dirty cells"): bevy's change
        // detection flags every write the codec's `Dirty` filter names
        // (the demo: every `Position` write) — by any writer, through any
        // API — so no writer can forget to mark a cell dirty: the mark
        // lives in bevy's write path itself. The query window is "writes
        // since the end of the previous `update`" (the kit's ONE
        // `clear_trackers` call, at the bottom of this method, closes
        // THIS tick's window), so CONTROL-phase joins (spawn writes) are
        // inside it; the demo's CONVERT-phase writes touch `MoveTarget`,
        // not `Position`, and reach the query through the systems'
        // resulting `Position` writes. Only the changed entities are visited —
        // per-tick work is proportional to the movers, not to the entity
        // count. (The pass itself — including its quantization no-op and
        // its member arithmetic — is the shared engine,
        // [`crate::common::CellBook::dirty_pass`].)
        self.book.dirty_pass(world, self.game.codec(), &self.space);

        // Leavers: despawns are invisible to the change query — applied
        // from the removals parked in `on_leave` …
        self.book.apply_removals();
        // … and every despawn nobody parked (game code despawning an
        // NPC — §8.2), from the world's removed-component buffers, read
        // before this tick's change-window close empties them.
        self.book.sweep_removed::<Marker<G>>(world);

        // The per-cell flags, the group birth, and the occupancy roll —
        // order-independent, against the final bucket state (single-room
        // execution: every content source of the tick has landed by now,
        // so the roll sits here; the sharded composite defers it until
        // its borrowed strip has been integrated).
        self.book.roll();

        // The tick's ONE change-window close (§4.4: the kit owns it; a
        // game hook never calls it).
        crate::common::close_change_window(world);
    }

    /// The game's request handlers
    /// ([`Game::handle_request`](crate::game::Game::handle_request)), resolved
    /// against this room's player→entity table. Every kit room forwards:
    /// whether a room answers game RPCs does not depend on its
    /// visibility strategy.
    fn handle_request(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        req: &gsb_core::rpc::RpcRequest,
    ) -> Option<gsb_core::rpc::RequestDecision> {
        self.game
            .handle_request(world, ctx, req, &self.player_entity)
    }
}

impl<G: Game, S: CellSpace<Wire<G>>> RoomLogic<World> for AoiRoom<G, S> {}
