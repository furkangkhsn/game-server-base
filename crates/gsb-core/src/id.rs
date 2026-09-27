//! Identifier newtypes.

use std::fmt;

/// Unique id of a client connection. Assigned by the accept loop.
///
/// The top bit is reserved: the accept loop mints densely from 1 and
/// never reaches it, and the core uses it for a PARK KEY — the session
/// id a parked entity is re-keyed to when its still-live session leaves
/// it behind (see [`Self::park_key`]).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionId(pub u64);

/// The reserved top bit of a [`ConnectionId`]: set = a park key.
const PARK_BIT: u64 = 1 << 63;

impl ConnectionId {
    /// The park key of this connection: the session id a room re-keys a
    /// member's PARKED row to when the room ends the membership on its
    /// own while the connection stays open — the input-idle ceiling
    /// under `afk_action = leave_room` (BACKLOG B40,
    /// `docs/RECONNECT.md` §16).
    ///
    /// From then on the park is exactly what a transport death leaves
    /// behind — a parked row whose session is gone — only under this key
    /// instead of the live connection's own id, so nothing the live
    /// connection does next (a leave, a fresh join, its own close) can
    /// reach the park, and the registry's row for the park and its row
    /// for the connection are two rows. Deterministic (no shared counter
    /// anywhere): one park per key per room is enforced where the key is
    /// taken.
    pub fn park_key(self) -> Self {
        Self(self.0 | PARK_BIT)
    }

    /// Whether this id is a [`Self::park_key`] (never a live connection).
    pub fn is_park_key(self) -> bool {
        self.0 & PARK_BIT != 0
    }
}

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
        if self.is_park_key() {
            // The session that left the park behind, readable in logs.
            write!(f, "c{}+park", self.0 & !PARK_BIT)
        } else {
            write!(f, "c{}", self.0)
        }
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
