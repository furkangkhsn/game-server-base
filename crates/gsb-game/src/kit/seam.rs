//! **THE phase-0 seam — temporary.** Every place kit code reaches demo
//! code or a demo type goes through this one module (KIT-ARCHITECTURE
//! §10, phase 0): outside this file, nothing under `kit/` names the demo
//! module. Its contents are therefore the exact, reviewable work list of
//! phase 1 (dependency inversion): each item below is grouped by the §4
//! seam it will become, and phase 1 is done when this file is empty and
//! deleted.
//!
//! **After phase 1a** the §4.1/§4.2/§4.3 traits exist (`kit::codec`,
//! `kit::space`, `kit::game`) and the two converted rooms — `OpenRoom<G>`
//! and `AoiRoom<G, S>` — consume nothing from here: every remaining
//! entry names its consumers, all of them phase 1b's (the team and PVS
//! rooms and the sharded composites) or the kit's envelope (phase 2).
//!
//! The rule is locked by a source-scanning unit test
//! (`crate::layering`), not only by review: under `kit/`, every `crate::`
//! path outside this file continues into `kit`, and `crate::demo`
//! appears in this file only. The same check by hand:
//!
//! ```text
//! grep -rn "crate::demo" crates/gsb-game/src/kit   # → only kit/seam.rs
//! ```

// ── RecordCodec (§4.1) ───────────────────────────────────────────────────

// RecordCodec::Marker / Query: the broadcast set ("has a Position") and
// the record source (also the Pos of Vision / SectorMap / Partition).
// Consumers: team, PVS, sharded (orphan stamping, snapshot encoders,
// vision/sector/region lookups, migration capture).
pub(crate) use crate::demo::components::Position;

// RecordCodec::encode: the typed body of one entity record. Consumers:
// the team, PVS and sharded snapshot encoders (`OpenRoom` writes the same
// envelope through the codec since 1a).
pub(crate) use crate::demo::game::EntityRecord;

// RecordCodec::Wire: the demo's wire value (truncated position) — also
// the sharded rooms' `Strip` payload (`Strip = Wire`). Consumers: sharded.
pub(crate) use crate::demo::wire::StripPos;

// RecordCodec itself: the demo's codec (`Wire = (i32, i32)`), with which
// the generic cell-delta engine is instantiated where a room is not yet
// generic over the game. Consumer: the sharded spatial composite.
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
// out-of-map sector the room falls back to. Consumer: PVS.
pub(crate) use crate::demo::sectors::{SECTOR_OUT, Sector};

// SectorMap::sector_of: point-in-convex-sector lookup over the demo map.
// Consumer: PVS.
pub(crate) use crate::demo::sectors::sector_of;

// SectorMap::visible_from: the static visibility table (a u16 bitmask per
// sector today; §8.4 notes its 16-sector ceiling). Consumer: PVS.
pub(crate) use crate::demo::sectors::VISIBLE_FROM;

// ── Partition (§4.2) ─────────────────────────────────────────────────────

// (no entry) The sharded rooms read `Position` as `Partition::Pos`; the
// grid partition itself (`grid_shape`, `shard_at`, the neighbour list,
// the border frame) is the future `GridPartition2` preset and is kit code
// already. The border strip's payload is `RecordCodec::Wire` (above).

// ── Game hooks (§4.3) ────────────────────────────────────────────────────
//
// The `Game` trait exists (`kit::game`) and the demo implements it
// (`DemoGame`); the entries below are the same hooks called directly by
// the rooms not yet generic over the game.

// Game::spawn_player (the kit stamps WireId). Consumers: team, PVS (via
// `common::on_join`), sharded.
pub(crate) use crate::demo::spawn::spawn_player;

// Game::on_player_spawned: the team room's join-time team assignment.
// Consumer: team.
pub(crate) use crate::demo::spawn::team_of;

// Game::Mig + capture: the migrating game state the sharded rooms read
// in `collect_migrations` and carry in `ShardedRoomState` (position is
// above) — the speed and the pending move target. Consumer: sharded.
pub(crate) use crate::demo::components::{MoveTarget, Speed};

// Game::restore: rebuild a migrated entity on the receiving shard.
// Consumer: sharded.
pub(crate) use crate::demo::spawn::restore_migrant;

// Game::systems: the demo's system stack (movement). Consumers: team,
// PVS, sharded.
pub(crate) use crate::demo::systems::movement_runner;

// Game::ingest: MOVE_TO decoding + application (the seq rule is kit's).
// Consumers: team, PVS, sharded.
pub(crate) use crate::demo::input::ingest;

// Game::bot_actions: the bot's synthesized wander input. Consumers: team,
// PVS, sharded.
pub(crate) use crate::demo::bot::synthesize_bot_moves;

// Game::handle_request: the ABILITY / ECONOMY request handlers …
// Consumer: sharded.
pub(crate) use crate::demo::rpc::handle_request;

// … and the economy service handle they delegate to (a room field in the
// sharded rooms; `DemoGame` state in the converted ones). Consumer:
// sharded.
pub(crate) use crate::demo::economy::EconomyService;

// Game::spawn_player's configuration: the default spawn map half-size
// the room constructors fall back to. Consumers: team, PVS.
pub(crate) use crate::demo::spawn::DEFAULT_SPAWN_HALF;

// Game::SNAPSHOT_OP / Game::PRIVATE_OP: the frame opcodes. Consumers:
// team, PVS, sharded.
pub(crate) use crate::demo::op::{PRIVATE, WORLD_SNAPSHOT};

// ── Identity (§4.4) ──────────────────────────────────────────────────────

// (no entry) `WireId` and its single `Minter` (sequential / range) are
// kit-owned (`kit::identity`, §8.1 closed in phase 1a).

// ── Kit envelope (§5 — moves into the kit's own proto) ───────────────────

// The snapshot / private-frame envelopes and the input ack: generated
// from the demo's game.proto today; the kit proto owns them in phase 2.
// Consumers: `common::emit_private` (every room), the team, PVS and
// sharded snapshot encoders (`WorldSnapshot`).
pub(crate) use crate::demo::game::{InputAck, Private, WorldSnapshot, private};

// ── Test fixtures (kit's in-module tests drive the kit rooms with the
//    demo game; phase 1 replaces them with the kit's own small test game)

// The demo's `Game`: the in-module tests of the generic rooms drive the
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
