//! **THE phase-0 seam — temporary.** Every place kit code reaches demo
//! code or a demo type goes through this one module (KIT-ARCHITECTURE
//! §10, phase 0): outside this file, nothing under `kit/` names the demo
//! module. Its contents are therefore the exact, reviewable work list of
//! phase 1 (dependency inversion): each item below is grouped by the §4
//! seam it will become, and phase 1 is done when this file is empty and
//! deleted.
//!
//! The rule is locked by a source-scanning unit test
//! (`crate::layering`), not only by review.

// ── RecordCodec (§4.1) ───────────────────────────────────────────────────

// RecordCodec::Marker / Query: the broadcast set ("has a Position") and
// the record source (also the Pos of Vision / SectorMap / Partition).
pub(crate) use crate::demo::components::Position;

// RecordCodec::encode: the typed body of one entity record.
pub(crate) use crate::demo::game::EntityRecord;

// ── CellSpace (§4.2) ─────────────────────────────────────────────────────

// CellSpace::encode_cell: the typed body of a `CellExit` record.
pub(crate) use crate::demo::game::CellExit;

// ── Game hooks (§4.3) ────────────────────────────────────────────────────

// Game::spawn_player: spawn point + player bundle (the kit stamps WireId).
pub(crate) use crate::demo::spawn::spawn_player;

// Game::systems: the demo's system stack (movement).
pub(crate) use crate::demo::systems::movement_runner;

// Game::ingest: MOVE_TO decoding + application (the seq rule is kit's).
pub(crate) use crate::demo::input::ingest;

// Game::bot_actions: the bot's synthesized wander input.
pub(crate) use crate::demo::bot::synthesize_bot_moves;

// Game::handle_request: the ABILITY / ECONOMY request handlers …
pub(crate) use crate::demo::rpc::handle_request;

// … and the economy service handle they delegate to (a room field today;
// Game state in phase 1).
pub(crate) use crate::demo::economy::EconomyService;

// Game::spawn_player's configuration: the default spawn map half-size
// the room constructors fall back to.
pub(crate) use crate::demo::spawn::DEFAULT_SPAWN_HALF;

// Game::SNAPSHOT_OP / Game::PRIVATE_OP: the frame opcodes.
pub(crate) use crate::demo::op::{PRIVATE, WORLD_SNAPSHOT};

// ── Kit envelope (§5 — moves into the kit's own proto) ───────────────────

// The snapshot / private-frame envelopes and the input ack: generated
// from the demo's game.proto today; the kit proto owns them in phase 2.
pub(crate) use crate::demo::game::{InputAck, Private, WorldSnapshot, private};

// ── Test fixtures (kit's in-module tests drive the kit rooms with the
//    demo game; phase 1 replaces them with the kit's own small test game)

// The demo's movement speed component and its default (tests spawn and
// inspect demo entities directly).
#[cfg(test)]
pub(crate) use crate::demo::components::{DEFAULT_SPEED, Speed};
