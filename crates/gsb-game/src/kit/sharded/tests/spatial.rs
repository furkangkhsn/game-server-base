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
    let snap = crate::kit::seam::WorldSnapshot::decode(out.as_ref()).expect("snapshot");
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
    let seen2: BTreeSet<u64> = crate::kit::seam::WorldSnapshot::decode(out2.as_ref())
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

/// A client-view accumulator with FULL/DELTA application semantics
/// (upserts, per-entity removals, whole-cell forgets) — what an
/// observer's connection holds after each packet.
#[derive(Default)]
struct ClientView {
    ents: HashMap<u64, (i32, i32)>,
}

impl ClientView {
    fn apply(&mut self, snap: &crate::kit::seam::WorldSnapshot, cell_size: f32) {
        if !snap.delta {
            self.ents.clear();
        }
        for ce in &snap.cell_exits {
            let exited = Cell(ce.x, ce.y);
            self.ents
                .retain(|_, &mut (x, y)| cell_of(x, y, cell_size) != exited);
        }
        for w in &snap.removed {
            self.ents.remove(w);
        }
        for e in &snap.entities {
            self.ents.insert(e.entity, (e.x, e.y));
        }
    }
}

/// Seam continuity: an observer beside the border sees the neighbor's
/// strip entities CONTINUOUSLY while they move on the other side —
/// every tick's applied view carries the mover at its CURRENT
/// truncated position (the accepted one-tick alignment blink is not
/// observable here because the exporter rebuilds its cache before the
/// exchange and the receiver integrates before its broadcast).
#[test]
fn borrowed_border_entities_render_without_gap_across_seam() {
    let mut w0 = World::new();
    let mut w1 = World::new();
    let mut s0 = ShardedRoom::new(0, 2, 50.0);
    let mut s1 = ShardedSpatialRoom::new(1, 2, 50.0, 20.0);
    let m = place(&mut w0, &mut s0, ConnectionId(1), -1.0, -10.0);
    let _o = place_spatial(&mut w1, &mut s1, ConnectionId(2), 5.0, -10.0);

    // The neighbor mover walks along the seam INSIDE its own region
    // (no migration), staying inside the observer shard's frame.
    let walk = [
        (-1.0f32, -10.0f32),
        (-3.0, -12.0),
        (-6.0, -14.0),
        (-9.0, -11.0),
    ];
    let mut view = ClientView::default();
    for (t, pos) in walk.iter().enumerate() {
        let tick = t as u64 + 1;
        let entity = *s0.player_entity.get(&PlayerId(1)).unwrap();
        w0.entity_mut(entity)
            .insert(Position { x: pos.0, y: pos.1 });

        // Mirror the actor order on both shards: update → export →
        // update → broadcast-with-borrowed.
        s0.update(&mut w0, &ctx(tick));
        let borrowed: Vec<BorderRecord<StripPos>> = s0.collect_border(&w0);
        s1.update(&mut w1, &ctx(tick));
        let mut out = bytes::BytesMut::new();
        assert!(
            s1.snapshot(&mut w1, &ctx(tick), &Cell(0, -1), &borrowed, &mut out),
            "tick {tick}: the observer's group emits"
        );
        let snap = crate::kit::seam::WorldSnapshot::decode(out.as_ref()).expect("snapshot");
        view.apply(&snap, 20.0);
        assert_eq!(
            view.ents.get(&m).copied(),
            Some((pos.0 as i32, pos.1 as i32)),
            "tick {tick}: the seam entity renders at its current position                  (no gap, no stale ghost)"
        );
    }
}

