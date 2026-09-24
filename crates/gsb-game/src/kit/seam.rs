//! **THE phase-0 seam — temporary.** Every place kit code reaches demo
//! code or a demo type goes through this one module (KIT-ARCHITECTURE
//! §10, phase 0): outside this file, nothing under `kit/` names the demo
//! module. Its contents are therefore the exact, reviewable work list of
//! phase 1 (dependency inversion): each item below is grouped by the §4
//! seam it will become, and phase 1 is done when this file is empty and
//! deleted.
//!
//! The rule is locked by a source-scanning unit test
//! (`crate::layering`), not only by review: under `kit/`, every `crate::`
//! path outside this file continues with `kit::`, and `crate::demo`
//! appears in this file only. The same check by hand:
//!
//! ```text
//! grep -rn "crate::demo" crates/gsb-game/src/kit   # → only kit/seam.rs
//! ```
//!
//! Some §4 seams have no entry of their own: they consume `Position`
//! (listed once, under RecordCodec) and nothing else from the game — the
//! kit-side machinery behind them (the 2D cell grid, the vision grid, the
//! shard grid) is the future §7 preset and already lives in the kit.

// ── RecordCodec (§4.1) ───────────────────────────────────────────────────

// RecordCodec::Marker / Query: the broadcast set ("has a Position") and
// the record source (also the Pos of Vision / SectorMap / Partition).
pub(crate) use crate::demo::components::Position;

// RecordCodec::encode: the typed body of one entity record.
pub(crate) use crate::demo::game::EntityRecord;

// RecordCodec::Wire: the demo's wire value (truncated position) — also
// the sharded rooms' `Strip` payload (`Strip = Wire`).
pub(crate) use crate::demo::wire::StripPos;

// RecordCodec itself: the demo's codec (`Wire = (i32, i32)`), which the
// generic cell-delta engine is instantiated with where a room is not yet
// generic over the game.
pub(crate) use crate::demo::codec::DemoCodec;

// ── CellSpace (§4.2) ─────────────────────────────────────────────────────

// (no entry) The kit's `Grid2` preset (`kit::space`) writes the
// `CellExit` body itself; the demo pins it to its typed `CellExit`.

// ── Vision (§4.2) ────────────────────────────────────────────────────────

// (no entry) The team room reads `Position` as `Vision::Pos`; its vision
// grid (`grid_cell`, `VISION_OFFSETS`, the squared-distance test) is the
// future `Grid2` preset and is kit code already. The team assignment rule
// is a Game hook (below).

// ── SectorMap (§4.2) ─────────────────────────────────────────────────────

// SectorMap::Sector: the sector key (the sector room's GroupKey) and the
// out-of-map sector the room falls back to.
pub(crate) use crate::demo::sectors::{SECTOR_OUT, Sector};

// SectorMap::sector_of: point-in-convex-sector lookup over the demo map.
pub(crate) use crate::demo::sectors::sector_of;

// SectorMap::visible_from: the static visibility table (a u16 bitmask per
// sector today; §8.4 notes its 16-sector ceiling).
pub(crate) use crate::demo::sectors::VISIBLE_FROM;

// ── Partition (§4.2) ─────────────────────────────────────────────────────

// (no entry) The sharded rooms read `Position` as `Partition::Pos`; the
// grid partition itself (`grid_shape`, `shard_at`, the neighbour list,
// the border frame) is the future `GridPartition2` preset and is kit code
// already. The border strip's payload is `RecordCodec::Wire` (above).

// ── Game hooks (§4.3) ────────────────────────────────────────────────────

// Game::spawn_player: spawn point + player bundle (the kit stamps WireId).
pub(crate) use crate::demo::spawn::spawn_player;

// Game::on_player_spawned: the team room's join-time team assignment.
pub(crate) use crate::demo::spawn::team_of;

// Game::Mig + capture: the migrating game state the sharded rooms read
// in `collect_migrations` and carry in `ShardedRoomState` (position is
// above) — the speed and the pending move target.
pub(crate) use crate::demo::components::{MoveTarget, Speed};

// Game::restore: rebuild a migrated entity on the receiving shard.
pub(crate) use crate::demo::spawn::restore_migrant;

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

// ── Identity (§4.4) ──────────────────────────────────────────────────────

// (no entry) `WireId` and its single `Minter` (sequential / range) are
// kit-owned (`kit::identity`, §8.1 closed in phase 1a).

// ── Kit envelope (§5 — moves into the kit's own proto) ───────────────────

// The snapshot / private-frame envelopes and the input ack: generated
// from the demo's game.proto today; the kit proto owns them in phase 2.
pub(crate) use crate::demo::game::{InputAck, Private, WorldSnapshot, private};

// ── Test fixtures (kit's in-module tests drive the kit rooms with the
//    demo game; phase 1 replaces them with the kit's own small test game)

// The demo's `Game`: the kit's generic rooms' in-module tests drive the
// demo instantiation.
#[cfg(test)]
pub(crate) use crate::demo::play::DemoGame;

// The typed `CellExit` mirror the AOI tests decode snapshots with.
#[cfg(test)]
pub(crate) use crate::demo::game::CellExit;

// The demo's default movement speed (tests spawn and inspect demo
// entities directly).
#[cfg(test)]
pub(crate) use crate::demo::components::DEFAULT_SPEED;

// The demo map's named sectors the PVS tests address directly.
#[cfg(test)]
pub(crate) use crate::demo::sectors::{SECTOR_EAST, SECTOR_NW, SECTOR_WEST};
