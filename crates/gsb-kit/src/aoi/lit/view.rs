//! The lit room's per-tick light pass and its viewer groups' frames (a
//! child of `lit`: the module docs there are the design).

use bevy_ecs::prelude::World;
use bytes::BytesMut;
use gsb_core::id::PlayerId;
use gsb_core::room::GameLogic;
use gsb_core::rpc::RpcReply;

use crate::aoi::lit::*;
use crate::common::Emitted;

impl<G: LitGame, S: CellSpace<Wire<G>>> LitAoiRoom<G, S> {
    /// After the AOI room's update (the world is the tick's final state,
    /// the buckets its content): ask the game for every player's light,
    /// rebuild each lit viewer's view, and settle the switches (module
    /// docs, "Switching").
    pub(super) fn light_pass(&mut self, world: &World) {
        let Self {
            room,
            viewers,
            baselines,
            shared,
            shared_now,
            ..
        } = self;
        shared_now.clear();
        for (&player, &entity) in &room.player_entity {
            // The player's cell: its AOI group, and its view's centre.
            let cell = room.group_of(world, player);
            let Some(light) = room.game.light(world, entity) else {
                // No light: the cell's shared group. A viewer until now
                // holds only its lit subset — the AOI room owes it a
                // one-shot full of the whole view (and its viewer tables
                // go: they are bounded by the lit viewers).
                if viewers.remove(&player).is_some() {
                    room.baselines.forget(player);
                    baselines.forget(player);
                }
                shared_now.insert(cell);
                continue;
            };
            // A new viewer's group is fresh: its first frame is a full,
            // which establishes the session's baseline of the lit view.
            let viewer = viewers.entry(player).or_default();
            viewer.content.clear();
            for c in room.space.view(cell) {
                let Some(bucket) = room.book.buckets.get(&c) else {
                    continue;
                };
                for (&id, wire) in bucket {
                    // A record whose entity is unknown is unlit (fail
                    // closed — the index covers every bucketed record).
                    let lit = room
                        .book
                        .entity_of(id)
                        .is_some_and(|record| room.game.lit(&light, world, record, wire));
                    if lit {
                        viewer.content.insert(id, wire.clone());
                    }
                }
            }
        }
        // A cell group with a shared member now but none at the previous
        // tick is fresh for the core: its first packet must be a full
        // (the AOI room's own births cover joins and crossings; this
        // covers a viewer switching back into a cell nobody else shares).
        for c in shared_now.iter() {
            if !shared.contains(c) {
                room.book.born_groups.insert(*c);
            }
        }
        std::mem::swap(shared, shared_now);
    }

    /// `GameLogic::snapshot` of viewer `player`'s group: the fresh
    /// group's full, or the delta against what the viewer holds, or
    /// nothing.
    pub(super) fn viewer_snapshot(
        &mut self,
        player: PlayerId,
        tick: u64,
        out: &mut BytesMut,
    ) -> bool {
        let step = self.room.book.step;
        let Some(v) = self.viewers.get_mut(&player) else {
            return false;
        };
        let codec = self.room.game.codec();
        v.ledger
            .emit_delta(codec, step, tick, &v.content, out, &mut self.encoded)
            != Emitted::Silent
    }

    /// `GameLogic::keepalive` of viewer `player`'s group: a fresh full of
    /// its lit view (the ledger re-synced to it).
    pub(super) fn viewer_keepalive(
        &mut self,
        player: PlayerId,
        tick: u64,
        out: &mut BytesMut,
    ) -> bool {
        let step = self.room.book.step;
        let Some(v) = self.viewers.get_mut(&player) else {
            return false;
        };
        let codec = self.room.game.codec();
        let full = v
            .ledger
            .resync(codec, step, tick, &v.content, &mut self.encoded);
        out.extend_from_slice(&full);
        true
    }

    /// `GameLogic::private` of a viewer: a one-shot full of its LIT view
    /// when its session holds no baseline (a resume, a dropped batch) and
    /// its group's own frame this step was not a full; otherwise the
    /// ordinary frame (ack, RPC answers, session payload). Never the
    /// AOI room's full of the whole neighbourhood.
    pub(super) fn viewer_private(
        &mut self,
        world: &World,
        player: PlayerId,
        responses: &[RpcReply],
        out: &mut BytesMut,
    ) -> bool {
        let step = self.room.book.step;
        if let Some(v) = self.viewers.get_mut(&player) {
            let group_full = v.ledger.full_sent(step);
            if self.baselines.owed(player, (), step, || group_full) {
                let codec = self.room.game.codec();
                let full =
                    v.ledger
                        .resync(codec, step, self.room.tick, &v.content, &mut self.encoded);
                return crate::common::emit_private_full(
                    &mut self.room.game,
                    world,
                    &self.room.player_entity,
                    &mut self.room.input,
                    player,
                    &full,
                    responses,
                    out,
                );
            }
        }
        crate::common::emit_private_frame(
            &mut self.room.game,
            world,
            &self.room.player_entity,
            &mut self.room.input,
            player,
            responses,
            out,
        )
    }
}