/// THE EVAPORATION GUARD (module docs, "THE borrowed-strip ×
/// delta-ledger subtlety"): the borrowed slice arrives full every
/// tick, yet STATIC strip records must not dirty any group — no
/// upserts shipped tick after tick — while a genuine strip move still
/// ships exactly that one record.
#[test]
fn delta_bookkeeping_ignores_unchanged_borrowed_strip() {
    let mut w0 = World::new();
    let mut w1 = World::new();
    let mut s0 = ShardedRoom::new(0, 2, 50.0);
    let mut s1 = ShardedSpatialRoom::new(1, 2, 50.0, 20.0);
    let p = place(&mut w0, &mut s0, ConnectionId(1), -1.0, -10.0); // Cell(-1,-1)
    let q = place(&mut w0, &mut s0, ConnectionId(2), -3.0, 15.0); // Cell(-1,0)
    let _o = place_spatial(&mut w1, &mut s1, ConnectionId(3), 5.0, -10.0); // Cell(0,-1)

    // Tick 1: first contact — everything enters once.
    s0.update(&mut w0, &ctx(1));
    let borrowed: Vec<BorderRecord<StripPos>> = s0.collect_border(&w0);
    s1.update(&mut w1, &ctx(1));
    let mut out = bytes::BytesMut::new();
    assert!(s1.snapshot(&mut w1, &ctx(1), &Cell(0, -1), &borrowed, &mut out));
    let seen: BTreeSet<u64> = crate::kit::seam::WorldSnapshot::decode(out.as_ref())
        .expect("snapshot")
        .entities
        .iter()
        .map(|e| e.entity)
        .collect();
    assert!(
        seen.contains(&p) && seen.contains(&q),
        "strip baselined once: {seen:?}"
    );

    // Ticks 2–3: the SAME slice arrives (full replacement every tick
    // — exactly the shape the naive port chokes on): silence, zero
    // encoded records.
    for tick in [2u64, 3] {
        s0.update(&mut w0, &ctx(tick));
        let borrowed: Vec<BorderRecord<StripPos>> = s0.collect_border(&w0);
        s1.update(&mut w1, &ctx(tick));
        let mut out = bytes::BytesMut::new();
        assert!(
            !s1.snapshot(&mut w1, &ctx(tick), &Cell(0, -1), &borrowed, &mut out),
            "tick {tick}: unchanged strip ⇒ silent"
        );
        assert!(out.is_empty(), "tick {tick}: no bytes at all");
        assert_eq!(
            s1.encoded_records(),
            0,
            "tick {tick}: NO upserts shipped for unchanged borrowed records"
        );
    }

    // Tick 4: one strip record moves — exactly that record ships.
    let entity_p = *s0.player_entity.get(&PlayerId(1)).unwrap();
    w0.entity_mut(entity_p)
        .insert(Position { x: -5.0, y: -10.0 }); // same cell
    s0.update(&mut w0, &ctx(4));
    let borrowed: Vec<BorderRecord<StripPos>> = s0.collect_border(&w0);
    s1.update(&mut w1, &ctx(4));
    let mut out = bytes::BytesMut::new();
    assert!(s1.snapshot(&mut w1, &ctx(4), &Cell(0, -1), &borrowed, &mut out));
    let snap = crate::kit::seam::WorldSnapshot::decode(out.as_ref()).expect("snapshot");
    assert!(snap.delta);
    assert_eq!(
        snap.entities.len(),
        1,
        "only the mover re-carried: {snap:?}"
    );
    assert_eq!(snap.entities[0].entity, p);
    assert_eq!((snap.entities[0].x, snap.entities[0].y), (-5, -10));
    assert!(
        snap.entities.iter().all(|e| e.entity != q),
        "static q untouched"
    );
    assert_eq!(s1.encoded_records(), 1);
}

/// Migration correctness (module docs, "Migration correctness"): a
/// player migrating INTO an established mid-cell arrives as a FRESH
/// group member — the very next private frame is the one-shot FULL of
/// their new view (the established group's own packet stayed a delta
/// and could not have baselined them).
#[test]
fn migrated_player_gets_private_full_on_arrival() {
    use crate::kit::seam::private::Payload;

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
            game: DemoMig {
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
    let snap = crate::kit::seam::WorldSnapshot::decode(out.as_ref()).expect("snapshot");
    assert!(snap.delta, "established group stays in delta mode");
    assert!(snap.entities.iter().any(|e| e.entity == arrival_wire));

    // The arrival's private frame for ITS cell: the one-shot FULL.
    let mut pbuf = bytes::BytesMut::new();
    assert!(
        s1.private(&mut w1, PlayerId(9), &Cell(0, -1), &[], &mut pbuf),
        "the arrival receives the one-shot private full"
    );
    let frame = crate::kit::seam::Private::decode(pbuf.as_ref()).expect("private frame");
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
