//! The filter itself: an unlit record never reaches the viewer, a lit
//! one comes back whole, and the viewer's delta removes what the light
//! leaves — while a viewer without a light keeps seeing everything.
//! Cell edge 20: every seat below is in `Cell(0, 0)` unless moved.

use crate::aoi::lit::tests::*;
use crate::testing::{Cloaked, Facing};

/// A viewer facing east at x = 10 with a record behind it (x = 5) and
/// one ahead (x = 15), all in its own cell, next to a player without a
/// light (x = 12, ahead too): over a dozen ticks — both records moving
/// every tick, a keep-alive every fifth — no frame of the viewer ever
/// carries the record behind it, while the other player's frames do;
/// the viewer's keep-alive is a fresh full of its lit view.
#[test]
fn an_unlit_record_in_the_viewers_own_cell_never_reaches_it() {
    let mut sim = Sim::new(lit_room());
    let a = sim.join(1, 10.0, 0.0);
    let b = sim.join(2, 5.0, 0.0);
    let c = sim.join(3, 15.0, 0.0);
    let d = sim.join(4, 12.0, 0.0);
    let a_entity = sim.seats[a].entity;
    sim.world.entity_mut(a_entity).insert(Facing(1));
    let hidden = sim.seats[b].wire;
    let mut others_saw_it = false;
    for t in 0..12u64 {
        if t > 0 {
            sim.at(b, 5.0 - (t % 4) as f32, 0.0);
            sim.at(c, 15.0 + (t % 3) as f32, 0.0);
        }
        sim.step_with(t % 5 == 4, &[]);
        assert_eq!(sim.group(a), LitGroup::Viewer(sim.seats[a].player));
        assert_eq!(sim.group(d), cell(0, 0), "no light: the shared group");
        assert!(
            !records_sent(&sim, a).contains(&hidden),
            "tick {t}: the unlit record reached the viewer"
        );
        assert_eq!(
            sim.holds(a),
            sim.wires(&[a, c, d]),
            "tick {t}: C and D are ahead"
        );
        assert_eq!(sim.holds(d), sim.wires(&[a, b, c, d]), "tick {t}");
        others_saw_it |= records_sent(&sim, d).contains(&hidden);
        if t % 5 == 4 {
            // The keep-alive: a fresh full of the LIT view (the cached
            // frame is a delta — re-sending it would heal nothing).
            let got = snapshots(&sim, a);
            let (private, full) = &got[0];
            assert!(!private && !full.delta, "tick {t}: a keep-alive full");
            let ids: BTreeSet<u64> = full.entities.iter().map(|r| r.entity).collect();
            assert_eq!(ids, sim.wires(&[a, c, d]), "tick {t}");
        }
    }
    assert!(others_saw_it, "the record's bytes went to the unlit viewer");
    assert!(
        snapshots(&sim, a)
            .iter()
            .all(|(_, s)| !s.delta || s.cell_exits.is_empty()),
        "a lit view is not a union of cells: no cell exits"
    );
}

/// The record walks into the light (a whole record in the viewer's
/// delta), out of it again (a `removed` id, no record), the viewer turns
/// round (the light moves without anything moving: the record behind is
/// lit, the one ahead removed), and a stealth rule on the RECORD's
/// entity hides it again — the other player sees it throughout.
#[test]
fn the_light_brings_a_record_back_whole_and_takes_it_away() {
    let mut sim = Sim::new(lit_room());
    let a = sim.join(1, 10.0, 0.0);
    let b = sim.join(2, 5.0, 0.0);
    let d = sim.join(4, 12.0, 0.0);
    let a_entity = sim.seats[a].entity;
    sim.world.entity_mut(a_entity).insert(Facing(1));
    sim.step();
    sim.step();
    let (wb, wd) = (sim.seats[b].wire, sim.seats[d].wire);
    assert_eq!(sim.holds(a), sim.wires(&[a, d]));

    // Into the light: a delta carrying B's whole record.
    sim.at(b, 16.0, 0.0);
    sim.step();
    let got = snapshots(&sim, a);
    assert_eq!(got.len(), 1, "one group frame, no private full");
    let (_, s) = &got[0];
    assert!(s.delta);
    let rec = s
        .entities
        .iter()
        .find(|r| r.entity == wb)
        .expect("B's record");
    assert_eq!((rec.x, rec.y), (16, 0));
    assert_eq!(sim.seats[a].view.get(wb), Some(&(16, 0)));

    // Out of it: removed, and no record of it.
    sim.at(b, 4.0, 0.0);
    sim.step();
    let got = snapshots(&sim, a);
    assert!(got[0].1.delta && got[0].1.removed == vec![wb]);
    assert!(!records_sent(&sim, a).contains(&wb));
    assert_eq!(sim.holds(a), sim.wires(&[a, d]));

    // The viewer turns west: B (behind, now ahead) is lit, D removed.
    sim.world.entity_mut(a_entity).insert(Facing(-1));
    sim.step();
    let got = snapshots(&sim, a);
    assert!(got[0].1.delta && got[0].1.removed == vec![wd]);
    assert_eq!(sim.seats[a].view.get(wb), Some(&(4, 0)));
    assert_eq!(sim.holds(a), sim.wires(&[a, b]));

    // A stealth rule on the record's entity: B leaves the light again.
    let b_entity = sim.seats[b].entity;
    sim.world.entity_mut(b_entity).insert(Cloaked);
    sim.step();
    assert_eq!(snapshots(&sim, a)[0].1.removed, vec![wb]);
    assert_eq!(sim.holds(a), sim.wires(&[a]));
    assert_eq!(
        sim.holds(d),
        sim.wires(&[a, b, d]),
        "the unlit viewer sees B"
    );

    // A still world: the viewer's group is silent.
    sim.step();
    assert!(
        sim.seats[a].frames.is_empty(),
        "nothing changed, nothing sent"
    );
}

