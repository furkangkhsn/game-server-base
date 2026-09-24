//! The Faz B composite: per-shard cell-grouped broadcast over the
//! shard's own region.

use super::*;

/// Cells group members by position within ONE shard: a group sees its
/// own cell plus the 3×3 ring — co-located and adjacent residents in,
/// far cells out — and fresh groups open with a full packet.
#[test]
fn sharded_spatial_cells_group_members_by_position() {
    let mut world = World::new();
    let mut s1 = ShardedSpatialRoom::new(1, 2, 50.0, 20.0);
    let a = place_spatial(&mut world, &mut s1, ConnectionId(1), 5.0, -10.0); // Cell(0,-1)
    let b = place_spatial(&mut world, &mut s1, ConnectionId(2), 25.0, -10.0); // Cell(1,-1)
    let c = place_spatial(&mut world, &mut s1, ConnectionId(3), 45.0, -10.0); // Cell(2,-1)
    s1.update(&mut world, &ctx(1));

    let mut out = bytes::BytesMut::new();
    assert!(
        s1.snapshot(&mut world, &ctx(1), &Cell(0, -1), &[], &mut out),
        "A's group emits (fresh)"
    );
    let snap = crate::testing::WorldSnapshot::decode(out.as_ref()).expect("snapshot");
    assert!(!snap.delta, "a fresh group's first packet is a full");
    let seen: BTreeSet<u64> = snap.entities.iter().map(|e| e.entity).collect();
    assert!(
        seen.contains(&a) && seen.contains(&b),
        "own + adjacent visible: {seen:?}"
    );
    assert!(
        !seen.contains(&c),
        "two cells away is outside the 3×3: {seen:?}"
    );

    let mut out2 = bytes::BytesMut::new();
    assert!(s1.snapshot(&mut world, &ctx(1), &Cell(2, -1), &[], &mut out2));
    let seen2: BTreeSet<u64> = crate::testing::WorldSnapshot::decode(out2.as_ref())
        .expect("snapshot")
        .entities
        .iter()
        .map(|e| e.entity)
        .collect();
    assert!(
        seen2.contains(&c) && seen2.contains(&b),
        "C's group mirrors: {seen2:?}"
    );
    assert!(
        !seen2.contains(&a),
        "far member not leaked across cells: {seen2:?}"
    );
}

/// Migration correctness (module docs, "Migration correctness"): a
/// player migrating INTO an established mid-cell arrives as a FRESH
/// group member — the very next private frame is the one-shot FULL of
/// their new view (the established group's own packet stayed a delta
/// and could not have baselined them).
#[test]
fn migrated_player_gets_private_full_on_arrival() {
    use crate::testing::private::Payload;

    let mut w1 = World::new();
    let mut s1 = ShardedSpatialRoom::new(1, 2, 50.0, 20.0);
    let r = place_spatial(&mut w1, &mut s1, ConnectionId(1), 25.0, -10.0); // Cell(1,-1)
    s1.update(&mut w1, &ctx(1));
    let mut out = bytes::BytesMut::new();
    assert!(
        s1.snapshot(&mut w1, &ctx(1), &Cell(1, -1), &[], &mut out),
        "resident establishes the group"
    );

    // An arrival from the west shard into Cell(0,-1) — mid-cell, an
    // already-established neighborhood (its cell sits inside the
    // resident group's 3×3).
    let arrival_wire = 42_u64; // any id from another range (test-only)
    s1.on_migrate_in(
        &mut w1,
        arrival_wire,
        KitMig {
            game: FixMig {
                pos: Position { x: 5.0, y: -10.0 },
                speed: Some(DEFAULT_SPEED),
                target: None,
            },
            park: None,
        },
        Some(PlayerId(9)),
    );
    s1.update(&mut w1, &ctx(2));

    // The ESTABLISHED resident group's packet is a delta carrying the
    // arrival's upsert — it does NOT baseline the arrival.
    out.clear();
    assert!(s1.snapshot(&mut w1, &ctx(2), &Cell(1, -1), &[], &mut out));
    let snap = crate::testing::WorldSnapshot::decode(out.as_ref()).expect("snapshot");
    assert!(snap.delta, "established group stays in delta mode");
    assert!(snap.entities.iter().any(|e| e.entity == arrival_wire));

    // The arrival's private frame for ITS cell: the one-shot FULL.
    let mut pbuf = bytes::BytesMut::new();
    assert!(
        s1.private(&mut w1, PlayerId(9), &Cell(0, -1), &[], &mut pbuf),
        "the arrival receives the one-shot private full"
    );
    let frame = crate::testing::Private::decode(pbuf.as_ref()).expect("private frame");
    let full = match frame.payload {
        Some(Payload::Snapshot(s)) => s,
        other => panic!("expected the snapshot oneof, got {other:?}"),
    };
    assert!(!full.delta, "the one-shot is a FULL");
    let seen: BTreeSet<u64> = full.entities.iter().map(|e| e.entity).collect();
    assert!(
        seen.contains(&arrival_wire) && seen.contains(&r),
        "the arrival sees itself AND the local resident immediately: {seen:?}"
    );

    // One-shot means one-shot.
    let mut pbuf2 = bytes::BytesMut::new();
    assert!(
        !s1.private(&mut w1, PlayerId(9), &Cell(0, -1), &[], &mut pbuf2),
        "the second private call ships nothing further"
    );
}
