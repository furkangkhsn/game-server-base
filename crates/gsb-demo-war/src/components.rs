//! The war game's ECS components. The world unit is the METRE; axis
//! order x, y, z with **y up** (height); the ground plane is (x, z).
//!
//! A unit's faction lives twice in the world, on purpose: as the kit's
//! [`TeamMember`](gsb_kit::team::TeamMember) (what the team fog reads —
//! the kit writes it for a joining player, the game for its towers and a
//! captured point) and in [`Unit::faction`] (what the codec puts on the
//! wire, so a client tells ally from enemy). Every write of one writes
//! the other.

use bevy_ecs::prelude::Component;
use gsb_kit::space::Planar;
use gsb_kit::team::Team;

use crate::world::{CEILING, WORLD_HALF};

/// A unit's position — the simulation value (the codec's marker: every
/// unit carrying it is broadcast) and the vision position (the kit's
/// `VisionGrid2` reads its ground plane).
#[derive(Debug, Clone, Copy, PartialEq, Default, Component)]
pub struct Pos3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Pos3 {
    /// A position at (`x`, `y`, `z`) metres.
    #[must_use]
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    /// A position on the ground at (`x`, `z`).
    #[must_use]
    pub const fn ground(x: f32, z: f32) -> Self {
        Self { x, y: 0.0, z }
    }

    /// This position clamped into the world (the square map on the
    /// ground, `[0, CEILING]` in height).
    #[must_use]
    pub fn clamped(self) -> Self {
        Self {
            x: self.x.clamp(-WORLD_HALF, WORLD_HALF),
            y: self.y.clamp(0.0, CEILING),
            z: self.z.clamp(-WORLD_HALF, WORLD_HALF),
        }
    }

    /// The distance to `other` on the ground (height ignored), metres.
    #[must_use]
    pub fn ground_dist(&self, other: &Pos3) -> f32 {
        let (dx, dz) = (self.x - other.x, self.z - other.z);
        (dx * dx + dz * dz).sqrt()
    }
}

/// The kit's ground-plane accessor: `[x, z]` — height is NOT part of the
/// projection. `GridPartition2` reads it for the shard regions, and
/// `VisionGrid2` for vision: a tower's platform does not change what it
/// sees.
impl Planar for Pos3 {
    type Coord = f32;

    #[inline]
    fn planar(&self) -> [f32; 2] {
        [self.x, self.z]
    }
}

/// What a unit is (the record's `kind`; `war.proto`'s `Kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Kind {
    Player = 1,
    /// A faction's watchtower: static, invulnerable, a vision source.
    Tower = 2,
    /// A capture point: neutral until a faction takes it, then that
    /// faction's (a vision source for it, like a tower).
    Point = 3,
}

/// The broadcast part of a unit besides its position: kind, faction
/// (`None`: an unclaimed point) and hit points (players only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Component)]
pub struct Unit {
    pub kind: Kind,
    pub faction: Option<Team>,
    pub hp: u16,
}

/// Where a player walks to on the ground (written by `MoveTo` input and
/// by the retreat bot; already clamped).
#[derive(Debug, Clone, Copy, PartialEq, Component)]
pub struct MoveTarget {
    pub x: f32,
    pub z: f32,
}

/// A capture point's state: who holds it and who is taking it. A point
/// flips to a faction whose players stood ALONE within
/// [`crate::world::CAPTURE_RADIUS`] for [`crate::world::CAPTURE_TICKS`]
/// ticks in a row (any other faction there, or nobody, resets the
/// count).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Component)]
pub struct Capture {
    /// The faction taking the point, and for how many ticks in a row.
    pub claim: Option<(Team, u32)>,
}
