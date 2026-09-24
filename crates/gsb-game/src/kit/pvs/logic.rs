//! The sector room's game-logic implementation: visibility is the
//! hand-authored sector table, so the group key is a sector.
//!
//! NOT split further: a trait impl is one block.

use std::collections::HashMap;

use bevy_ecs::prelude::World;
use gsb_core::id::{ConnectionId, EntityId, PlayerId};
use gsb_core::room::{Action, Admission, Detach, GameLogic, ResumeFound, RoomLogic, TickCtx};
use prost::Message;

use crate::kit::identity::WireId;
use crate::kit::pvs::*;
use crate::kit::seam;
use crate::kit::seam::Position;

impl GameLogic<World> for SectorRoom {
    type GroupKey = Sector;
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        seam::WORLD_SNAPSHOT
    }
    fn private_op(&self) -> u16 {
        seam::PRIVATE
    }

    /// The connection's group is the sector its entity is in (re-evaluated
    /// every tick by the room — a crossing player changes sector and thus
    /// group, and starts receiving the new sector's snapshot).
    fn group_of(&self, world: &World, player: PlayerId) -> Sector {
        let Some(&entity) = self.player_entity.get(&player) else {
            return Sector(SECTOR_OUT);
        };
        let pos = world
            .entity(entity)
            .get::<Position>()
            .copied()
            .unwrap_or_default();
        sector_of(pos)
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
        sector: &Sector,
        // Single-room execution: no boundary records exist here.
        _borrowed: &[gsb_core::shard::BorderRecord<()>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        let mut content: HashMap<u64, (i32, i32)> = HashMap::new();
        let mask = VISIBLE_FROM[sector.0 as usize];
        for s in 0..VISIBLE_FROM.len() as u8 {
            if mask & (1 << s) != 0
                && let Some(bucket) = self.buckets.get(&Sector(s))
            {
                for &(wire_id, x, y) in bucket {
                    content.insert(wire_id, (x, y));
                }
            }
        }

        if let Some(prev) = self.last.get(sector)
            && *prev == content
        {
            return false;
        }

        let mut snap = seam::WorldSnapshot {
            sequence: ctx.tick,
            entities: Vec::with_capacity(content.len()),
            removed: Vec::new(),
            cell_exits: Vec::new(),
            delta: false,
        };
        for (&wire_id, &(x, y)) in &content {
            snap.entities.push(seam::EntityRecord {
                entity: wire_id,
                x,
                y,
            });
        }
        // In-memory encode cannot fail; treat a failure as a bug.
        snap.encode(out)
            .expect("protobuf encode into an in-memory buffer failed");

        self.encoded += content.len() as u64;
        self.last.insert(*sector, content);
        true
    }

    fn on_join(&mut self, world: &mut World, conn: ConnectionId) -> Admission {
        crate::kit::common::on_join(
            &mut self.player_entity,
            &mut self.next_player_id,
            &mut self.minter,
            self.spawn_half,
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
        seam::synthesize_bot_moves(
            self.park_ledger
                .values()
                .filter(|e| e.bot)
                .map(|e| (e.player, e.entity)),
            world,
            ctx,
            actions,
        );
        seam::ingest(&self.player_entity, world, actions, &mut self.input)
    }

    /// The per-connection input acknowledgment (see `OpenRoom::private`).
    fn private(
        &mut self,
        _world: &mut World,
        player: PlayerId,
        _group: &Sector,
        responses: &[gsb_core::rpc::RpcReply],
        out: &mut bytes::BytesMut,
    ) -> bool {
        crate::kit::common::emit_private(&mut self.input, player, responses, out)
    }

    fn update(&mut self, world: &mut World, ctx: &TickCtx) {
        crate::kit::common::run_systems(&mut self.runner, world, ctx);

        // Orphan stamping (idempotent, mirrors the other rooms): entities
        // with a `Position` but no `WireId` get the next serial, so the
        // broadcast set is exactly "has a `Position`" — structural, never
        // silently invisible. Done here (before the bucket build) so
        // freshly-stamped entities are in the buckets the broadcast phase
        // reads.
        crate::kit::common::stamp_orphans(&mut self.minter, world);

        // Bucket the world by sector, once per tick (each entity exactly
        // once); a sector's snapshot is the union of the buckets its
        // visibility table entry names.
        self.buckets.clear();
        let mut query = world.query::<(&WireId, &Position)>();
        for (wire_id, pos) in query.iter(world) {
            let s = sector_of(*pos);
            self.buckets
                .entry(s)
                .or_default()
                .push((wire_id.get(), pos.x as i32, pos.y as i32));
        }
    }

    fn encoded_records(&mut self) -> u64 {
        let n = self.encoded;
        self.encoded = 0;
        n
    }
}

impl RoomLogic<World> for SectorRoom {}
