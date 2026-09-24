//! **The kit→demo seam, after phase 1.** Every place kit code reaches
//! demo code or a demo type goes through this one module
//! (KIT-ARCHITECTURE §10): outside this file, nothing under `kit/` names
//! the demo module.
//!
//! **Phase 1 emptied it** of everything the kit needed the demo game
//! for: the codec, the spaces, the sector map, the partition and every
//! gameplay hook are seams the demo implements (`kit::codec`,
//! `kit::space`, `kit::game`), and every room is generic over them. What
//! remains outside the test fixtures is the kit's own envelope — the
//! `Private` frame and the input ack, generated from the demo's
//! `game.proto` today — which phase 2 moves into the kit's proto, after
//! which this file is deleted.
//!
//! The test fixtures are genuinely fixtures: the kit's in-module tests
//! drive the generic rooms with the demo's instantiation (its `Game`,
//! components, wire value, map constants) and decode snapshots with the
//! demo's typed mirror.
//!
//! The rule is locked by a source-scanning unit test
//! (`crate::layering`), not only by review: under `kit/`, every `crate::`
//! path outside this file continues into `kit`, and `crate::demo`
//! appears in this file only. The same check by hand:
//!
//! ```text
//! grep -rn "crate::demo" crates/gsb-demo/src/kit   # → only kit/seam.rs
//! ```

// ── Kit envelope (§5 — moves into the kit's own proto) ───────────────────

// The private-frame envelope and the input ack: the kit's own proto
// (gsb-kit's `kit.proto`). Consumer: `common::emit_private` (every room).
pub(crate) use gsb_kit::proto::{InputAck, Private, private};

// The snapshot envelope: the kit writes it by hand (`common/frame.rs`);
// the demo's typed mirrors are what the in-module tests decode snapshots
// and one-shot private fulls with.
#[cfg(test)]
pub(crate) use crate::demo::game::WorldSnapshot;
#[cfg(test)]
pub(crate) use crate::demo::game::{Private as TypedPrivate, private as typed_private};

// ── Test fixtures (the kit's in-module tests drive the generic rooms
//    with the demo's instantiation)

// The typed `CellExit` mirror the AOI tests decode snapshots with.
#[cfg(test)]
pub(crate) use crate::demo::game::CellExit;

// The demo's `Game`: the in-module tests of the generic rooms drive the
// demo instantiation.
#[cfg(test)]
pub(crate) use crate::demo::play::DemoGame;

// The demo's components and default movement speed (tests spawn and
// inspect demo entities directly), its wire value (tests hand-build
// border records) and its migrating state (tests build one by hand).
#[cfg(test)]
pub(crate) use crate::demo::components::{DEFAULT_SPEED, MoveTarget, Position, Speed};
#[cfg(test)]
pub(crate) use crate::demo::migrate::DemoMig;
#[cfg(test)]
pub(crate) use crate::demo::wire::StripPos;

// The demo map's named sectors the PVS tests address directly.
#[cfg(test)]
pub(crate) use crate::demo::sectors::{SECTOR_EAST, SECTOR_NW, SECTOR_OUT, SECTOR_WEST};
