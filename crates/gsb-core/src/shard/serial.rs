//! The wire-identity arithmetic of a sharded room (parent docs, "Wire
//! identity"): which value a shard's `n`-th draw is, which shard drew a
//! value, and how many draws a shard may make in one incarnation. The
//! kit's minter draws through [`interleaved_id`], and so does a test
//! stub that mints for itself: there is ONE formula.
//!
//! ## What the engine needs from wire ids
//!
//! 1. **Unique within a room incarnation**, across shards too: a
//!    client's view unions its shard's records with borrowed ones, and
//!    the snapshot ledgers are maps over wire ids.
//! 2. **Never re-used within an incarnation**, even after a despawn: a
//!    remote effect's target identity is `(wire, epoch)` with the room
//!    incarnation's epoch (`docs/CROSS-SHARD.md` §4b), so a re-drawn id
//!    would let a stale effect land on a newcomer.
//! 3. **Preserved across migration**: the id travels in `Migrating::wire`
//!    and the receiving shard re-materializes it (not a draw).
//! 4. **Minted without coordination**: no lock, no message, no await —
//!    spawns happen inside the synchronous tick body, and the shards
//!    share no state.
//! 5. **Deterministically ordered** where the engine orders by wire id
//!    (crystallization's lower/higher rule, the team views' wire order):
//!    plain `u64` comparison, which any unique scheme keeps.
//!
//! [`interleaved_id`] keeps all five with only `(index, N)` — both fixed
//! for the incarnation (the partition's; a rebuild is a new incarnation
//! with fresh counters) — and makes the ids compact: while every shard
//! has drawn `n`, the room has used exactly `1 ..= n·N`. The drawing
//! shard is `(id − 1) mod N` ([`minting_shard`]); no engine path needs
//! it (effect routing follows the lender and the forwarding table,
//! crystallization's anchor is the lender), only tests read it.
//!
//! ## The bound
//!
//! A shard may draw [`SHARD_SERIAL_CAPACITY`] values per incarnation;
//! the join guard refuses (`RoomFull`, logged) the join that would pass
//! the logic's `serial_capacity()`. Uniqueness does not rest on it (the
//! classes are disjoint for any count — the old ranges needed it); it
//! bounds every id at `2^20 · N`, the old ranges' ceiling, and keeps the
//! same error path. Orphan stamping (a game-spawned NPC) is not gated,
//! as before: past the bound it keeps drawing unique values of its class
//! (`u64` overflow lies ~2^62 draws away at N = 4).
//!
//! ## Rejected alternatives
//!
//! - **Narrower ranges** (`i · 2^k` with a small `k`): still a fixed
//!   offset per shard (shard 3 of 4 starts at `3 · 2^k`), and a small `k`
//!   makes exhaustion reachable — interleaving gives the smallest values
//!   with the same bound.
//! - **`n · N + i`** (serials from 1): the values `1 .. N` would never
//!   be drawn and shard 0 would start at `N`; the `+ 1` form is dense
//!   from 1 (and keeps shard 0's first value at 1, as before).
//! - **A registry-issued allocator** (a round trip per spawn, or batches
//!   of ids): coordination — an await in the tick body or a shared
//!   component in a shared-state-free design; a pre-fetched batch is
//!   range partitioning with extra steps.
//! - **Per-connection id remapping on the wire** (each client sees small
//!   ids of its own): breaks encode-once — a group frame and a shared
//!   cell piece are encoded once for every member.
//! - **Separate counters for player ids and wire ids** (a join would
//!   draw one wire value, not two): kept shared so the exhaustion guard
//!   counts one number; the halving it would buy is a separate choice.

/// Serials one shard may draw in one room incarnation — the exhaustion
/// bound the join guard enforces (module docs, "Wire identity"): 2^20 ≈
/// 100× the measured 10k single-room wall in per-shard lifetime spawn
/// churn. Every value a room mints under it is at most
/// `SHARD_SERIAL_CAPACITY × shard count`.
pub const SHARD_SERIAL_CAPACITY: u64 = 1 << 20;

/// The value of shard `index`'s `n`-th draw (`n` from 1) in a room of
/// `shards` shards: `(n − 1) · shards + index + 1` — the `n`-th member
/// of the residue class `index + 1 (mod shards)`. The classes of
/// different shards are disjoint whatever their counts, a class is
/// strictly increasing in `n`, the first `n` draws of all shards are
/// exactly `1 ..= n · shards`, and one shard counts 1, 2, 3, ….
#[inline]
pub const fn interleaved_id(index: usize, shards: usize, n: u64) -> u64 {
    debug_assert!(index < shards && n >= 1);
    (n - 1) * shards as u64 + index as u64 + 1
}

/// The shard that drew `value` (a value [`interleaved_id`] produced for
/// the same `shards`): `(value − 1) mod shards`.
#[inline]
pub const fn minting_shard(value: u64, shards: usize) -> usize {
    debug_assert!(value >= 1 && shards >= 1);
    ((value - 1) % shards as u64) as usize
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// Uniqueness without coordination: shards of the same room never
    /// draw the same value, whatever their counts, and every value
    /// names the shard that drew it.
    #[test]
    fn shards_never_draw_the_same_value() {
        for shards in 1..=9 {
            let mut seen = HashSet::new();
            for index in 0..shards {
                for n in 1..=300 {
                    let v = interleaved_id(index, shards, n);
                    assert!(seen.insert(v), "{v} drawn twice (N={shards})");
                    assert_eq!(minting_shard(v, shards), index, "{v} (N={shards})");
                }
            }
        }
    }

    /// Compact: the first `n` draws of every shard are exactly the
    /// values `1 ..= n·N` — small counts give small values (1-byte
    /// varints while `n·N < 128`), and a single-shard room counts 1, 2,
    /// 3, … like a single room.
    #[test]
    fn the_first_draws_of_all_shards_are_dense() {
        for shards in 1..=9usize {
            let n = 40;
            let mut all: Vec<u64> = (0..shards)
                .flat_map(|k| (1..=n).map(move |s| interleaved_id(k, shards, s)))
                .collect();
            all.sort_unstable();
            let dense: Vec<u64> = (1..=n * shards as u64).collect();
            assert_eq!(all, dense, "N={shards}");
        }
        assert_eq!(
            (1..=5).map(|n| interleaved_id(0, 1, n)).collect::<Vec<_>>(),
            [1, 2, 3, 4, 5]
        );
    }

    /// The exhaustion bound: the last draw the join guard admits stays
    /// at `capacity × N` (the old ranges' ceiling), and the draws are
    /// increasing per shard (a value is never re-drawn).
    #[test]
    fn the_capacity_bounds_every_value() {
        for shards in [1usize, 2, 4, 16] {
            let top = (0..shards)
                .map(|k| interleaved_id(k, shards, SHARD_SERIAL_CAPACITY))
                .max()
                .unwrap();
            assert_eq!(top, SHARD_SERIAL_CAPACITY * shards as u64);
            for k in 0..shards {
                assert!(interleaved_id(k, shards, 2) > interleaved_id(k, shards, 1));
            }
        }
    }
}
