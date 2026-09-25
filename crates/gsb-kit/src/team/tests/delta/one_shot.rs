//! The delta mode's baselines: a fresh team's full group frame baselines
//! its members; a member without a baseline for its team's view — a
//! join into an established team, a runtime team change, a resume — gets
//! a one-shot private full, once.

use super::*;

/// A fresh team group's first frame is a FULL, which baselines every
/// member at once: no member gets a one-shot private full.
#[test]
fn a_fresh_team_gets_a_full_that_baselines_its_members() {
    let mut world = World::new();
    let mut room = delta_room();
    let (a, pa) = place(&mut world, &mut room, ConnectionId(2), 0.0, 0.0);
    let (c, pc) = place(&mut world, &mut room, ConnectionId(4), 400.0, 0.0);
    room.update(&mut world, &ctx(1));
    let s = frame(&mut room, &mut world, 1, 0).expect("a fresh team emits");
    assert!(!s.delta, "the fresh team's frame is a full");
    assert_eq!(records(&s), [(a, 0, 0), (c, 400, 0)].into_iter().collect());
    for p in [pa, pc] {
        assert!(
            one_shot(&private(&mut room, &mut world, p, 0)).is_none(),
            "baselined by the group's full"
        );
    }
}

/// A member joining an ESTABLISHED team gets a one-shot private full of
/// the team's view (the group frame is a delta it has no baseline for);
/// the team's other members do not, and the joiner gets it once.
#[test]
fn a_member_joining_an_established_team_gets_a_one_shot_full() {
    let mut world = World::new();
    let mut room = delta_room();
    let (a, pa) = place(&mut world, &mut room, ConnectionId(2), 0.0, 0.0);
    room.update(&mut world, &ctx(1));
    frame(&mut room, &mut world, 1, 0).expect("full");
    private(&mut room, &mut world, pa, 0);

    let (c, pc) = place(&mut world, &mut room, ConnectionId(4), 50.0, 0.0);
    room.update(&mut world, &ctx(2));
    let s = frame(&mut room, &mut world, 2, 0).expect("C is new to the view");
    assert!(s.delta, "the established team's frame stays a delta");
    assert_eq!(records(&s), [(c, 50, 0)].into_iter().collect());
    assert!(one_shot(&private(&mut room, &mut world, pa, 0)).is_none());
    let full = one_shot(&private(&mut room, &mut world, pc, 0)).expect("C's one-shot");
    assert!(!full.delta && full.sequence == 2);
    assert_eq!(
        records(&full),
        [(a, 0, 0), (c, 50, 0)].into_iter().collect()
    );

    room.update(&mut world, &ctx(3));
    assert!(frame(&mut room, &mut world, 3, 0).is_none());
    assert!(
        one_shot(&private(&mut room, &mut world, pc, 0)).is_none(),
        "once"
    );
    room.on_leave(&mut world, pc);
    assert_eq!(room.baselines.len(), 1, "a leave drops its baseline");
}

/// A runtime team change and a resume leave the session without a
/// baseline for its (new) team's view: each gets a one-shot full.
#[test]
fn a_team_change_and_a_resume_get_a_one_shot_full() {
    let mut world = World::new();
    let mut room = delta_room();
    let (a, pa) = place(&mut world, &mut room, ConnectionId(2), 0.0, 0.0); // team 0
    let (b, pb) = place(&mut world, &mut room, ConnectionId(1), 100.0, 0.0); // team 1
    room.update(&mut world, &ctx(1));
    for (p, t) in [(pa, 0), (pb, 1)] {
        frame(&mut room, &mut world, 1, t).expect("full");
        private(&mut room, &mut world, p, t);
    }

    let entity_a = room.player_entity[&pa];
    world.entity_mut(entity_a).insert(TeamMember(Team(1)));
    room.update(&mut world, &ctx(2));
    assert_eq!(room.group_of(&world, pa), Team(1));
    frame(&mut room, &mut world, 2, 1).expect("A joined team 1's view");
    let full = one_shot(&private(&mut room, &mut world, pa, 1)).expect("A's one-shot");
    assert_eq!(
        records(&full),
        [(a, 0, 0), (b, 100, 0)].into_iter().collect()
    );
    assert!(one_shot(&private(&mut room, &mut world, pb, 1)).is_none());

    room.on_disconnect(&mut world, pb, "bee");
    room.on_resume(&mut world, "bee", ConnectionId(5), pb, b);
    room.update(&mut world, &ctx(3));
    assert!(frame(&mut room, &mut world, 3, 1).is_none());
    let full = one_shot(&private(&mut room, &mut world, pb, 1)).expect("B's one-shot");
    assert_eq!(
        records(&full),
        [(a, 0, 0), (b, 100, 0)].into_iter().collect()
    );
}
