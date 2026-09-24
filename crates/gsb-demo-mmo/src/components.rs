//! The MMO's ECS components. The world unit is the METRE; axis order
//! x, y, z with **y up** (height); the ground plane is (x, z).

use bevy_ecs::prelude::Component;
use gsb_kit::space::Planar;

use crate::world::{CEILING, WORLD_HALF};

/// An entity's position — the simulation value (the codec's marker:
/// every entity carrying it is broadcast, players and mobs alike).
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

    /// The 3D distance to `other`, metres.
    #[must_use]
    pub fn dist(&self, other: &Pos3) -> f32 {
        let d = [self.x - other.x, self.y - other.y, self.z - other.z];
        (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
    }
}

/// The kit's ground-plane accessor: `[x, z]` — height is NOT part of the
/// projection. `GridPartition2` reads it for the shard regions and the
/// border export: a flyer 150 m up is owned by the shard under it.
impl Planar for Pos3 {
    type Coord = f32;

    #[inline]
    fn planar(&self) -> [f32; 2] {
        [self.x, self.z]
    }
}

/// What an entity is (the record's `kind`; `mmo.proto`'s `Kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Kind {
    Player = 1,
    /// A ground mob.
    Mob = 2,
    /// A flying mob (holds its altitude).
    Flyer = 3,
}

/// The broadcast vitals: kind and hit points (both on the wire — the
/// codec's `Dirty` is position OR vitals changed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Component)]
pub struct Vitals {
    pub kind: Kind,
    pub hp: u16,
}

/// Where a player walks to on the ground (written by `MoveTo` input and
/// by the logout bot; already clamped). Removed on arrival.
#[derive(Debug, Clone, Copy, PartialEq, Component)]
pub struct MoveTarget {
    pub x: f32,
    pub z: f32,
}

/// A player's run speed, metres per second. Players only: a mob's pace
/// lives in its [`Mob`] brain — mobs deliberately carry NO speed-like
/// component (KIT-ARCHITECTURE §8.5: the kit migrates every broadcast
/// entity, whatever it carries).
#[derive(Debug, Clone, Copy, PartialEq, Component)]
pub struct RunSpeed(pub f32);

/// A mob's brain: its patrol route on the ground, its pace, and when it
/// despawns. Spawned and despawned by GAME code (the spawn table and the
/// lifecycle system — never by joins/leaves); travels whole across a
/// shard border inside the MMO's `Mig`.
#[derive(Debug, Clone, PartialEq, Component)]
pub struct Mob {
    /// Ground waypoints `(x, z)`, walked in order; the mob holds at the
    /// last one (or loops back to the first when `patrol`).
    pub route: Vec<[f32; 2]>,
    /// The waypoint being walked to.
    pub leg: usize,
    /// Metres per second along the route.
    pub pace: f32,
    /// Loop the route instead of holding at its end.
    pub patrol: bool,
    /// The global tick at which the mob despawns (a camp's wave ends).
    pub dies_at: u64,
}

/// A player is IN COMBAT until global tick `until`: every attack of its
/// that lands sets `until` to [`crate::world::COMBAT_TICKS`] past the
/// hit, and the combat system removes the marker once it has passed. A
/// disconnected character does not log out while it carries one (the
/// MMO's `Game::may_release` veto). Travels with the player across a
/// shard border.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Component)]
pub struct InCombat {
    pub until: u64,
}
