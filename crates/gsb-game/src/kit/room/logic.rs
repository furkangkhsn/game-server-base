//! The open room's game-logic implementation: everyone sees the whole
//! world, so the snapshot is one group and the seams are the plain
//! ones.
//!
//! NOT split further: a trait impl is one block.

use bevy_ecs::prelude::{With, World};
use gsb_core::id::{ConnectionId, EntityId, PlayerId};
use gsb_core::room::{Action, Admission, Detach, GameLogic, ResumeFound, RoomLogic, TickCtx};
use gsb_core::rpc::RequestDecision;

use crate::kit::codec::RecordCodec;
use crate::kit::common::{put_entity_records, write_full_header};
use crate::kit::game::{Game, Wire};
use crate::kit::identity::WireId;
use crate::kit::room::*;

/// The game's broadcast marker (the codec's `Marker`).
type Marker<G> = <<G as Game>::Codec as RecordCodec>::Marker;
/// The game's record query (the codec's `Query`).
type RecordQuery<G> = <<G as Game>::Codec as RecordCodec>::Query;

impl<G: Game> OpenRoom<G> {
    /// Every broadcastable entity's `(wire id, wire value)`, in query
    /// order.
    fn collect_records(&self, world: &mut World) -> Vec<(u64, Wire<G>)> {
        let codec = self.game.codec();
        let mut query = world.query_filtered::<(&WireId, RecordQuery<G>), With<Marker<G>>>();
        query
            .iter(world)
            .map(|(wire_id, item)| (wire_id.get(), codec.wire(item)))
            .collect()
    }
}

