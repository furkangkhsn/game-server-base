//! The demo's [`Game`] (KIT-ARCHITECTURE §4.3): the hooks the kit's
//! rooms call — spawn, input, the bot's wander, the movement system, the
//! RPC handlers — over the demo's own state (the system stack, the spawn
//! map size, the economy service handle).

use std::collections::HashMap;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, TickCtx};
use gsb_core::rpc::{RequestDecision, RpcRequest};
use gsb_ecs::SystemRunner;

use crate::demo::codec::DemoCodec;
use crate::demo::economy::EconomyService;
use crate::demo::{bot, input, op, rpc, spawn, systems};
use crate::kit::common::{InputSeq, run_systems};
use crate::kit::game::{Game, TeamGame};
use crate::kit::team::Team;

/// The demo game: one moving entity per player on a square 2D map, free
/// movement toward the latest `MOVE_TO` target, two request kinds
/// (`ABILITY`, `ECONOMY`), and a wandering bot for parked players.
pub struct DemoGame {
    /// The demo's system stack (movement).
    runner: SystemRunner,
    /// Half-size of the square spawn map (see [`spawn::spawn_pos`]):
    /// entities spawn uniformly in `[-half, half]²`. The default
    /// ([`spawn::DEFAULT_SPAWN_HALF`]) keeps the historical 100×100
    /// arena; a load profile's "wide map" is just a larger value.
    spawn_half: f32,
    /// The economy service handle (the RPC pattern's external-I/O half);
    /// `None` = `ECONOMY` requests get a normal "not configured"
    /// rejection.
    economy: Option<EconomyService>,
    /// The record codec (zero-sized).
    codec: DemoCodec,
}

impl DemoGame {
    /// The demo over a square spawn map of half-size `spawn_half`
    /// (clamped to at least 1), without an economy service.
    #[must_use]
    pub fn new(spawn_half: f32) -> Self {
        Self {
            runner: systems::movement_runner(),
            spawn_half: spawn_half.max(1.0),
            economy: None,
            codec: DemoCodec,
        }
    }

    /// Attach the economy service handle: `ECONOMY` requests are
    /// delegated to it and answered on a later tick.
    pub fn set_economy(&mut self, economy: EconomyService) {
        self.economy = Some(economy);
    }
}

impl Default for DemoGame {
    fn default() -> Self {
        Self::new(spawn::DEFAULT_SPAWN_HALF)
    }
}

impl Game for DemoGame {
    type Codec = DemoCodec;

    const SNAPSHOT_OP: u16 = op::WORLD_SNAPSHOT;
    const PRIVATE_OP: u16 = op::PRIVATE;

    fn codec(&self) -> &DemoCodec {
        &self.codec
    }

    /// The deterministic spawn point on this game's spawn map, derived
    /// from the TRANSPORT session id (as it always was — the load
    /// generator's home distribution pairs with it), and the player
    /// bundle (position + default speed).
    fn spawn_player(&mut self, world: &mut World, conn: ConnectionId) -> Entity {
        world
            .spawn(spawn::player_bundle(conn, self.spawn_half))
            .id()
    }

    fn bot_actions(
        &mut self,
        world: &World,
        ctx: &TickCtx,
        bots: impl Iterator<Item = (PlayerId, Entity)>,
        out: &mut Vec<Action>,
    ) {
        bot::synthesize_bot_moves(bots, world, ctx, out);
    }

    fn ingest(
        &mut self,
        world: &mut World,
        _ctx: &TickCtx,
        actions: &mut Vec<Action>,
        players: &HashMap<PlayerId, Entity>,
        seq: &mut InputSeq,
    ) {
        input::ingest(players, world, actions, seq);
    }

    fn systems(&mut self, world: &mut World, ctx: &TickCtx) {
        run_systems(&mut self.runner, world, ctx);
    }

    fn handle_request(
        &mut self,
        world: &mut World,
        _ctx: &TickCtx,
        req: &RpcRequest,
        players: &HashMap<PlayerId, Entity>,
    ) -> Option<RequestDecision> {
        rpc::handle_request(players, self.economy.as_ref(), world, req)
    }
}

/// The demo's team policy: conn parity (see [`spawn::team_of`]).
impl TeamGame for DemoGame {
    fn team_of(&mut self, _world: &World, conn: ConnectionId, _entity: Entity) -> Team {
        spawn::team_of(conn)
    }
}
