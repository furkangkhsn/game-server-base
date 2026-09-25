//! The 3D arena — gsb-kit's second validation demo
//! (`docs/KIT-ARCHITECTURE.md` §11/§12, phase 3).
//!
//! A small team arena (a MOBA / team-shooter floor with a vertical
//! volume) whose visibility model is **team fog of war in 3D**: a team
//! sees a unit iff at least one of its own members is within
//! [`VISION_RADIUS`] of it, measured in 3D — height counts, so a unit on
//! a platform high above an enemy is hidden from it although the two
//! share a spot on the floor. Three teams by default (any number works).
//!
//! **An acceptance test of the kit, not a kit user with privileges:**
//! this crate uses only gsb-kit's PUBLIC surface (it cannot reach a
//! `pub(crate)` item — the proof is structural) and does not depend on
//! the 2D demo. What it brings is exactly what KIT-ARCHITECTURE §2 calls
//! "the game's":
//!
//! - [`components`] — the 3D position [`Pos3`] (metres, y up; the kit's
//!   [`Spatial`](gsb_kit::space::Spatial) accessor), move target, speed,
//!   home base;
//! - [`movement`] — its own 3D kinematic move-to-target system (the kit
//!   has no movement trait, §6);
//! - [`codec`] — its [`RecordCodec`](gsb_kit::codec::RecordCodec): the
//!   position quantized to integer centimetres;
//! - [`game`] — its [`Game`](gsb_kit::game::Game) and
//!   [`TeamGame`](gsb_kit::game::TeamGame) hooks: spawn at the team
//!   base, round-robin team assignment, `MoveTo` input, a bot that
//!   retreats to base;
//! - [`arena`] + [`op`] — its wire (`proto/arena.proto`: its input, its
//!   record, typed mirrors of the kit's envelope) and opcodes.
//!
//! What it takes from the kit: the team-fog room
//! ([`TeamRoom`](gsb_kit::team::TeamRoom)) over the kit's 3D vision
//! preset ([`VisionGrid3`]) — see [`ArenaRoom`]. Everything the room
//! does around the hooks (wire identity, grouping by team, the per-team
//! content and "no change" ledger, the snapshot and `Private`
//! envelopes, the input ack, park/resume) is the kit's.
//!
//! Hosted by `gsb-server` as `game = "arena"` and driven by
//! `gsb-loadgen --game arena` (`docs/GAME-MODULE.md`); its own tests
//! verify it through the real `gsb-core` room actor.

pub mod codec;
pub mod components;
pub mod game;
mod input;
pub mod movement;
pub mod op;

pub use components::{Pos3, VISION_RADIUS};
pub use game::ArenaGame;

use gsb_kit::space::VisionGrid3;
use gsb_kit::team::TeamRoom;
use gsb_protocol::MessageTable;

/// Generated arena protocol messages (package `gsb.arena`, file
/// `arena.proto`): the arena's own messages and its typed mirrors of the
/// kit's envelope. The kit's `InputAck` is used as is and re-exported
/// here.
pub mod arena {
    include!(concat!(env!("OUT_DIR"), "/gsb.arena.rs"));

    pub use gsb_kit::proto::InputAck;
}

/// The arena room: the kit's team-fog room running [`ArenaGame`] over
/// the kit's 3D vision preset on [`Pos3`] (27-cell neighbourhood, exact
/// 3D distance).
pub type ArenaRoom = TeamRoom<ArenaGame, VisionGrid3<Pos3>>;

/// An arena room running `game` with the arena's vision radius
/// ([`VISION_RADIUS`]). (A free function, not a constructor: the room
/// type is the kit's, so an inherent impl here is E0116.)
#[must_use]
pub fn arena_room(game: ArenaGame) -> ArenaRoom {
    TeamRoom::with_game(game, VisionGrid3::new(VISION_RADIUS))
}

/// Register the arena's wire messages with `table` (the server builds
/// one table at startup: base messages + a game's messages).
pub fn register(table: &mut MessageTable) {
    table.reg::<arena::MoveTo>(op::ARENA_MOVE_TO);
    table.reg::<arena::WorldSnapshot>(op::ARENA_SNAPSHOT);
    table.reg::<arena::Private>(op::ARENA_PRIVATE);
}
