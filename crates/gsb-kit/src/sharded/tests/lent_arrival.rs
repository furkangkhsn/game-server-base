//! An entity crossing a seam that the receiving shard was already
//! showing through the border strip (KIT-ARCHITECTURE §10, F1): the
//! strip ledger's exit for the lent copy must never erase the OWN
//! record the arrival just placed — and a stale lent copy in another
//! cell, or a departed entity coming back through the strip, must end
//! up exactly once in the view.
//!
//! Geometry (2 shards, half 50, cell 20): shard 1 owns x ∈ [0, 50]. The
//! fixture's wire TRUNCATES, so a shard-0 entity at x = -0.5 has wire
//! x = 0 — shard 1's Cell(0, -1) — and shard 1 lends it in there.

use super::*;

/// The lent copy of wire `wire` at wire position `(x, y)`, as the
/// core's flattened border view hands it to the broadcast phase.
fn lent(wire: u64, x: i32, y: i32) -> Vec<BorderRecord<WirePos>> {
    vec![BorderRecord {
        wire,
        state: WirePos { x, y },
    }]
}

/// The migration state of a fixture entity at `(x, y)` (an NPC when
/// `speed` is `None`).
fn arriving(x: f32, y: f32, speed: Option<f32>) -> KitMig<FixMig> {
    KitMig {
        game: FixMig {
            pos: Position { x, y },
            speed,
            target: None,
        },
        park: None,
    }
}

/// One broadcast tick of `room` for the groups `cells` with the strip
/// `borrowed`: `update`, then every group's snapshot (the first one
/// integrates the strip).
fn tick(
    world: &mut World,
    room: &mut ShardedSpatialRoom,
    t: u64,
    cells: &[Cell],
    borrowed: &[BorderRecord<WirePos>],
) -> Vec<Option<crate::testing::WorldSnapshot>> {
    room.update(world, &ctx(t));
    cells
        .iter()
        .map(|c| {
            let mut out = bytes::BytesMut::new();
            room.snapshot(world, &ctx(t), c, borrowed, &mut out)
                .then(|| crate::testing::WorldSnapshot::decode(out.as_ref()).expect("snapshot"))
        })
        .collect()
}

/// The group's current FULL view (its keep-alive), as `(wire, x)`
/// pairs in packet order.
fn full_view(
    world: &mut World,
    room: &mut ShardedSpatialRoom,
    t: u64,
    cell: Cell,
) -> Vec<(u64, i32)> {
    let mut out = bytes::BytesMut::new();
    assert!(room.keepalive(world, &ctx(t), &cell, None, &mut out));
    let full = crate::testing::WorldSnapshot::decode(out.as_ref()).expect("full");
    assert!(!full.delta);
    full.entities.iter().map(|e| (e.entity, e.x)).collect()
}

/// An NPC lent in Cell(0,-1) (wire x = 0, still owned by shard 0)
/// migrates into shard 1 and stops in that same cell, alone there. The
/// observer next door gets no removal and no cell exit for it, and every
/// later full of shard 1 carries it — before the fix the ledger's exit
/// emptied the cell and the NPC stayed invisible until it moved cells.
#[test]
fn an_npc_arriving_in_its_lent_cell_stays_in_that_cell() {
    let mut w1 = World::new();
    let mut s1 = ShardedSpatialRoom::new(1, 2, 50.0, 20.0);
    let o = place_spatial(&mut w1, &mut s1, ConnectionId(1), 25.0, -10.0); // Cell(1,-1)
    let observer = [Cell(1, -1)];
    let npc = 7_u64; // shard 0's range
    tick(&mut w1, &mut s1, 1, &observer, &lent(npc, 0, -10));
    assert!(full_view(&mut w1, &mut s1, 1, Cell(1, -1)).contains(&(npc, 0)));

    // The crossing tick: the own record lands in Cell(0,-1); the core's
    // own-wins filter drops the lent copy from the strip.
    s1.on_migrate_in(&mut w1, npc, arriving(0.5, -10.0, None), None);
    let delta = tick(&mut w1, &mut s1, 2, &observer, &[]).remove(0);
    if let Some(delta) = delta {
        assert!(delta.delta, "an established group stays in delta mode");
        assert!(!delta.removed.contains(&npc), "no removal: {delta:?}");
        assert!(delta.cell_exits.is_empty(), "no cell exit: {delta:?}");
    }
    for t in 2..=4 {
        if t > 2 {
            tick(&mut w1, &mut s1, t, &observer, &[]);
        }
        let mut view = full_view(&mut w1, &mut s1, t, Cell(1, -1));
        view.sort_unstable();
        assert_eq!(
            view,
            vec![(npc, 0), (o, 25)],
            "tick {t}: the arrival is in its cell, once"
        );
    }
}

