//! The team room's game-logic implementation: one group per team, each
//! seeing its own units plus whatever its vision reveals.
//!
//! NOT split further: a trait impl is one block.

use bevy_ecs::prelude::World;
use gsb_core::id::{ConnectionId, EntityId, PlayerId};
use gsb_core::room::{Action, Admission, Detach, GameLogic, ResumeFound, RoomLogic, TickCtx};

use crate::game::TeamGame;
use crate::space::Vision;
use crate::team::*;

impl<G: TeamGame, V: Vision> GameLogic<World> for TeamRoom<G, V> {
    type GroupKey = Team;
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        G::SNAPSHOT_OP
    }
    fn private_op(&self) -> u16 {
        G::PRIVATE_OP
    }

    /// The connection's group is its team — game state kept in the world
    /// (the entity's [`TeamMember`] component). This room's `group_of`
    /// reads the world, just like `AoiRoom`'s reads `Position`; the
    /// difference is *what* it reads (a membership component, not a
    /// position), which is what keeps the group key non-spatial. A
    /// connection in the room always has an entity with a `TeamMember`
    /// (written in `on_join`, removed with the entity on leave); the
    /// `unwrap_or` fallback only keeps the function total for bookkeeping
    /// edges (e.g. a conn evicted between `members` and the call).
    fn group_of(&self, world: &World, player: PlayerId) -> Team {
        let Some(&entity) = self.player_entity.get(&player) else {
            return Team(0);
        };
        world
            .entity(entity)
            .get::<TeamMember>()
            .map(|m| m.0)
            .unwrap_or(Team(0))
    }

    /// Encode `team`'s snapshot from this tick's content cache (module
    /// docs; the two snapshot modes: `frames`). Returns `false` when the
    /// team's wire content is unchanged since that team's last emit
    /// (per-team ledger; membership and visibility transitions change
    /// the content and flip it).
    fn snapshot(
        &mut self,
        _world: &mut World,
        ctx: &TickCtx,
        team: &Team,
        // Single-room execution: no boundary records exist here.
        _borrowed: &[gsb_core::shard::BorderRecord<()>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        self.emit_snapshot(ctx.tick, *team, out)
    }

    /// Delta mode: a fresh FULL of the team's view on the keep-alive
    /// cadence (the convergence guarantee); full mode: the core's
    /// default re-send (`frames`).
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

    fn on_join(&mut self, world: &mut World, conn: ConnectionId) -> Admission {
        // Team assignment is game policy, decided in the spawn step
        // (`TeamGame::spawn_team_player` — by default `spawn_player`
        // then `team_of`; the demo hashes the TRANSPORT session id, as it
        // always has: the load generator's team distribution pairs with
        // it; an arena overrides it to spawn at the team's base).
        let mut team = None;
        let admission = crate::common::join_with(
            &mut self.game,
            &mut self.player_entity,
            &mut self.next_player_id,
            &mut self.minter,
            world,
            conn,
            &mut self.input,
            |game, world, conn| {
                let (entity, t) = game.spawn_team_player(world, conn);
                team = Some(t);
                entity
            },
        );
        // Team membership goes into the WORLD (the component), not just
        // this room's bookkeeping: `group_of` and `rebuild` read it from
        // world state, so a runtime team change is a plain component
        // write — no room hook, no protocol op, no bookkeeping to keep in
        // sync.
        let entity = self
            .player_entity
            .get(&admission.player)
            .copied()
            .expect("inserted above");
        let team = team.expect("the spawn step ran");
        world.entity_mut(entity).insert(TeamMember(team));
        admission
    }

    fn on_leave(&mut self, world: &mut World, player: PlayerId) {
        // A re-join is a new session: no view baseline survives it.
        self.baselines.forget(player);
        crate::common::on_leave(&mut self.player_entity, world, player, &mut self.input)
    }

    // -- the disconnect policy (see `crate::room::OpenRoom`, the shared
    //    hook bodies live in `crate::common`) ---------------------------
    //
    // Visibility of a parked hero (RECONNECT §3.2): under team fog a
    // disconnected player's entity keeps its TeamMember component, so it
    // keeps appearing to its OWN team and stays subject to the ordinary
    // vision rule for enemies. A strategy that wanted to HIDE or mark
    // the parked hero instead would filter/annotate its record HERE —
    // in the content rebuild / snapshot encoder below — which is
    // game-band policy, deliberately not implemented in the base.

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
        // Faz 2 shrink: ledger consume + seq/ack reset only.
        crate::common::park_resume(&mut self.park_ledger, &mut self.input, identity, player);
        // Delta mode: the resumed SESSION has no view baseline — the next
        // private frame carries a one-shot full (as after a join).
        self.baselines.forget(player);
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

    /// The per-connection frame: the input acknowledgment (see
    /// `OpenRoom::private`) — and, in delta mode, the one-shot full for a
    /// member without a baseline (`frames`).
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

    fn update(&mut self, world: &mut World, ctx: &TickCtx) {
        // One step: the delta ledgers' "previous step" and the tick the
        // `private` seam stamps into one-shot fulls.
        self.step += 1;
        self.tick = ctx.tick;
        crate::common::systems(&mut self.game, world, ctx);

        // Orphan stamping (idempotent, mirrors the other rooms): entities
        // with the codec's marker but no `WireId` get the next serial, so
        // the broadcast set is exactly "has the marker" — structural,
        // never silently invisible. Done before `rebuild` so
        // freshly-stamped entities are in this tick's content.
        crate::common::stamp_orphans::<Marker<G>>(&mut self.minter, world);

        self.rebuild(world);

        // The tick's ONE change-window close (§4.4: the kit owns it; a
        // game hook never calls it). This room reads no change filter
        // itself, but the window is world-wide: without the close the
        // removed-component buffers grow with every despawn for the
        // room's lifetime (§8.3).
        crate::common::close_change_window(world);
    }

    fn encoded_records(&mut self) -> u64 {
        let n = self.encoded;
        self.encoded = 0;
        n
    }

    /// The game's request handlers
    /// ([`Game::handle_request`]), resolved
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

impl<G: TeamGame, V: Vision> RoomLogic<World> for TeamRoom<G, V> {}
