//! Demo game logic for gsb.
//!
//! This crate is the *only* place that knows anything about a particular
//! game. It plugs into the core via [`gsb_core::room::RoomLogic`] (see
//! [`room::DemoRoom`]) and registers its wire messages with the
//! [`gsb_protocol::MessageTable`] (see [`register`]).
//!
//! Everything here is intentionally small and replaceable: swap the
//! components, systems, and `RoomLogic` implementation for a real MOBA or
//! MMORPG without touching the core, net, protocol, or ecs crates.

pub mod components;
pub mod op;
pub mod room;
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
}
