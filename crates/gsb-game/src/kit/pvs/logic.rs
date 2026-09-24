//! The sector room's game-logic implementation: visibility is the
//! hand-authored sector table, so the group key is a sector.
//!
//! NOT split further: a trait impl is one block.

use std::collections::HashMap;

use bevy_ecs::prelude::{With, World};
use gsb_core::id::{ConnectionId, EntityId, PlayerId};
use gsb_core::room::{Action, Admission, Detach, GameLogic, ResumeFound, RoomLogic, TickCtx};

use crate::kit::codec::RecordCodec;
use crate::kit::common::{put_entity_records, write_full_header};
use crate::kit::game::Game;
use crate::kit::identity::WireId;
use crate::kit::pvs::*;
use crate::kit::space::SectorMap;

/// The game's broadcast marker (the codec's `Marker`).
type Marker<G> = <<G as Game>::Codec as RecordCodec>::Marker;
/// The game's record query (the codec's `Query`).
type RecordQuery<G> = <<G as Game>::Codec as RecordCodec>::Query;

impl<G: Game, M: SectorMap> GameLogic<World> for SectorRoom<G, M> {
    type GroupKey = M::Sector;
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        G::SNAPSHOT_OP
    }
    fn private_op(&self) -> u16 {
        G::PRIVATE_OP
    }

    /// The connection's group is the sector its entity is in (re-evaluated
    /// every tick by the room — a crossing player changes sector and thus
    /// group, and starts receiving the new sector's snapshot). What the
    /// room cannot place — a player without an entity, an entity without
    /// the map's position — is grouped in the map's containment sector.
    fn group_of(&self, world: &World, player: PlayerId) -> M::Sector {
        let Some(&entity) = self.player_entity.get(&player) else {
            return self.map.outside();
        };
        match world.entity(entity).get::<M::Pos>() {
            Some(pos) => self.map.sector_of(pos),
            None => self.map.outside(),
        }
    }

    /// Encode `sector`'s snapshot: the union of the buckets of every sector
    /// the static table says is visible from it (module docs). Returns
    /// `false` when that content is unchanged since this sector's last
    /// emit (per-sector ledger; membership and boundary crossings change
    /// the content).
    fn snapshot(
        &mut self,
        _world: &mut World,
        ctx: &TickCtx,
        sector: &M::Sector,
        // Single-room execution: no boundary records exist here.
        _borrowed: &[gsb_core::shard::BorderRecord<()>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        let mut content: HashMap<u64, Wire<G>> = HashMap::new();
        for s in self.map.visible_from(*sector) {
            if let Some(bucket) = self.buckets.get(&s) {
                for (wire_id, wire) in bucket {
                    content.insert(*wire_id, wire.clone());
                }
            }
        }

        if let Some(prev) = self.last.get(sector)
            && *prev == content
        {
            return false;
        }

        // The FULL envelope (header + one record per entity, in content
        // order), byte-identical to the typed `WorldSnapshot` encoding.
        write_full_header(out, ctx.tick);
        put_entity_records(
            self.game.codec(),
            content.iter().map(|(id, wire)| (*id, wire)),
            out,
        );

        self.encoded += content.len() as u64;
        self.last.insert(*sector, content);
        true
    }

    fn on_join(&mut self, world: &mut World, conn: ConnectionId) -> Admission {
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

    // -- the disconnect policy (see `crate::kit::room::OpenRoom`, the shared
    //    hook bodies live in `crate::kit::common`) ---------------------------

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
        // Faz 2 shrink: ledger consume + seq/ack reset only.
        crate::kit::common::park_resume(&mut self.park_ledger, &mut self.input, identity, player);
    }

    fn ingest(&mut self, world: &mut World, ctx: &TickCtx, actions: &mut Vec<Action>) {
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

    /// The per-connection input acknowledgment (see `OpenRoom::private`).
    fn private(
        &mut self,
        _world: &mut World,
        player: PlayerId,
        _group: &M::Sector,
        responses: &[gsb_core::rpc::RpcReply],
        out: &mut bytes::BytesMut,
    ) -> bool {
        crate::kit::common::emit_private(&mut self.input, player, responses, out)
    }

    fn update(&mut self, world: &mut World, ctx: &TickCtx) {
        crate::kit::common::systems(&mut self.game, world, ctx);

        // Orphan stamping (idempotent, mirrors the other rooms): entities
        // with the codec's marker but no `WireId` get the next serial, so
        // the broadcast set is exactly "has the marker" — structural,
        // never silently invisible. Done here (before the bucket build)
        // so freshly-stamped entities are in the buckets the broadcast
        // phase reads.
        crate::kit::common::stamp_orphans::<Marker<G>>(&mut self.minter, world);

        // Bucket the world by sector, once per tick (each entity exactly
        // once); a sector's snapshot is the union of the buckets its
        // visibility entry names. An entity without the map's position
        // lands in the containment sector.
        self.buckets.clear();
        let codec = self.game.codec();
        let mut query =
            world.query_filtered::<(&WireId, RecordQuery<G>, Option<&M::Pos>), With<Marker<G>>>();
        for (wire_id, item, pos) in query.iter(world) {
            let s = match pos {
                Some(pos) => self.map.sector_of(pos),
                None => self.map.outside(),
            };
            self.buckets
                .entry(s)
                .or_default()
                .push((wire_id.get(), codec.wire(item)));
        }

        // The tick's ONE change-window close (§4.4, §8.3 — see
        // `TeamRoom::update`).
        crate::kit::common::close_change_window(world);
    }

    fn encoded_records(&mut self) -> u64 {
        let n = self.encoded;
        self.encoded = 0;
        n
    }
}

impl<G: Game, M: SectorMap> RoomLogic<World> for SectorRoom<G, M> {}
