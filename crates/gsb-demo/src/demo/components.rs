//! ECS components of the demo game.
//!
//! The wire identity (`WireId`) is not here: identity is kit-owned
//! (KIT-ARCHITECTURE §4.4) and lives in `gsb_kit::identity`; the
//! public `gsb_demo::components` path still re-exports it.

use bevy_ecs::prelude::Component;

use gsb_kit::space::Planar;

/// Position on the 2D map plane (world units).
#[derive(Debug, Clone, Copy, PartialEq, Default, Component)]
pub struct Position {
    pub x: f32,
    pub y: f32,
}

/// The simulation position on the ground plane — what the kit's 2D
/// simulation presets (team vision, the PVS sector map, the shard grid)
/// read.
impl Planar for Position {
    type Coord = f32;

    #[inline]
    fn planar(&self) -> [f32; 2] {
        [self.x, self.y]
    }
}

/// Where the owner wants the entity to go. Inserted by `ingest`, removed
/// by the movement system when the entity arrives.
#[derive(Debug, Clone, Copy, PartialEq, Default, Component)]
pub struct MoveTarget {
    pub x: f32,
    pub y: f32,
}

/// Movement speed in units per second (default: [`DEFAULT_SPEED`]).
#[derive(Debug, Clone, Copy, PartialEq, Component)]
pub struct Speed(pub f32);

/// Units per second used when an entity has no explicit [`Speed`].
pub const DEFAULT_SPEED: f32 = 10.0;

/// The player's verified identity (the join's ticket player, or the
/// local-auth name), stamped at spawn when the session has one: the
/// identity a purchase is made on behalf of, and the one a granted item
/// (`ItemGranted`, round e5) is addressed to. Migrates with the entity.
#[derive(Debug, Clone, PartialEq, Eq, Component)]
pub struct Account(pub gsb_core::calls::Identity);

/// What the player was granted (the WORLD leg of `ItemGranted`, round
/// e5): the count, and the highest grant version seen (a grant older
/// than it is a duplicate). Migrates with the entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Component)]
pub struct Granted {
    pub items: u32,
    pub version: u64,
}
