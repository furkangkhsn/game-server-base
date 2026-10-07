//! [`Owner`]: what a piece of game state belongs to, opaque to the
//! engine — shared by the call table ([`crate::calls`], whose results
//! and sessions are keyed by it) and the ECS-agnostic kit
//! (`docs/KIT-ECS.md`), so the same key travels between them.

use crate::id::PlayerId;

/// What a call (or its result, or kit state) belongs to, opaque to the
/// engine: an entity's bits, a player id, anything the game can test for
/// life ([`GameLogic::owner_alive`](crate::room::GameLogic::owner_alive)).
/// The engine's default owner of a session is its player
/// (`Owner::from(PlayerId)`). ECS adapters convert their entity ids
/// through [`Self::from_bits`] / [`Self::to_bits`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Owner(pub u64);

impl Owner {
    /// "The actor itself": a call without `.owner(..)`; always alive.
    pub const ACTOR: Owner = Owner(u64::MAX);

    /// An owner from its raw bits (an entity's `to_bits`, a game id).
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    /// The owner's raw bits.
    pub const fn to_bits(self) -> u64 {
        self.0
    }
}

impl From<PlayerId> for Owner {
    fn from(p: PlayerId) -> Self {
        Owner(p.0)
    }
}
