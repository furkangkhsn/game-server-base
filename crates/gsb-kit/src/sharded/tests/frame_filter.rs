//! The receiver-side frame filter ([`Partition::admits`]) on the spatial
//! composite: a neighbour exports its WHOLE border, and the parts of it
//! far from this shard are not this shard's content — on the plain
//! [`ShardedRoom`] and on [`ShardedSpatialRoom`] alike.
//!
//! Geometry (4×4, half 50 — regions 25 wide, border margin 6.25; cells
//! of 20): shard 0 owns x, y ∈ [-50, -25), its east neighbour shard 1
//! owns x ∈ [-25, 0). Shard 0 admits wire x ≤ -18.75. An observer of
//! shard 0 at x = -30 sits in Cell(-2,-2), whose 3×3 reaches Cell(-1,·)
//! — wire x up to -1, i.e. shard 1's FAR (east) edge.

use super::lent_arrival::{full_view, tick};
use super::*;

/// Shard 1's strip: an entity by the seam with shard 0 (x = -24) and
/// one by shard 1's east edge (x = -1), both at y = -37.5.
fn shard1_strip() -> (Vec<BorderRecord<WirePos>>, u64, u64) {
    let mut w1 = World::new();
    let mut s1 = ShardedRoom::new(1, 16, 50.0);
    let seam = place(&mut w1, &mut s1, ConnectionId(1), -24.0, -37.5);
    let east = place(&mut w1, &mut s1, ConnectionId(2), -1.0, -37.5);
    s1.update(&mut w1, &ctx(1));
    let border = s1.collect_border(&w1);
    let wires: BTreeSet<u64> = border.iter().map(|r| r.wire).collect();
    assert_eq!(wires, BTreeSet::from([seam, east]), "both are exported");
    (border, seam, east)
}

/// The far edge of the neighbour's export is filtered on both rooms: the
/// plain room's single group and the composite's cell group whose 3×3
/// would otherwise reach it hold the seam entity only.
#[test]
fn the_composite_applies_the_frame_filter_like_the_plain_room() {
    let (border, seam, east) = shard1_strip();

    let mut w0 = World::new();
    let mut plain = ShardedRoom::new(0, 16, 50.0);
    let mut out = bytes::BytesMut::new();
    assert!(plain.snapshot(&mut w0, &ctx(1), &(), &border, &mut out));
    assert_eq!(snap_ids(&out), BTreeSet::from([seam]), "the plain room");

    let mut w0 = World::new();
    let mut sp0 = ShardedSpatialRoom::new(0, 16, 50.0, 20.0);
    let o = place_spatial(&mut w0, &mut sp0, ConnectionId(3), -30.0, -37.5); // Cell(-2,-2)
    let [Some(first)] = &tick(&mut w0, &mut sp0, 1, &[Cell(-2, -2)], &border)[..] else {
        panic!("a fresh group emits")
    };
    let seen: BTreeSet<u64> = first.entities.iter().map(|e| e.entity).collect();
    assert!(!seen.contains(&east), "the far edge is filtered: {seen:?}");
    assert_eq!(
        seen,
        BTreeSet::from([o, seam]),
        "the composite's first full"
    );
    let mut view = full_view(&mut w0, &mut sp0, 1, Cell(-2, -2));
    view.sort_unstable();
    let mut want = vec![(o, -30), (seam, -24)];
    want.sort_unstable();
    assert_eq!(view, want, "the composite's keep-alive full");
    assert!(
        !sp0.book.buckets.contains_key(&Cell(-1, -2)),
        "the far record is not in the cell book"
    );
}

/// A lent record walking out of the admitted frame leaves the view (an
/// exit, not a move), and walking back in re-enters it — the ledger
/// diffs the FILTERED strip, so the frame edge reads like the strip's
/// own edge.
#[test]
fn a_record_crossing_the_frame_edge_exits_and_reenters() {
    let mut w0 = World::new();
    let mut sp0 = ShardedSpatialRoom::new(0, 16, 50.0, 20.0);
    let o = place_spatial(&mut w0, &mut sp0, ConnectionId(1), -30.0, -37.5); // Cell(-2,-2)
    let group = [Cell(-2, -2)];
    let n = interleaved_id(1, 16, 7); // shard 1's
    let at = |x| {
        vec![BorderRecord {
            wire: n,
            state: WirePos { x, y: -37 },
        }]
    };

    tick(&mut w0, &mut sp0, 1, &group, &at(-20));
    assert!(full_view(&mut w0, &mut sp0, 1, group[0]).contains(&(n, -20)));

    // x = -15: past shard 0's margin (-18.75), still inside the 3×3.
    let delta = tick(&mut w0, &mut sp0, 2, &group, &at(-15)).remove(0);
    let delta = delta.expect("leaving the frame is news for the group");
    assert!(delta.delta, "an established group stays in delta mode");
    assert!(
        !delta.entities.iter().any(|e| e.entity == n),
        "no upsert of a record outside the frame: {delta:?}"
    );
    assert!(
        delta.removed.contains(&n) || !delta.cell_exits.is_empty(),
        "the record leaves the view: {delta:?}"
    );
    assert_eq!(full_view(&mut w0, &mut sp0, 2, group[0]), vec![(o, -30)]);

    // Still outside and moving: silent.
    assert_eq!(tick(&mut w0, &mut sp0, 3, &group, &at(-10)), vec![None]);

    // Back inside the frame: an entry again.
    let delta = tick(&mut w0, &mut sp0, 4, &group, &at(-19)).remove(0);
    let delta = delta.expect("re-entering the frame is news");
    assert!(delta.entities.iter().any(|e| (e.entity, e.x) == (n, -19)));
    let mut view = full_view(&mut w0, &mut sp0, 4, group[0]);
    view.sort_unstable();
    let mut want = vec![(n, -19), (o, -30)];
    want.sort_unstable();
    assert_eq!(view, want);
}
