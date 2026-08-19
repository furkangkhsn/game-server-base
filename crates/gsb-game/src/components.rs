//! ECS components of the demo game.

use bevy_ecs::prelude::Component;

/// Position on the 2D map plane (world units).
#[derive(Debug, Clone, Copy, PartialEq, Default, Component)]
pub struct Position {
    pub x: f32,
    pub y: f32,
}

/// The entity's wire identity (see `game.proto`, `EntityRecord.entity`).
///
/// The field is **private and the `Default` derive is gone on purpose**:
/// the identity invariant ("two different entities cannot share the same
/// wire identity over the room's lifetime") has exactly one legitimate
/// source — the room's monotonic counter — and the type now says so. The
/// only construction path is [`WireId::new`], which is crate-private and
/// is called from the rooms' single shared minting point
/// ([`crate::common::next_serial`]); the two call sites (`on_join`
/// for player entities, whose value also goes to the joiner in
/// `JOIN_ROOM_RESULT`, and the broadcast pass for any other entity that
/// carries a [`Position`]) both go through it. No other code — in this
/// crate or any other — can fabricate a `WireId` that collides with the
/// counter's space. Reading is open: [`WireId::get`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Component)]
pub struct WireId(u64);

impl WireId {
    /// Mint a wire identity from a room counter value. Crate-private by
    /// design: external crates cannot construct identities at all, and
    /// within the crate only the room's minting point calls this.
    pub(crate) const fn new(id: u64) -> Self {
        Self(id)
    }

    /// Read the serial (the wire side).
    pub const fn get(self) -> u64 {
        self.0
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
