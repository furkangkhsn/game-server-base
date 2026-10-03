//! The game server's side: the game reads the VERIFIED claims at join.
//!
//! The game is the 2D demo with one change: a player spawns where its
//! class stands (mages west, warriors east), and the class comes from the
//! ticket the lobby signed — `Game::spawn_player_verified`, not anything
//! the client said. Its stateless ticket check ([`check`]) refuses a class
//! the game does not know, under the game's own reason name.

use std::collections::HashMap;
use std::sync::Arc;

use bevy_ecs::prelude::{Component, Entity, World};
use gsb_core::auth::{GameReason, Joiner};
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::registry::{BuiltRoom, RoomFactory};
use gsb_core::room::{Action, RoomLogic, TickCtx};
use gsb_core::rpc::{RequestDecision, RpcRequest};
use gsb_demo::DemoGame;
use gsb_demo::components::Position;
use gsb_kit::game::{Game, InputSeq};
use gsb_kit::room::OpenRoom;
use gsb_server::{Config, GameModule, RegistryParts, RegistryTask, ServerError};
use gsb_ticket::Claims;
use serde::{Deserialize, Serialize};

/// The game's own claims, as the lobby signs them into the ticket's `ext`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Loadout {
    /// The saved character the player plays.
    pub character: u64,
    /// Its class.
    pub class: String,
}

/// The classes the game knows, and where each spawns (world x).
pub const CLASSES: [(&str, f32); 2] = [("mage", -30.0), ("warrior", 30.0)];

/// The game's refusal of a class it does not know.
pub const UNKNOWN_CLASS: GameReason = GameReason::new("unknown_class");

/// The game's stateless ticket check: run by the validator after the
/// signature and the standard claims, off the tick.
pub fn check(claims: &Claims<Loadout>) -> Result<(), GameReason> {
    match CLASSES.iter().any(|(c, _)| *c == claims.game.class) {
        true => Ok(()),
        false => Err(UNKNOWN_CLASS),
    }
}

/// The character a player's entity was spawned as (from the ticket).
#[derive(Debug, Clone, PartialEq, Eq, Component)]
pub struct Character {
    pub id: u64,
    pub class: String,
}

/// The demo, spawning by the verified loadout.
#[derive(Default)]
pub struct ClassGame(DemoGame);

impl Game for ClassGame {
    type Codec = <DemoGame as Game>::Codec;
    const SNAPSHOT_OP: u16 = DemoGame::SNAPSHOT_OP;
    const PRIVATE_OP: u16 = DemoGame::PRIVATE_OP;

    fn codec(&self) -> &Self::Codec {
        self.0.codec()
    }

    fn spawn_player(&mut self, world: &mut World, conn: ConnectionId) -> Entity {
        self.0.spawn_player(world, conn)
    }

    /// The claims are the issuer's signed bytes: decode, and spawn the
    /// character where its class stands. A join without claims (local
    /// auth) spawns like the demo.
    fn spawn_player_verified(
        &mut self,
        world: &mut World,
        conn: ConnectionId,
        joiner: &Joiner<'_>,
    ) -> Entity {
        let entity = self.0.spawn_player(world, conn);
        let loadout = joiner
            .claims
            .and_then(|c| serde_json::from_slice::<Loadout>(c).ok());
        if let Some(l) = loadout {
            let x = CLASSES
                .iter()
                .find(|(c, _)| *c == l.class)
                .map_or(0.0, |c| c.1);
            let character = Character {
                id: l.character,
                class: l.class,
            };
            world
                .entity_mut(entity)
                .insert((Position { x, y: 0.0 }, character));
        }
        entity
    }

    fn bot_actions(
        &mut self,
        world: &World,
        ctx: &TickCtx,
        bots: impl Iterator<Item = (PlayerId, Entity)>,
        out: &mut Vec<Action>,
    ) {
        self.0.bot_actions(world, ctx, bots, out);
    }

    fn ingest(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        actions: &mut Vec<Action>,
        players: &HashMap<PlayerId, Entity>,
        seq: &mut InputSeq,
    ) {
        self.0.ingest(world, ctx, actions, players, seq);
    }

    fn systems(&mut self, world: &mut World, ctx: &TickCtx) {
        self.0.systems(world, ctx);
    }

    fn handle_request(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        req: &RpcRequest,
        players: &HashMap<PlayerId, Entity>,
    ) -> Option<RequestDecision> {
        self.0.handle_request(world, ctx, req, players)
    }
}

/// The module the server hosts: one open room of [`ClassGame`].
pub struct ClassModule;

impl GameModule for ClassModule {
    fn name(&self) -> &'static str {
        "lobby-example"
    }

    fn configure(&mut self, _raw: &toml::Table, _engine: &Config) -> Result<(), ServerError> {
        Ok(())
    }

    fn register(&self, table: &mut gsb_protocol::MessageTable) {
        gsb_demo::register(table);
    }

    fn spawn_registry(&self, parts: RegistryParts) -> RegistryTask {
        let factory: RoomFactory<World, (), (), ()> = Arc::new(|_id, _cfg| BuiltRoom::Single {
            world: World::new(),
            logic: Box::new(OpenRoom::with_game(ClassGame::default()))
                as Box<dyn RoomLogic<World, GroupKey = (), Strip = ()>>,
        });
        parts.spawn(factory)
    }

    fn describe(&self) -> String {
        "one open room; players spawn by the class their ticket carries".into()
    }

    fn owned_keys(&self) -> Vec<&'static str> {
        Vec::new()
    }
}
