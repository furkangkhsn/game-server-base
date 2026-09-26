//! The composite's game-logic contract: the team group key, the team
//! frames, and the grid protocol's hooks delegated.
//!
//! NOT split further: a trait impl is one block.

use bevy_ecs::prelude::World;
use gsb_core::id::{ConnectionId, EntityId, PlayerId};
use gsb_core::room::{Action, Admission, Detach, GameLogic, ResumeFound, TickCtx};
use gsb_core::rpc::RequestDecision;
use gsb_core::shard::BorderRecord;

use crate::game::{ShardGame, TeamGame, Wire};
use crate::sharded::team::*;
use crate::space::{Partition, Vision};

impl<G, P, V> GameLogic<World> for ShardedTeamRoom<G, P, V>
where
    G: ShardGame + TeamGame,
    P: Partition<Wire<G>>,
    V: Vision,
{
    type GroupKey = Team;
    type Strip = Wire<G>;

    fn snapshot_op(&self) -> u16 {
        G::SNAPSHOT_OP
    }

    fn private_op(&self) -> u16 {
        G::PRIVATE_OP
    }

    /// The player's team (its entity's [`TeamMember`]), as in the team
    /// room; team 0 when it has none.
    fn group_of(&self, world: &World, player: PlayerId) -> Team {
        self.inner
            .player_entity
            .get(&player)
            .and_then(|&e| Self::team_of_entity(world, e))
            .unwrap_or(Team(0))
    }

    /// `team`'s frame from the content the TEAMS phase built this tick
    /// (the borrowed strip is already in it — `team_exchange` received
    /// the same slice).
    fn snapshot(
        &mut self,
        _world: &mut World,
        ctx: &TickCtx,
        team: &Team,
        _borrowed: &[BorderRecord<Wire<G>>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        self.emit_snapshot(ctx.tick, *team, out)
    }

    fn keepalive(
        &mut self,
        _world: &mut World,
        ctx: &TickCtx,
        team: &Team,
        _last: Option<&bytes::Bytes>,
        out: &mut bytes::BytesMut,
    ) -> bool {
        self.emit_keepalive(ctx.tick, *team, out)
    }

    fn encoded_records(&mut self) -> u64 {
        std::mem::take(&mut self.encoded)
    }

    fn on_join(&mut self, world: &mut World, conn: ConnectionId) -> Admission {
        self.on_join_as(world, conn, "")
    }

    /// The team room's join on the grid: the game spawns AND picks the
    /// team from the authenticated identity
    /// ([`TeamGame::spawn_team_player_as`]); the team goes into the world
    /// as the entity's [`TeamMember`].
    fn on_join_as(&mut self, world: &mut World, conn: ConnectionId, identity: &str) -> Admission {
        let mut team = None;
        let admission = self.inner.admit(world, |game, world| {
            let (entity, t) = game.spawn_team_player_as(world, conn, identity);
            team = Some(t);
            entity
        });
        let entity = self.inner.player_entity[&admission.player];
        let team = team.expect("the spawn step ran");
        world.entity_mut(entity).insert(TeamMember(team));
        admission
    }

    fn on_leave(&mut self, world: &mut World, player: PlayerId) {
        self.baselines.forget(player);
        self.inner.on_leave(world, player);
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
        // A resumed session has no view baseline (as after a join).
        self.baselines.forget(player);
    }

    fn private(
        &mut self,
        world: &mut World,
        player: PlayerId,
        group: &Team,
        responses: &[gsb_core::rpc::RpcReply],
        out: &mut bytes::BytesMut,
    ) -> bool {
        self.emit_private(world, player, *group, responses, out)
    }

    /// A dropped batch (F11): the ack and the session payload its
    /// private frame carried are owed again, and when it carried view
    /// content (the group frame, or the one-shot full) the baseline is
    /// taken back — the next frame re-sends a one-shot full, paced
    /// against a storm (`Baselines`).
    fn on_batch_dropped(&mut self, _world: &mut World, player: PlayerId, snapshot: bool) {
        let full = self.inner.input.dropped(player);
        self.baselines.dropped(player, self.step, snapshot || full);
    }

    /// The channel took a batch again (F11): a re-send the storm
    /// pacing held back goes out on the next frame.
    fn on_batch_resumed(&mut self, _world: &mut World, player: PlayerId) {
        self.baselines.resumed(player);
    }

    fn update(&mut self, world: &mut World, ctx: &TickCtx) {
        self.step += 1;
        self.tick = ctx.tick;
        self.inner.step(world, ctx, None);
        crate::common::close_change_window(world);
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
