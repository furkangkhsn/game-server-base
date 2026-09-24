//! The team room's game-logic implementation: two groups, each seeing
//! its own units plus whatever its vision radius reveals.
//!
//! NOT split further: a trait impl is one block.

use bevy_ecs::prelude::World;
use gsb_core::id::{ConnectionId, EntityId, PlayerId};
use gsb_core::room::{Action, Admission, Detach, GameLogic, ResumeFound, RoomLogic, TickCtx};
use prost::Message;

use crate::kit::seam;
use crate::kit::team::*;

impl GameLogic<World> for TeamRoom {
    type GroupKey = Team;
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        seam::WORLD_SNAPSHOT
    }
    fn private_op(&self) -> u16 {
        seam::PRIVATE
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
    /// docs). Returns `false` when the team's wire content is unchanged
    /// since that team's last emit (per-team ledger; membership and
    /// visibility transitions change the content and flip it).
    fn snapshot(
        &mut self,
        _world: &mut World,
        ctx: &TickCtx,
        team: &Team,
        // Single-room execution: no boundary records exist here.
        _borrowed: &[gsb_core::shard::BorderRecord<()>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        let t = team.0 as usize;
        let content = &self.contents[t];
        if self.last[t] == *content {
            return false;
        }

        let mut snap = seam::WorldSnapshot {
            sequence: ctx.tick,
            entities: Vec::with_capacity(content.len()),
            removed: Vec::new(),
            cell_exits: Vec::new(),
            delta: false,
        };
        for (&wire_id, &(x, y)) in content {
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
        self.last[t] = content.clone();
        true
    }

    fn on_join(&mut self, world: &mut World, conn: ConnectionId) -> Admission {
        let admission = crate::kit::common::on_join(
            &mut self.player_entity,
            &mut self.next_player_id,
            &mut self.minter,
            self.spawn_half,
            world,
            conn,
            &mut self.input,
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
        // Team assignment hashes the TRANSPORT session id (as it always
        // has): the load generator's team distribution pairs with it.
        world
            .entity_mut(entity)
            .insert(TeamMember(seam::team_of(conn)));
        admission
    }

    fn on_leave(&mut self, world: &mut World, player: PlayerId) {
        crate::kit::common::on_leave(&mut self.player_entity, world, player, &mut self.input)
    }

    // -- the disconnect policy (see `crate::kit::room::OpenRoom`, the shared
    //    hook bodies live in `crate::kit::common`) ---------------------------
    //
    // Visibility of a parked hero (RECONNECT §3.2): under team fog a
    // disconnected player's entity keeps its TeamMember component, so it
    // keeps appearing to its OWN team and stays subject to the ordinary
    // vision rule for enemies. A strategy that wanted to HIDE or mark
    // the parked hero instead would filter/annotate its record HERE —
    // in the content rebuild / snapshot encoder below — which is
    // game-band policy, deliberately not implemented in the base.

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
        _group: &Team,
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
        // silently invisible. Done before `rebuild` so freshly-stamped
        // entities are in this tick's content.
        crate::kit::common::stamp_orphans(&mut self.minter, world);

        self.rebuild(world);
    }

    fn encoded_records(&mut self) -> u64 {
        let n = self.encoded;
        self.encoded = 0;
        n
    }
}

impl RoomLogic<World> for TeamRoom {}
