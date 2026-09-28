//! The lit AOI room's game-logic implementation: a `Cell` group is the
//! AOI room's own (every hook forwarded as is); a `Viewer` group is the
//! lit room's (`view`). The session hooks are the AOI room's, plus the
//! viewer tables' upkeep.
//!
//! NOT split further: a trait impl is one block.

use bevy_ecs::prelude::World;
use gsb_core::id::{ConnectionId, EntityId, PlayerId};
use gsb_core::metrics::LogicCounters;
use gsb_core::room::{
    Action, Admission, Detach, DisconnectCause, ExpireTo, GameLogic, ResumeFound, RoomLogic,
    TickCtx,
};
use gsb_core::rpc::{RequestDecision, RpcReply, RpcRequest};
use gsb_core::shard::BorderRecord;

use crate::aoi::lit::*;

impl<G: LitGame, S: CellSpace<Wire<G>>> GameLogic<World> for LitAoiRoom<G, S> {
    type GroupKey = LitGroup<S::Cell>;
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        self.room.snapshot_op()
    }
    fn private_op(&self) -> u16 {
        self.room.private_op()
    }

    /// A player with a light this tick is its own group; every other
    /// player is in its cell's (the AOI room's `group_of`). The lights
    /// are decided in `update`, which precedes the broadcast phase; a
    /// joiner asked before its first update is in its cell's group until
    /// then (nothing is sent before the broadcast phase re-asks).
    fn group_of(&self, world: &World, player: PlayerId) -> LitGroup<S::Cell> {
        if self.viewers.contains_key(&player) {
            LitGroup::Viewer(player)
        } else {
            LitGroup::Cell(self.room.group_of(world, player))
        }
    }

    fn snapshot(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        group: &LitGroup<S::Cell>,
        borrowed: &[BorderRecord<()>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        match group {
            LitGroup::Cell(c) => self.room.snapshot(world, ctx, c, borrowed, out),
            LitGroup::Viewer(p) => self.viewer_snapshot(*p, ctx.tick, out),
        }
    }

    fn keepalive(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        group: &LitGroup<S::Cell>,
        last: Option<&bytes::Bytes>,
        out: &mut bytes::BytesMut,
    ) -> bool {
        match group {
            LitGroup::Cell(c) => self.room.keepalive(world, ctx, c, last, out),
            LitGroup::Viewer(p) => self.viewer_keepalive(*p, ctx.tick, out),
        }
    }

    fn encoded_records(&mut self) -> u64 {
        self.room.encoded_records() + std::mem::take(&mut self.encoded)
    }

    fn logic_counters(&self, world: &World, out: &mut LogicCounters) {
        self.room.logic_counters(world, out);
    }

    fn private(
        &mut self,
        world: &mut World,
        player: PlayerId,
        group: &LitGroup<S::Cell>,
        responses: &[RpcReply],
        out: &mut bytes::BytesMut,
    ) -> bool {
        match group {
            LitGroup::Cell(c) => self.room.private(world, player, c, responses, out),
            LitGroup::Viewer(_) => self.viewer_private(world, player, responses, out),
        }
    }

    /// A dropped batch (F11): a viewer's baseline of its own view is
    /// taken back (its one-shot full is paced like the AOI room's);
    /// every other player's is the AOI room's.
    fn on_batch_dropped(&mut self, world: &mut World, player: PlayerId, snapshot: bool) {
        if self.viewers.contains_key(&player) {
            let full = self.room.input.dropped(player);
            let step = self.room.book.step;
            self.baselines.dropped(player, step, snapshot || full);
        } else {
            self.room.on_batch_dropped(world, player, snapshot);
        }
    }

    fn on_batch_resumed(&mut self, world: &mut World, player: PlayerId) {
        self.baselines.resumed(player);
        self.room.on_batch_resumed(world, player);
    }

    fn on_join(&mut self, world: &mut World, conn: ConnectionId) -> Admission {
        self.room.on_join(world, conn)
    }

    fn on_join_as(&mut self, world: &mut World, conn: ConnectionId, identity: &str) -> Admission {
        self.room.on_join_as(world, conn, identity)
    }

    fn on_leave(&mut self, world: &mut World, player: PlayerId) {
        self.room.on_leave(world, player);
        self.viewers.remove(&player);
        self.baselines.forget(player);
    }

    fn on_disconnect(&mut self, world: &mut World, player: PlayerId, identity: &str) -> Detach {
        self.room.on_disconnect(world, player, identity)
    }

    fn on_disconnect_with(
        &mut self,
        world: &mut World,
        player: PlayerId,
        identity: &str,
        cause: DisconnectCause,
    ) -> Detach {
        self.room.on_disconnect_with(world, player, identity, cause)
    }

    fn may_release(&mut self, world: &mut World, player: PlayerId) -> bool {
        self.room.may_release(world, player)
    }

    fn on_detach_expired(&mut self, world: &mut World, player: PlayerId, to: ExpireTo) {
        self.room.on_detach_expired(world, player, to);
    }

    fn resume_lookup(&self, world: &World, identity: &str) -> ResumeFound {
        self.room.resume_lookup(world, identity)
    }

    /// A resumed session holds no baseline of either view: the AOI room
    /// forgets its cell baseline, this room its viewer baseline.
    fn on_resume(
        &mut self,
        world: &mut World,
        identity: &str,
        conn: ConnectionId,
        player: PlayerId,
        entity: EntityId,
    ) {
        self.room.on_resume(world, identity, conn, player, entity);
        self.baselines.forget(player);
    }

    fn ingest(&mut self, world: &mut World, ctx: &TickCtx, actions: &mut Vec<Action>) {
        self.room.ingest(world, ctx, actions);
    }

    /// The AOI room's update (systems, bookkeeping, the change-window
    /// close), then the light pass (`view`).
    fn update(&mut self, world: &mut World, ctx: &TickCtx) {
        self.room.update(world, ctx);
        self.light_pass(world);
    }

    fn handle_request(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        req: &RpcRequest,
    ) -> Option<RequestDecision> {
        self.room.handle_request(world, ctx, req)
    }
}

impl<G: LitGame, S: CellSpace<Wire<G>>> RoomLogic<World> for LitAoiRoom<G, S> {}
