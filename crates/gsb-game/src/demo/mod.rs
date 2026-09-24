//! The example game (KIT-ARCHITECTURE §3: the future `gsb-demo`).
//!
//! Everything here is a *game* decision in the §2 sense — "what the bytes
//! are and how the game plays": the components and the 2D position type,
//! the movement system, the wire messages and opcodes, the spawn
//! distribution, input decoding, the bot's wander, the RPC request
//! handlers and the economy service they delegate to.
//!
//! The demo may use the kit (`crate::kit`) freely — that is the target
//! dependency direction. The reverse direction is the phase-0 seam: kit
//! code reaches this module ONLY through `crate::kit::seam`, which lists
//! every such coupling (the phase-1 work list).

pub mod components;
pub mod economy;
pub mod op;
pub mod systems;

/// Generated game protocol messages (package `gsb.game`, file `game.proto`).
pub mod game {
    include!(concat!(env!("OUT_DIR"), "/gsb.game.rs"));
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
