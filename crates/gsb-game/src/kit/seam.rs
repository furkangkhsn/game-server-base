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

// ── Transitional: the sharded spatial composite (generic next) ─────────

// The composite still wraps the demo's shard and drives the cell-delta
// engine with the demo's codec over the demo's wire value: the demo's
// `Game`, its codec, its position (the group-of fallback) and its wire
// value (the border strip's payload, `Strip = Wire`), and the frame
// opcodes. Consumer: the sharded spatial composite.
pub(crate) use crate::demo::codec::DemoCodec;
pub(crate) use crate::demo::components::Position;
pub(crate) use crate::demo::op::{PRIVATE, WORLD_SNAPSHOT};
pub(crate) use crate::demo::play::DemoGame;
pub(crate) use crate::demo::wire::StripPos;

// ── Kit envelope (§5 — moves into the kit's own proto) ───────────────────

// The private-frame envelope and the input ack: generated from the demo's
// game.proto today; the kit proto owns them in phase 2. Consumer:
// `common::emit_private` (every room).
pub(crate) use crate::demo::game::{InputAck, Private, private};

// The snapshot envelope: the kit writes it by hand (`common/frame.rs`);
// the typed message is what the in-module tests decode snapshots with.
#[cfg(test)]
pub(crate) use crate::demo::game::WorldSnapshot;

// ── Test fixtures (kit's in-module tests drive the kit rooms with the
//    demo game; phase 1 replaces them with the kit's own small test game)

// The typed `CellExit` mirror the AOI tests decode snapshots with.
#[cfg(test)]
pub(crate) use crate::demo::game::CellExit;

// The demo's components and default movement speed (tests spawn and
// inspect demo entities directly), and its migrating state (tests build
// one by hand).
#[cfg(test)]
pub(crate) use crate::demo::components::{DEFAULT_SPEED, MoveTarget, Speed};
#[cfg(test)]
pub(crate) use crate::demo::migrate::DemoMig;

// The demo map's named sectors the PVS tests address directly.
#[cfg(test)]
pub(crate) use crate::demo::sectors::{SECTOR_EAST, SECTOR_NW, SECTOR_OUT, SECTOR_WEST};
