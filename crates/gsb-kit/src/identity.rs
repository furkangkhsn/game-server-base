//! The kit-owned wire identity and its single minting point
//! (KIT-ARCHITECTURE §4.4: identity and its minting belong to the kit,
//! not to the game).
//!
//! **The invariant is structural.** [`WireId`]'s field is private to
//! THIS module and the type has no constructor at all: the only code
//! that can build a `WireId` is [`Minter`], defined below. A room owns
//! exactly one minter; every identity it stamps — the joiner's entity
//! (its value also goes to the joiner in `JOIN_ROOM_RESULT`), every
//! orphan the broadcast set picks up — is drawn from it. Nothing else in
//! this crate, and nothing in any other crate, can fabricate a `WireId`
//! that collides with a minter's space: the compiler, not a test, is the
//! guard (§8.1: the sharded rooms' three direct constructor calls now go
//! through the minter too).
//!
//! The minter itself is kit-internal (`pub(super)`: the `kit` module): a
//! game never mints — it spawns an entity carrying the codec's marker
//! component and the kit stamps the identity.

use bevy_ecs::prelude::Component;

/// The entity's wire identity (see `game.proto`, `EntityRecord.entity`).
///
/// The field is **private and there is no `Default` and no constructor
/// on purpose**: the identity invariant ("two different entities cannot
/// share the same wire identity over the room's lifetime") has exactly
/// one legitimate source — the room's [`Minter`] — and the type says so.
/// Reading is open: [`WireId::get`].
///
/// Outside the identity module the type cannot be built — this is the
/// compile-level lock:
///
/// ```compile_fail
/// let forged = gsb_kit::identity::WireId(42);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Component)]
pub struct WireId(u64);

impl WireId {
    /// Read the serial (the wire side).
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A room's identity counter — the ONLY construction path of
/// [`WireId`] (module docs). Monotonic: a value is never re-used within
/// the room's lifetime, even when the ECS allocator recycles the old
/// entity's slot. Both shapes share one arithmetic: the `n`-th draw
/// (from 1) is `base + n`.
#[derive(Debug, Clone)]
pub(super) enum Minter {
    /// A single-world room: serials 1, 2, 3, … (`base = 0`).
    Sequential { used: u64 },
    /// One shard of a sharded room: serials `base + 1, base + 2, …` from
    /// the shard's disjoint slice of the id space (range partitioning —
    /// two shards never mint the same value). The core's
    /// range-exhaustion guard reads [`Self::used`].
    Range { base: u64, used: u64 },
}

impl Minter {
    /// A fresh single-world counter.
    pub(super) const fn sequential() -> Self {
        Self::Sequential { used: 0 }
    }

    /// A fresh counter over the slice that starts after `base`.
    pub(super) const fn range(base: u64) -> Self {
        Self::Range { base, used: 0 }
    }

    /// Mint the next wire identity.
    #[inline]
    pub(super) fn mint(&mut self) -> WireId {
        WireId(self.next_serial())
    }

    /// Draw the next raw serial from the SAME counter without making it
    /// a wire identity — for an identity space that shares the counter
    /// (a shard mints its stable player ids from its range too, so the
    /// core's exhaustion guard stays exact over everything the range
    /// backs). A raw serial never becomes a `WireId`: only
    /// [`Self::mint`] and [`Self::arrival`] build one.
    #[inline]
    pub(super) fn next_serial(&mut self) -> u64 {
        match self {
            Self::Sequential { used } => {
                *used += 1;
                *used
            }
            Self::Range { base, used } => {
                *used += 1;
                *base + *used
            }
        }
    }

    /// How many serials this counter has drawn.
    pub(super) const fn used(&self) -> u64 {
        match self {
            Self::Sequential { used } | Self::Range { used, .. } => *used,
        }
    }

    /// Re-materialize the identity of an entity that migrated IN from a
    /// sibling shard. Not a mint: the value was minted by the sibling's
    /// range (the core carries it as a raw `u64` in `Migrating::wire`),
    /// and the entity keeps it for its whole lifetime. Only a range
    /// minter receives migrants.
    pub(super) fn arrival(&self, wire: u64) -> WireId {
        debug_assert!(
            matches!(self, Self::Range { .. }),
            "only a shard (range minter) receives migrants"
        );
        WireId(wire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequential_counts_from_one() {
        let mut m = Minter::sequential();
        assert_eq!(m.mint().get(), 1);
        assert_eq!(m.mint().get(), 2);
        assert_eq!(m.used(), 2);
    }

    /// Wire ids and raw serials share one counter (a shard's player
    /// ids): interleaved draws never repeat a value, and the count
    /// covers both.
    #[test]
    fn range_draws_share_one_counter() {
        let mut m = Minter::range(1_000);
        assert_eq!(m.next_serial(), 1_001);
        assert_eq!(m.mint().get(), 1_002);
        assert_eq!(m.next_serial(), 1_003);
        assert_eq!(m.used(), 3);
        assert_eq!(m.arrival(7).get(), 7);
    }
}
