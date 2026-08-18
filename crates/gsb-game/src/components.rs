//! ECS components of the demo game.

use bevy_ecs::prelude::Component;
use gsb_core::id::ConnectionId;

/// Position on the 2D map plane (world units).
#[derive(Debug, Clone, Copy, PartialEq, Default, Component)]
pub struct Position {
    pub x: f32,
    pub y: f32,
}

/// The entity's wire identity (see `game.proto`, `EntityRecord.entity`).
///
/// Room-assigned and monotonic: the room hands out the next value at
/// spawn and **never reuses a value within the room's lifetime**, even
/// when the ECS allocator recycles the old entity's slot. That is what
/// lets the client tell "the same entity moved" from "a new entity took
/// the slot" using only self-contained snapshots (see `game.proto`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Component)]
pub struct WireId(pub u64);

/// The connection that owns (controls) this entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Component)]
pub struct Owner(pub ConnectionId);

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
