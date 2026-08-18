//! Identifier newtypes.

use std::fmt;

/// Unique id of a client connection. Assigned by the accept loop.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionId(pub u64);

/// Unique id of a room / map.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RoomId(pub u64);

/// Opaque handle to a player entity inside a room.
///
/// Deliberately a plain `u64` so this crate stays ECS-agnostic: the game
/// crate chooses the identity space behind it. The demo assigns a
/// room-local, never-reused serial at spawn (see gsb-game's `DemoRoom`
/// and `game.proto`, `EntityRecord.entity`).
pub type EntityId = u64;

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "c{}", self.0)
    }
}

impl fmt::Display for RoomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "r{}", self.0)
    }
}

impl RoomId {
    /// A small helper for tests and configs.
    pub fn new(id: u64) -> Self {
        Self(id)
    }
}
