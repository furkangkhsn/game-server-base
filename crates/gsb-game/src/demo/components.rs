//! ECS components of the demo game.
//!
//! The wire identity (`WireId`) is not here: identity is kit-owned
//! (KIT-ARCHITECTURE §4.4) and lives in `crate::kit::identity`; the
//! public `gsb_game::components` path still re-exports it.

use bevy_ecs::prelude::Component;

/// Position on the 2D map plane (world units).
#[derive(Debug, Clone, Copy, PartialEq, Default, Component)]
pub struct Position {
    pub x: f32,
    pub y: f32,
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