/// A record leaving the neighbourhood, a leaver's (despawned) one and an
/// NPC the game despawns are `removed` from the viewer's view — and the
/// kit's `wire id → entity` index lets go of the despawned records.
#[test]
fn a_record_that_leaves_the_neighbourhood_or_the_world_is_removed() {
    let mut sim = Sim::new(lit_room());
    let a = sim.join(1, 10.0, 0.0);
    let c = sim.join(3, 15.0, 0.0);
    let e = sim.join(5, 30.0, 0.0);
    let a_entity = sim.seats[a].entity;
    sim.world.entity_mut(a_entity).insert(Facing(1));
    sim.step();
    assert_eq!(sim.holds(a), sim.wires(&[a, c, e]));

    sim.at(e, 70.0, 0.0); // Cell(3, 0): out of the 3×3.
    sim.step();
    assert_eq!(snapshots(&sim, a)[0].1.removed, vec![sim.seats[e].wire]);

    let (player, wc) = (sim.seats[c].player, sim.seats[c].wire);
    sim.room.on_leave(&mut sim.world, player);
    sim.seats.remove(c);
    sim.step();
    assert_eq!(snapshots(&sim, a)[0].1.removed, vec![wc]);
    assert_eq!(sim.holds(a), sim.wires(&[a]));
    let book = &sim.room.room.book;
    let index = book
        .entities
        .as_ref()
        .expect("the lit room keeps the index");
    assert_eq!(
        index.len(),
        book.last_cell.len(),
        "one entry per bucketed record"
    );
    assert_eq!(book.entity_of(wc), None);

    // An NPC the game spawns in the light (stamped by the kit) is lit
    // through the index; despawned by game code, it is removed.
    let npc = sim.world.spawn(Position { x: 17.0, y: 0.0 }).id();
    sim.step();
    let wn = sim.world.get::<WireId>(npc).expect("stamped").get();
    assert_eq!(sim.seats[a].view.get(wn), Some(&(17, 0)));
    sim.world.despawn(npc);
    sim.step();
    assert_eq!(snapshots(&sim, a)[0].1.removed, vec![wn]);
    assert_eq!(sim.holds(a), sim.wires(&[a]));
    let book = &sim.room.room.book;
    assert_eq!(book.entity_of(wn), None, "the sweep lets go of it too");
    assert_eq!(
        book.entities.as_ref().map(|i| i.len()),
        Some(book.last_cell.len())
    );
}

/// Fail closed: a record the room cannot resolve to an entity (never the
/// case while the index covers the buckets — forced here) is unlit.
#[test]
fn a_record_without_a_known_entity_is_unlit() {
    let mut sim = Sim::new(lit_room());
    let a = sim.join(1, 10.0, 0.0);
    let c = sim.join(3, 15.0, 0.0);
    let a_entity = sim.seats[a].entity;
    sim.world.entity_mut(a_entity).insert(Facing(1));
    sim.step();
    assert_eq!(sim.holds(a), sim.wires(&[a, c]));

    let wc = sim.seats[c].wire;
    let index = sim.room.room.book.entities.as_mut().expect("kept");
    index.remove(&wc);
    sim.step();
    assert_eq!(snapshots(&sim, a)[0].1.removed, vec![wc]);
    assert_eq!(sim.holds(a), sim.wires(&[a]));
}
