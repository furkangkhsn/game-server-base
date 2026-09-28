//! [`LitGame`] — the per-viewer light of the lit AOI room
//! ([`LitAoiRoom`](crate::aoi::LitAoiRoom), KIT-ARCHITECTURE §10 "A9").
//!
//! The AOI room's fog is content-independent: a viewer sees every record
//! in its cell's neighbourhood (the 3×3 block, 27 cells in 3D), and the
//! only knob is the cell size. A game that wants less — a light cone, a
//! line of sight, a stealth rule — decides it here, per (viewer, record),
//! INSIDE that neighbourhood; the kit guarantees that a record the rule
//! leaves unlit sends no byte of itself to that viewer, and that every
//! viewer the rule leaves alone keeps the cell's shared packet.

use bevy_ecs::prelude::{Entity, World};

use crate::game::{Game, Wire};

/// A game the lit AOI room can run: which records of a viewer's
/// neighbourhood the viewer sees this tick.
///
/// A strategy-specific extension of [`Game`] (like
/// [`TeamGame`](crate::game::TeamGame)): only the lit room calls it, and
/// a game that never filters its AOI view never answers it.
///
/// **Asked once per tick, after the systems** (the world is the tick's
/// final state): [`Self::light`] for every player of the room, then
/// [`Self::lit`] for every record in the neighbourhood of each viewer
/// that has a light. Both read the world only.
pub trait LitGame: Game {
    /// What the rule needs to know about ONE viewer, computed once per
    /// viewer per tick and handed to every [`Self::lit`] question of that
    /// viewer (a cone's apex, facing and half-angle; a line-of-sight
    /// origin; the viewer's detection level; its entity). Never stored
    /// past the tick.
    type Light;

    /// The light of the player whose entity is `viewer`, this tick, or
    /// `None`: the viewer sees its whole neighbourhood and shares its
    /// cell's packet — the AOI room's view and bytes, unchanged. A viewer
    /// with a light gets its own frames (the kit's cost model:
    /// `LitAoiRoom`'s docs).
    fn light(&self, world: &World, viewer: Entity) -> Option<Self::Light>;

    /// Whether the viewer whose light is `light` sees the record of
    /// entity `record` (its wire value: `wire` — what the client would
    /// receive), a record of the viewer's neighbourhood. `false`: no byte
    /// of the record reaches that viewer this tick (a record it held is
    /// removed from its view). The viewer's OWN record is asked too: a
    /// rule that should always show it says so.
    fn lit(&self, light: &Self::Light, world: &World, record: Entity, wire: &Wire<Self>) -> bool;
}
