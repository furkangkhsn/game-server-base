//! The sharded × spatial composite over the demo game (`cell_size =
//! 20`, half = 50, 2 shards: s0 = x ∈ [-50, 0], s1 = x ∈ [0, 50]; border
//! margin 12.5): the borrowed border strip, pinned through the demo
//! codec's record values.

use gsb_core::shard::{BorderRecord, ShardLogic};

use super::*;
use crate::aoi::Cell;
use crate::demo::wire::StripPos;
use crate::kit::space::{CellSpace, Grid2};
use crate::sharded::{ShardedRoom, ShardedSpatialRoom};

/// A client-view accumulator with FULL/DELTA application semantics
/// (upserts, per-entity removals, whole-cell forgets) — what an
/// observer's connection holds after each packet.
#[derive(Default)]
struct ClientView {
    ents: HashMap<u64, (i32, i32)>,
}

impl ClientView {
    fn apply(&mut self, snap: &WorldSnapshot, cell_size: f32) {
        if !snap.delta {
            self.ents.clear();
        }
        let grid = Grid2::new(cell_size);
        for ce in &snap.cell_exits {
            let exited = Cell(ce.x, ce.y);
            self.ents
                .retain(|_, &mut (x, y)| grid.cell_of(&StripPos { x, y }) != exited);
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
    let _o = place(&mut w1, &mut s1, ConnectionId(2), 5.0, -10.0);

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
        let entity = entity_of(&mut w0, m);
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
        let snap = WorldSnapshot::decode(out.as_ref()).expect("snapshot");
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
    let _o = place(&mut w1, &mut s1, ConnectionId(3), 5.0, -10.0); // Cell(0,-1)

    // Tick 1: first contact — everything enters once.
    s0.update(&mut w0, &ctx(1));
    let borrowed: Vec<BorderRecord<StripPos>> = s0.collect_border(&w0);
    s1.update(&mut w1, &ctx(1));
    let mut out = bytes::BytesMut::new();
    assert!(s1.snapshot(&mut w1, &ctx(1), &Cell(0, -1), &borrowed, &mut out));
    let seen: BTreeSet<u64> = WorldSnapshot::decode(out.as_ref())
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
    let entity_p = entity_of(&mut w0, p);
    w0.entity_mut(entity_p)
        .insert(Position { x: -5.0, y: -10.0 }); // same cell
    s0.update(&mut w0, &ctx(4));
    let borrowed: Vec<BorderRecord<StripPos>> = s0.collect_border(&w0);
    s1.update(&mut w1, &ctx(4));
    let mut out = bytes::BytesMut::new();
    assert!(s1.snapshot(&mut w1, &ctx(4), &Cell(0, -1), &borrowed, &mut out));
    let snap = WorldSnapshot::decode(out.as_ref()).expect("snapshot");
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
