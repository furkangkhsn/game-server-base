//! [`Owner`] — the core's name for "the thing a record belongs to".

/// An opaque owner id: what a binding's entity becomes in the core
/// (bevy_ecs: `Entity::to_bits`, hecs: `Entity::to_bits`, shipyard:
/// `EntityId::inner`, no ECS: the game's own id). The core only hashes
/// and compares it; it never turns it back into an entity — only the
/// binding that made it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Owner(pub u64);
