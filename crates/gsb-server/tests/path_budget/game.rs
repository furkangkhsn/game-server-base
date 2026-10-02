//! The game behind the door: a kit `OpenRoom` over a swarm of dots that
//! all move every tick (a full snapshot every tick, ~1.5 kB), opted in
//! to `SnapshotBudget` — and reporting every budget its systems read
//! from the tick context.

use std::collections::HashMap;
use std::sync::Arc;

use bevy_ecs::prelude::{Changed, Component, Entity, World};
use bytes::BytesMut;
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::registry::{BuiltRoom, RoomFactory};
use gsb_core::room::{Action, RoomLogic, TickCtx};
use gsb_kit::budget::SnapshotBudget;
use gsb_kit::codec::RecordCodec;
use gsb_kit::game::{Game, InputSeq};
use gsb_kit::room::OpenRoom;
use gsb_protocol::MessageTable;
use gsb_server::{Config, GameModule, RegistryParts, RegistryTask, ServerError};
use tokio::sync::mpsc;

/// The dots besides the players.
const DOTS: i32 = 40;

#[derive(Component, Clone, Copy)]
struct Dot {
    x: i32,
    y: i32,
}

struct DotCodec;

impl RecordCodec for DotCodec {
    type Marker = Dot;
    type Query = &'static Dot;
    type Dirty = Changed<Dot>;
    type Wire = (i32, i32);

    fn wire(&self, d: &Dot) -> (i32, i32) {
        (d.x, d.y)
    }

    /// A fat record (32 bytes): the frame is what the path must carry.
    fn encode(&self, _id: u64, &(x, y): &(i32, i32), out: &mut BytesMut) {
        out.extend_from_slice(&x.to_le_bytes());
        out.extend_from_slice(&y.to_le_bytes());
        out.extend_from_slice(&[0u8; 24]);
    }
}

struct Swarm {
    codec: DotCodec,
    seeded: bool,
    budgets: mpsc::UnboundedSender<usize>,
}

impl Game for Swarm {
    type Codec = DotCodec;
    const SNAPSHOT_OP: u16 = 1950;
    const PRIVATE_OP: u16 = 1951;

    fn codec(&self) -> &DotCodec {
        &self.codec
    }

    fn spawn_player(&mut self, world: &mut World, _conn: ConnectionId) -> Entity {
        world.spawn(Dot { x: 0, y: 0 }).id()
    }

    fn ingest(
        &mut self,
        _world: &mut World,
        _ctx: &TickCtx,
        actions: &mut Vec<Action>,
        _players: &HashMap<PlayerId, Entity>,
        _seq: &mut InputSeq,
    ) {
        actions.clear();
    }

    fn systems(&mut self, world: &mut World, ctx: &TickCtx) {
        if !self.seeded {
            self.seeded = true;
            for x in 0..DOTS {
                world.spawn(Dot { x, y: 0 });
            }
        }
        let mut dots = world.query::<&mut Dot>();
        for mut d in dots.iter_mut(world) {
            d.y += 1;
        }
        // The room's only member: whichever player id it minted.
        for p in 0..4 {
            if let Some(b) = ctx.budget(PlayerId(p)) {
                let _ = self.budgets.send(b);
            }
        }
    }
}

/// The module: one open room of the swarm, with the snapshot budget.
pub struct SwarmModule {
    pub budgets: mpsc::UnboundedSender<usize>,
}

impl GameModule for SwarmModule {
    fn name(&self) -> &'static str {
        "swarm"
    }

    fn configure(&mut self, _raw: &toml::Table, _engine: &Config) -> Result<(), ServerError> {
        Ok(())
    }

    fn register(&self, _table: &mut MessageTable) {}

    fn spawn_registry(&self, parts: RegistryParts) -> RegistryTask {
        let budgets = self.budgets.clone();
        let factory: RoomFactory<World, (), (), ()> = Arc::new(move |_id, _config| {
            let game = Swarm {
                codec: DotCodec,
                seeded: false,
                budgets: budgets.clone(),
            };
            BuiltRoom::Single {
                world: World::new(),
                logic: Box::new(
                    OpenRoom::with_game(game).with_snapshot_budget(SnapshotBudget::new()),
                ) as Box<dyn RoomLogic<World, GroupKey = (), Strip = ()>>,
            }
        });
        parts.spawn(factory)
    }

    fn describe(&self) -> String {
        "swarm: one open room, snapshot budget on".into()
    }
}
