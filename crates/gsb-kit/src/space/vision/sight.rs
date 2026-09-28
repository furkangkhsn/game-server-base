//! Per-unit sight in the kit's vision presets (BACKLOG A8): the clamp
//! a unit's own radius goes through and the ring count of the widened
//! neighbourhood (a child of the vision module).

/// How far a per-unit sight radius reaches in the kit's presets, in
/// cells of the preset's grid (whose cell is the preset's radius): a
/// [`SightRadius`](crate::team::SightRadius) is clamped to `[1,
/// MAX_SIGHT_CELLS · radius]` (a `NaN` counts as 1), so a team's
/// enemy test reads at most `(2 · MAX_SIGHT_CELLS + 1)²` cells (2D) or
/// its cube (3D) per target. A game whose units see farther builds the
/// preset with a larger radius (fewer, fuller cells).
pub const MAX_SIGHT_CELLS: u8 = 4;

/// A per-unit radius as the presets apply it (see [`MAX_SIGHT_CELLS`]).
#[inline]
pub(super) fn clamp_sight(radius: f32, cell: f32) -> f32 {
    radius.max(1.0).min(cell * f32::from(MAX_SIGHT_CELLS))
}

/// How many rings of `cell`-sized cells around a target a source with
/// sight `reach` can see into: at least one (the preset's own sources),
/// at most [`MAX_SIGHT_CELLS`]. Two units within `r` differ by at most
/// `r` on every axis, so their cell indices by at most `ceil(r / cell)`.
#[inline]
pub(super) fn rings(reach: f32, cell: f32) -> i32 {
    let rings = (clamp_sight(reach, cell) / cell).ceil() as i32;
    rings.clamp(1, i32::from(MAX_SIGHT_CELLS))
}
