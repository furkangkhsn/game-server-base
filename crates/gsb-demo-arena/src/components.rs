//! The arena's ECS components and its world constants. The world unit
//! is the METRE; axis order x, y, z with **y up** (height).

use bevy_ecs::prelude::Component;
use gsb_kit::space::Spatial;

/// Half the arena's horizontal extent: x and z live in
/// `[-ARENA_HALF, ARENA_HALF]` (a 100 m × 100 m floor).
pub const ARENA_HALF: f32 = 50.0;

/// The arena's ceiling: heights live in `[0, CEILING]` (ramps,
/// platforms, jump pads — a vertical volume, which is why the fog is 3D).
pub const CEILING: f32 = 30.0;

/// The uniform vision radius of every unit, in metres (3D distance). A
/// fifth of the floor's side: a small arena where vision is contested,
/// and — against [`CEILING`] — a height gap alone can hide a unit.
pub const VISION_RADIUS: f32 = 15.0;

/// A unit's movement speed when nothing else says (m/s): fast movement
/// (KIT-ARCHITECTURE §12 "küçük oda, hızlı hareket") — the floor's
/// diagonal in about 12 s.
pub const DEFAULT_SPEED: f32 = 12.0;

/// A unit's position — the simulation value team vision is measured on
/// (the codec's marker: every unit carrying it is broadcast).
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

    /// This position clamped into the arena's volume.
    #[must_use]
    pub fn clamped(self) -> Self {
        Self {
            x: self.x.clamp(-ARENA_HALF, ARENA_HALF),
            y: self.y.clamp(0.0, CEILING),
            z: self.z.clamp(-ARENA_HALF, ARENA_HALF),
        }
    }
}

/// The kit's 3D accessor: all three axes, height included — what
/// `VisionGrid3` reads. (No `Planar` impl: the arena runs no
/// ground-plane preset; its fog is volumetric on purpose.)
impl Spatial for Pos3 {
    type Coord = f32;

    #[inline]
    fn spatial(&self) -> [f32; 3] {
        [self.x, self.y, self.z]
    }
}

/// Where the unit's owner wants it to go (written by input decoding,
/// already clamped into the arena). Kept after arrival: the movement
/// system simply stops writing the position once it is there.
#[derive(Debug, Clone, Copy, PartialEq, Component)]
pub struct MoveTarget3(pub Pos3);

/// Movement speed, metres per second.
#[derive(Debug, Clone, Copy, PartialEq, Component)]
pub struct Speed(pub f32);
