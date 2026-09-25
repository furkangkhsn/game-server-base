//! The kit-owned wire identity and its single minting point
//! (KIT-ARCHITECTURE §4.4: identity and its minting belong to the kit,
//! not to the game).
//!
//! **The invariant is structural.** [`WireId`]'s field is private to
//! THIS module and the type has no constructor at all: the only code
//! that can build a `WireId` is `Minter`, defined below. A room owns
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
use gsb_core::shard::interleaved_id;

/// The entity's wire identity (see `game.proto`, `EntityRecord.entity`).
///
/// The field is **private and there is no `Default` and no constructor
/// on purpose**: the identity invariant ("two different entities cannot
/// share the same wire identity over the room's lifetime") has exactly
/// one legitimate source — the room's `Minter` — and the type says so.
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
/// entity's slot. A single world counts 1, 2, 3, …; a shard of an
/// `N`-shard room draws the `n`-th value of its own residue class
/// ([`interleaved_id`]: `(n − 1) · N + index + 1`) — with `N = 1` the
/// same 1, 2, 3, ….
#[derive(Debug, Clone)]
pub(super) enum Minter {
    /// A single-world room: serials 1, 2, 3, ….
    Sequential { used: u64 },
    /// Shard `index` of an `shards`-shard room: its draws interleave
    /// with its siblings' (two shards never draw the same value, and
    /// the room's values stay small — KIT-ARCHITECTURE §4.4). The
    /// core's exhaustion guard reads [`Self::used`].
    Interleaved {
        index: usize,
        shards: usize,
        used: u64,
    },
}

impl Minter {
    /// A fresh single-world counter.
    pub(super) const fn sequential() -> Self {
        Self::Sequential { used: 0 }
    }

    /// A fresh counter for shard `index` of a room of `shards` shards
    /// (fixed for the room's incarnation: the partition's).
    pub(super) const fn interleaved(index: usize, shards: usize) -> Self {
        assert!(index < shards, "a shard index lies below the shard count");
        Self::Interleaved {
            index,
            shards,
            used: 0,
        }
    }

    /// Mint the next wire identity.
    #[inline]
    pub(super) fn mint(&mut self) -> WireId {
        WireId(self.next_serial())
    }

    /// Draw the next raw serial from the SAME counter without making it
    /// a wire identity — for an identity space that shares the counter
    /// (a shard mints its stable player ids from it too, so the core's
    /// exhaustion guard stays exact over everything the counter backs).
    /// A raw serial never becomes a `WireId`: only [`Self::mint`] and
    /// [`Self::arrival`] build one.
    #[inline]
    pub(super) fn next_serial(&mut self) -> u64 {
        match self {
            Self::Sequential { used } => {
                *used += 1;
                *used
            }
            Self::Interleaved {
                index,
                shards,
                used,
            } => {
                *used += 1;
                interleaved_id(*index, *shards, *used)
            }
        }
    }

    /// How many serials this counter has drawn.
    pub(super) const fn used(&self) -> u64 {
        match self {
            Self::Sequential { used } | Self::Interleaved { used, .. } => *used,
        }
    }

    /// Re-materialize the identity of an entity that migrated IN from a
    /// sibling shard. Not a mint: the value was minted by the sibling
    /// (the core carries it as a raw `u64` in `Migrating::wire`), and the
    /// entity keeps it for its whole lifetime. Only a shard's counter
    /// receives migrants.
    pub(super) fn arrival(&self, wire: u64) -> WireId {
        debug_assert!(
            matches!(self, Self::Interleaved { .. }),
            "only a shard (interleaved minter) receives migrants"
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
    fn shard_draws_share_one_counter() {
        let mut m = Minter::interleaved(2, 4);
        assert_eq!(m.next_serial(), 3);
        assert_eq!(m.mint().get(), 7);
        assert_eq!(m.next_serial(), 11);
        assert_eq!(m.used(), 3);
        assert_eq!(m.arrival(5).get(), 5);
    }

    /// A one-shard room counts like a single world.
    #[test]
    fn a_single_shard_counts_like_a_single_world() {
        let (mut one, mut seq) = (Minter::interleaved(0, 1), Minter::sequential());
        for _ in 0..50 {
            assert_eq!(one.mint(), seq.mint());
        }
    }
}