impl<G: Game> GameLogic<World> for OpenRoom<G> {
    // One group per room: everyone sees the whole world. (The interface
    // supports finer groupings, e.g. `GroupKey = ConnectionId` — but then
    // the `last` ledger above must be keyed by group; see the module
    // docs and `RoomLogic::snapshot`.)
    type GroupKey = ();
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        G::SNAPSHOT_OP
    }
    fn private_op(&self) -> u16 {
        G::PRIVATE_OP
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
        // Identity assignment (module docs, "Wire identity"): entities
        // with the marker but no `WireId` — spawned outside `on_join`
        // (bullets, NPCs, traps, …) — are stamped with the next serial
        // here, so the broadcast set is exactly "has the marker" and no
        // entity can be silently invisible (see `common::stamp_orphans`
        // for the two-pass pattern and idempotence). The record query
        // below runs *after* the stamps, so it sees every broadcastable
        // entity exactly once (stamped and pre-stamped alike).
        crate::kit::common::stamp_orphans::<Marker<G>>(&mut self.minter, world);
        // The broadcastable state (wire id, wire value).
        let current = self.collect_records(world);

        // "No change" = identical wire content: the same set of entities
        // with the same wire values. A membership change (join/leave) or
        // any record change flips it. The comparison is on exactly what
        // the snapshot carries (see module docs).
        let changed = self.last.len() != current.len()
            || current
                .iter()
                .any(|(entity, wire)| self.last.get(entity).is_none_or(|last| last != wire));
        if !changed {
            return false;
        }

        // The FULL envelope (header + one record per entity, in query
        // order), byte-identical to the typed `WorldSnapshot` encoding.
        write_full_header(out, ctx.tick);
        put_entity_records(
            self.game.codec(),
            current.iter().map(|(id, wire)| (*id, wire)),
            out,
        );

        self.encoded += current.len() as u64;
        self.last.clear();
        self.last.extend(current);
        true
    }

    fn encoded_records(&mut self) -> u64 {
        let n = self.encoded;
        self.encoded = 0;
        n
    }

    fn on_join(&mut self, world: &mut World, conn: ConnectionId) -> Admission {
        // Shared join path (`common::join`): the game's spawn, a fresh
        // stable player identity + wire identity through their minting
        // counters, and the player→entity table update. The entity value
        // is also returned to the joiner in `JOIN_ROOM_RESULT`, so both
        // paths share one space. No spawn event: membership is expressed
        // by presence in the next snapshot, which now includes the new
        // entity (the join happened in the control phase, before this
        // tick's broadcast).
        crate::kit::common::join(
            &mut self.game,
            &mut self.player_entity,
            &mut self.next_player_id,
            &mut self.minter,
            world,
            conn,
            &mut self.input,
        )
    }

    fn on_leave(&mut self, world: &mut World, player: PlayerId) {
        crate::kit::common::on_leave(&mut self.player_entity, world, player, &mut self.input)
    }

    // -- the disconnect policy (docs/RECONNECT.md §3/§5/§9; the hook
    //    bodies are shared with every demo room — see `crate::kit::common`) --

    fn on_disconnect(&mut self, _world: &mut World, player: PlayerId, identity: &str) -> Detach {
        crate::kit::common::park_on_disconnect(
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
        crate::kit::common::park_on_expire(&mut self.park_ledger, player, to);
    }

    fn resume_lookup(&self, world: &World, identity: &str) -> ResumeFound {
        crate::kit::common::park_lookup(world, &self.park_ledger, identity)
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
        crate::kit::common::park_resume(&mut self.park_ledger, &mut self.input, identity, player);
    }

    fn ingest(&mut self, world: &mut World, ctx: &TickCtx, actions: &mut Vec<Action>) {
        // The bot's synthesized frames ride the SAME list as wire input
        // (RECONNECT §9: "bot = bağlantısız girdi kaynağı" — an input
        // source without a connection): one decode/sequence/move path for
        // both.
        crate::kit::common::ingest(
            &mut self.game,
            world,
            ctx,
            actions,
            &self.player_entity,
            &self.park_ledger,
            &mut self.input,
        )
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
        crate::kit::common::emit_private(&mut self.input, player, responses, out)
    }

    fn update(&mut self, world: &mut World, ctx: &TickCtx) {
        crate::kit::common::systems(&mut self.game, world, ctx);
        // The tick's ONE change-window close (§4.4: the kit owns it; a
        // game hook never calls it). This room reads no change filter
        // itself, but the window is world-wide: without the close the
        // removed-component buffers grow with every despawn for the
        // room's lifetime (§8.3).
        crate::kit::common::close_change_window(world);
    }

    /// The game's request handlers ([`Game::handle_request`] — the demo:
    /// `ABILITY`, answered in this tick; `ECONOMY`, delegated to the
    /// economy service and answered on a later tick), resolved against
    /// this room's player→entity table.
    fn handle_request(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        req: &gsb_core::rpc::RpcRequest,
    ) -> Option<RequestDecision> {
        self.game
            .handle_request(world, ctx, req, &self.player_entity)
    }

    /// The match result (the control plane's result seam, feature A):
    /// the room's FINAL snapshot at shutdown — the complete,
    /// self-contained state the game considers "the result" (who was in
    /// the room, where they ended up), ordered by wire id. Encoded as the
    /// ordinary `WorldSnapshot` envelope (the platform's adapter decodes
    /// it against the same schema it uses for live snapshots).
    fn match_result(&mut self, world: &mut World) -> Option<bytes::Bytes> {
        let mut records = self.collect_records(world);
        records.sort_by_key(|(id, _)| *id);
        let mut out = bytes::BytesMut::new();
        // The shutdown snapshot has no live ticker: sequence 0 marks
        // "terminal" (live snapshots are strictly positive ticks; a 0
        // sequence is omitted on the wire, as proto3 does).
        write_full_header(&mut out, 0);
        put_entity_records(
            self.game.codec(),
            records.iter().map(|(id, wire)| (*id, wire)),
            &mut out,
        );
        Some(out.freeze())
    }
}

// Faz 3 trait promotion: `handle_request` / `match_result` moved onto the
// shared `GameLogic` supertrait above; this impl remains the compile-time
// marker that OpenRoom targets the single-room actor.
impl<G: Game> RoomLogic<World> for OpenRoom<G> {}
