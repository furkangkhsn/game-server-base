//! The AOI room over the volumetric preset (`Grid3`, BACKLOG A5), with
//! the 3D fixture game (the third axis is height): the room is the same
//! generic room — a group is a cube cell, its packet assembles the 27
//! cells around it, and an emptied cell leaves as one `CellExit` naming
//! its three indices. `cell_size = 20`.

use crate::space::{Cell3, Grid3};
use crate::testing::{Fixture3, Position3, WorldSnapshot3};

use super::*;

type Room3 = crate::aoi::AoiRoom<Fixture3, Grid3>;

fn room3() -> Room3 {
    Room3::with_game(Fixture3::default(), Grid3::new(20.0))
}

/// Join a player and move its entity to `(x, y, z)`; its wire id.
fn place3(world: &mut World, room: &mut Room3, conn: u64, [x, y, z]: [f32; 3]) -> u64 {
    let admission = room.on_join(world, ConnectionId(conn));
    let entity = room.player_entity[&admission.player];
    world.entity_mut(entity).insert(Position3 { x, y, z });
    admission.entity
}

/// Group `cell`'s packet this tick, decoded.
fn packet(world: &mut World, room: &mut Room3, tick: u64, cell: Cell3) -> WorldSnapshot3 {
    let mut out = bytes::BytesMut::new();
    assert!(room.snapshot(world, &ctx(tick), &cell, &[], &mut out));
    WorldSnapshot3::decode(out.as_ref()).expect("snapshot payload")
}

fn ids3(snap: &WorldSnapshot3) -> BTreeSet<u64> {
    snap.entities.iter().map(|e| e.entity).collect()
}

/// A player's interest is the 3×3×3 block: the cells directly above and
/// below it, and the far corner of the block, are in; two cells up, or
/// two cells along the ground, are out — and the view is symmetric (the
/// player two cells up sees the one above, not the origin's).
#[test]
fn grid3_interest_is_the_27_cell_block() {
    let mut world = World::new();
    let mut room = room3();
    let a = place3(&mut world, &mut room, 1, [5.0, 5.0, 5.0]); // (0, 0, 0)
    let above = place3(&mut world, &mut room, 2, [5.0, 5.0, 25.0]); // (0, 0, 1)
    let below = place3(&mut world, &mut room, 3, [5.0, 5.0, -15.0]); // (0, 0, -1)
    let corner = place3(&mut world, &mut room, 4, [-5.0, 25.0, -5.0]); // (-1, 1, -1)
    let two_up = place3(&mut world, &mut room, 5, [5.0, 5.0, 45.0]); // (0, 0, 2)
    let two_off = place3(&mut world, &mut room, 6, [45.0, 5.0, 5.0]); // (2, 0, 0)
    room.update(&mut world, &ctx(1));

    let seen = ids3(&packet(&mut world, &mut room, 1, Cell3(0, 0, 0)));
    assert_eq!(seen, BTreeSet::from([a, above, below, corner]), "{seen:?}");
    let up = ids3(&packet(&mut world, &mut room, 1, Cell3(0, 0, 2)));
    assert_eq!(up, BTreeSet::from([above, two_up]), "{up:?}");
    let off = ids3(&packet(&mut world, &mut room, 1, Cell3(2, 0, 0)));
    assert_eq!(off, BTreeSet::from([two_off]), "{off:?}");
}

/// A climb out of a cell: the new cell's delta carries the climber as an
/// update under the same wire id; the emptied cell's delta carries ONE
/// cell exit naming its three indices (the height index included) and
/// no entity record; the record carries the height.
#[test]
fn grid3_a_climb_exits_the_emptied_cell_by_its_three_indices() {
    let mut world = World::new();
    let mut room = room3();
    let a = place3(&mut world, &mut room, 1, [5.0, 5.0, -35.0]); // (0, 0, -2)
    let b = place3(&mut world, &mut room, 2, [5.0, 5.0, 65.0]); // (0, 0, 3)
    room.update(&mut world, &ctx(1));
    assert_eq!(
        ids3(&packet(&mut world, &mut room, 1, Cell3(0, 0, -2))),
        [a].into()
    );
    assert_eq!(
        ids3(&packet(&mut world, &mut room, 1, Cell3(0, 0, 3))),
        [b].into()
    );

    let entity = room.player_entity[&PlayerId(1)];
    let climbed = Position3 {
        x: 5.0,
        y: 5.0,
        z: 70.0,
    };
    world.entity_mut(entity).insert(climbed);
    room.update(&mut world, &ctx(2));

    let arrived = packet(&mut world, &mut room, 2, Cell3(0, 0, 3));
    assert!(arrived.delta, "an established group ships a delta");
    assert_eq!(ids3(&arrived), [a].into(), "the climber, as an update");
    assert_eq!(arrived.entities[0].z, 70, "the record carries the height");
    assert!(arrived.removed.is_empty() && arrived.cell_exits.is_empty());

    let left = packet(&mut world, &mut room, 2, Cell3(0, 0, -2));
    assert!(left.delta && left.entities.is_empty(), "{left:?}");
    let exits: Vec<(i32, i32, i32)> = left.cell_exits.iter().map(|e| (e.x, e.y, e.z)).collect();
    assert_eq!(exits, vec![(0, 0, -2)], "one exit, three indices");
}