/// A PLAYER lent in its landing cell, an established group there (a
/// resident): its arrival one-shot full (the fresh-member rule) holds
/// the player itself — and the resident's delta does not remove it.
#[test]
fn a_player_arriving_in_its_lent_cell_sees_itself_on_arrival() {
    use crate::testing::private::Payload;

    let mut w1 = World::new();
    let mut s1 = ShardedSpatialRoom::new(1, 2, 50.0, 20.0);
    let r = place_spatial(&mut w1, &mut s1, ConnectionId(1), 5.0, -10.0); // Cell(0,-1)
    let p = 9_u64;
    let group = [Cell(0, -1)];
    tick(&mut w1, &mut s1, 1, &group, &lent(p, 0, -10));

    s1.on_migrate_in(
        &mut w1,
        p,
        arriving(0.5, -10.0, Some(DEFAULT_SPEED)),
        Some(PlayerId(9)),
    );
    let delta = tick(&mut w1, &mut s1, 2, &group, &[]).remove(0);
    if let Some(delta) = delta {
        assert!(delta.delta, "the resident's group stays in delta mode");
        assert!(!delta.removed.contains(&p), "no removal: {delta:?}");
    }
    let mut pbuf = bytes::BytesMut::new();
    assert!(s1.private(&mut w1, PlayerId(9), &group[0], &[], &mut pbuf));
    let frame = crate::testing::Private::decode(pbuf.as_ref()).expect("private");
    let Some(Payload::Snapshot(full)) = frame.payload else {
        panic!("expected the one-shot full, got {frame:?}")
    };
    let seen: BTreeSet<u64> = full.entities.iter().map(|e| e.entity).collect();
    assert_eq!(
        seen,
        BTreeSet::from([p, r]),
        "the arrival sees itself and the resident"
    );
}

/// The lent copy sits in Cell(-1,-1) (wire x = -1), the arrival lands in
/// Cell(0,-1) (x = 1): the stale copy leaves its cell and the arrival is
/// in its own, so a group seeing both cells holds it exactly once — at
/// its new position.
#[test]
fn an_arrival_beside_its_lent_cell_leaves_no_stale_copy() {
    let mut w1 = World::new();
    let mut s1 = ShardedSpatialRoom::new(1, 2, 50.0, 20.0);
    let q = place_spatial(&mut w1, &mut s1, ConnectionId(1), 5.0, -10.0); // Cell(0,-1)
    let npc = 7_u64;
    tick(&mut w1, &mut s1, 1, &[Cell(0, -1)], &lent(npc, -1, -10));
    assert!(full_view(&mut w1, &mut s1, 1, Cell(0, -1)).contains(&(npc, -1)));

    s1.on_migrate_in(&mut w1, npc, arriving(1.0, -10.0, None), None);
    let [Some(delta)] = &tick(&mut w1, &mut s1, 2, &[Cell(0, -1)], &[])[..] else {
        panic!("the crossing is news for the group")
    };
    assert!(
        delta.cell_exits.len() == 1 || delta.removed.contains(&npc),
        "the stale copy is exited from its cell: {delta:?}"
    );
    assert!(delta.entities.iter().any(|e| (e.entity, e.x) == (npc, 1)));
    let mut view = full_view(&mut w1, &mut s1, 2, Cell(0, -1));
    view.sort_unstable();
    assert_eq!(view, vec![(npc, 1), (q, 5)], "once, at the new position");
    assert!(
        !s1.book.buckets.contains_key(&Cell(-1, -1)),
        "no stale copy"
    );
}

/// The reverse crossing: shard 1's NPC walks into shard 0 and comes
/// back into shard 1's view through the strip — in the tick its parked
/// removal lands (the neighbour's border arrived early) or a tick later.
/// Either way the observer ends up holding it once, at the lent position.
#[test]
fn a_departed_entity_reappears_through_the_strip() {
    for same_tick in [true, false] {
        let mut w1 = World::new();
        let mut s1 = ShardedSpatialRoom::new(1, 2, 50.0, 20.0);
        let q = place_spatial(&mut w1, &mut s1, ConnectionId(1), 5.0, -30.0); // Cell(0,-2)
        let npc = w1.spawn(Position { x: 1.0, y: -10.0 }).id(); // Cell(0,-1)
        let groups = [Cell(0, -2)];
        tick(&mut w1, &mut s1, 1, &groups, &[]);
        let wire = w1.get::<WireId>(npc).expect("stamped").get();

        // It crosses to x = -1 (Cell(-1,-1), shard 0's region) and is
        // handed over at the end of the tick.
        w1.entity_mut(npc).insert(Position { x: -1.0, y: -10.0 });
        s1.update(&mut w1, &ctx(2));
        assert_eq!(s1.collect_migrations(&mut w1, 0).len(), 1);
        s1.on_migrate_out(&mut w1, wire);
        let mut out = bytes::BytesMut::new();
        s1.snapshot(&mut w1, &ctx(2), &groups[0], &[], &mut out);

        let strip3 = if same_tick {
            lent(wire, -1, -10)
        } else {
            Vec::new()
        };
        tick(&mut w1, &mut s1, 3, &groups, &strip3);
        tick(&mut w1, &mut s1, 4, &groups, &lent(wire, -1, -10));
        let mut view = full_view(&mut w1, &mut s1, 4, Cell(0, -2));
        view.sort_unstable();
        let mut want = vec![(wire, -1), (q, 5)];
        want.sort_unstable();
        assert_eq!(view, want, "same tick: {same_tick}");
    }
}
