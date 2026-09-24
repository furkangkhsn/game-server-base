//! The example game (KIT-ARCHITECTURE §3: the future `gsb-demo`).
//!
//! Everything here is a *game* decision in the §2 sense — "what the bytes
//! are and how the game plays": the components and the 2D position type,
//! the movement system, the wire messages and opcodes, the spawn
//! distribution, the team assignment, what migrates across a shard
//! border, input decoding, the bot's wander, the RPC request handlers
//! and the economy service they delegate to, the PVS map data.
//!
//! The demo uses the kit (`crate::kit`) — that is the dependency
//! direction: it implements the kit's seams (`DemoGame`: `Game`,
//! `TeamGame`, `ShardGame`; `DemoCodec`; `Planar` for `Position` and
//! `StripPos`) and instantiates the kit's rooms with them (`rooms`).
//! The only reverse references are the kit's envelope types, generated
//! from this module's `game.proto` until phase 2 (`crate::kit::seam`).

pub(crate) mod bot;
pub mod codec;
pub mod components;
pub mod economy;
pub(crate) mod input;
pub mod migrate;
pub mod op;
pub mod play;
pub mod rooms;
pub(crate) mod rpc;
pub mod sectors;
pub mod spawn;
pub mod systems;
pub mod wire;

/// Generated game protocol messages (package `gsb.game`, file `game.proto`):
/// the demo's own messages and its typed mirrors of the kit's envelope
/// (`WorldSnapshot`, `Private`). The kit's `InputAck` is used as is and
/// re-exported here, so `game::InputAck` keeps naming it.
pub mod game {
    include!(concat!(env!("OUT_DIR"), "/gsb.game.rs"));

    pub use gsb_kit::proto::InputAck;
}

use gsb_protocol::MessageTable;

/// Register the demo game's wire messages with `table`.
///
/// The server builds one [`MessageTable`] at startup (base messages + game
/// messages) and shares it read-only between all actors.
pub fn register(table: &mut MessageTable) {
    table.reg::<game::MoveTo>(op::MOVE_TO);
    table.reg::<game::WorldSnapshot>(op::WORLD_SNAPSHOT);
    table.reg::<game::Private>(op::PRIVATE);
}
