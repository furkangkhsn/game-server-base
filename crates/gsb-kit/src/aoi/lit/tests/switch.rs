//! Switching a light on and off, and the viewer's session: a dropped
//! batch and a resume re-baseline the viewer with its LIT view, never
//! the neighbourhood's; a leaver takes its tables along.

use crate::aoi::lit::tests::*;
use crate::testing::Facing;

/// A light switched on replaces the client's view with the lit one (the
/// viewer group's fresh full); switched off, the client gets the whole
/// neighbourhood back (the AOI room's one-shot private full — its cell
/// group has another member, so it is not fresh).
#[test]
fn a_light_switched_on_replaces_the_view_and_off_restores_it() {
    let mut sim = Sim::new(lit_room());
    let a = sim.join(1, 10.0, 0.0);
    let b = sim.join(2, 5.0, 0.0);
    sim.step();
    assert_eq!(sim.holds(a), sim.wires(&[a, b]));

    let a_entity = sim.seats[a].entity;
    sim.world.entity_mut(a_entity).insert(Facing(1));
    sim.step();
    let got = snapshots(&sim, a);
    assert_eq!(got.len(), 1);
    assert!(
        !got[0].0 && !got[0].1.delta,
        "the viewer group's first frame is a full"
    );
    assert_eq!(sim.holds(a), sim.wires(&[a]), "B is gone from A's client");
    assert_eq!(sim.holds(b), sim.wires(&[a, b]));

    sim.world.entity_mut(a_entity).remove::<Facing>();
    sim.step();
    assert_eq!(sim.group(a), cell(0, 0));
    assert!(
        snapshots(&sim, a)
            .iter()
            .any(|(private, s)| *private && !s.delta),
        "the one-shot private full of the whole view"
    );
    assert_eq!(sim.holds(a), sim.wires(&[a, b]));
    assert!(
        sim.room.viewers.is_empty(),
        "a viewer's tables go with its light"
    );
    assert_eq!(sim.room.baselines.len(), 0);
}

/// Switched off while no other member of its cell shares the cell's
/// group (the other player has a light): the cell group is fresh for the
/// core, so its first packet is a full — the core's contract for a fresh
/// group — and it baselines the returning player.
#[test]
fn a_cell_group_a_viewer_returns_to_alone_is_fresh() {
    let mut sim = Sim::new(lit_room());
    let a = sim.join(1, 10.0, 0.0);
    let b = sim.join(2, 5.0, 0.0);
    let (ea, eb) = (sim.seats[a].entity, sim.seats[b].entity);
    sim.world.entity_mut(ea).insert(Facing(1));
    sim.world.entity_mut(eb).insert(Facing(1));
    sim.step();
    sim.step();
    assert_eq!(sim.holds(a), sim.wires(&[a]));

    sim.world.entity_mut(ea).remove::<Facing>();
    sim.step();
    let got = snapshots(&sim, a);
    assert_eq!(
        got.len(),
        1,
        "no one-shot: the group's own full baselines it"
    );
    assert!(!got[0].0 && !got[0].1.delta, "the fresh cell group's full");
    assert_eq!(sim.holds(a), sim.wires(&[a, b]));
    assert_eq!(sim.holds(b), sim.wires(&[a, b]), "A is ahead of B: lit");
}

/// A dropped batch and a resume: the viewer is re-sent a full of its LIT
/// view in its private frame (B stays out of it); a leaver leaves no
/// viewer state behind.
#[test]
fn a_viewers_resend_is_its_lit_view() {
    let mut sim = Sim::new(lit_room());
    let a = sim.join(1, 10.0, 0.0);
    let b = sim.join(2, 5.0, 0.0);
    let c = sim.join(3, 15.0, 0.0);
    let a_entity = sim.seats[a].entity;
    sim.world.entity_mut(a_entity).insert(Facing(1));
    sim.step();
    sim.at(c, 16.0, 0.0);
    sim.step_with(false, &[a]);
    assert_eq!(sim.seats[a].view.get(sim.seats[c].wire), Some(&(15, 0)));

    let (wb, lit) = (sim.seats[b].wire, sim.wires(&[a, c]));
    let private_full = |sim: &Sim<Room>| {
        let fulls: Vec<_> = snapshots(sim, a)
            .into_iter()
            .filter(|(private, _)| *private)
            .collect();
        assert_eq!(fulls.len(), 1, "one private full");
        fulls[0]
            .1
            .entities
            .iter()
            .map(|r| r.entity)
            .collect::<BTreeSet<_>>()
    };
    sim.step();
    assert_eq!(private_full(&sim), lit, "the re-send is the lit view");
    assert!(!records_sent(&sim, a).contains(&wb));
    assert_eq!(sim.seats[a].view.get(sim.seats[c].wire), Some(&(16, 0)));

    let player = sim.seats[a].player;
    sim.room.on_resume(
        &mut sim.world,
        "",
        ConnectionId(9),
        player,
        sim.seats[a].wire,
    );
    sim.step();
    assert_eq!(private_full(&sim), lit, "the resumed session's full is lit");

    sim.room.on_leave(&mut sim.world, player);
    assert!(
        sim.room.viewers.is_empty(),
        "no viewer state outlives a leave"
    );
    assert_eq!(sim.room.baselines.len(), 0);
}
