//! Identifier newtypes.

use std::fmt;

/// Unique id of a client connection. Assigned by the accept loop.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionId(pub u64);

/// Stable identity of a PLAYER inside a room (Faz 2,
/// `docs/TRAIT-ARCHITECTURE.md` §5). Minted by the GAME LOGIC at a
/// session's first join — identity policy is the game's job; core never
/// invents player ids. Unlike [`ConnectionId`] (which is the
/// transport-session key and changes on every reconnect), this id is
/// stable across resume and shard migration: it keys every room/shard
/// internal table (`conns`, the READ roster), so a resume re-points ONE
/// binding row instead of re-keying N tables (`docs/RECONNECT.md`
/// §14.1's radical fix).
///
/// Deliberately NOT `Default`: there is no meaningful "zeroth player" —
/// a default would invite accidentally keying state under an identity
/// nobody minted.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlayerId(pub u64);

impl PlayerId {
    /// A small helper for tests and configs (same role as
    /// [`RoomId::new`]): wraps a raw value without implying a minting
    /// policy.
    pub fn new(id: u64) -> Self {
        Self(id)
    }
}

/// Unique id of a room / map.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RoomId(pub u64);

/// Opaque handle to a player entity inside a room.
///
/// Deliberately a plain `u64` so this crate stays ECS-agnostic: the game
/// crate chooses the identity space behind it. The demo assigns a
/// room-local, never-reused serial at spawn (see gsb-game's `OpenRoom`
/// and `game.proto`, `EntityRecord.entity`).
pub type EntityId = u64;

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "c{}", self.0)
    }
}

impl fmt::Display for PlayerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "p{}", self.0)
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
