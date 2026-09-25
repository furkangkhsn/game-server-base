//! The cell engine under a send rate (A10; the fixture's `Rated` codec:
//! x in `[32, 48)` every 4th step, `[48, 64)` every 8th). Cell edge 20:
//! A (41, 0) and B (44, y) share `Cell(2, 0)`. A step's number is its
//! tick here (one `update` per tick from tick 1).
//!
//! - A move inside a cell goes out only on the record's due steps, with
//!   the value it has then; the shared piece carries it to every group.
//! - A cell crossing and a despawn go out at once (`removed` + the
//!   upsert in the target cell / the `removed` alone).
//! - The keep-alive full and the one-shot private full carry a pending
//!   record's CURRENT value.

use super::*;
use crate::codec::SendEvery;
use crate::space::Grid2;
use crate::testing::{Private, RatedFixture, private::Payload};

type Rated = super::super::AoiRoom<RatedFixture, Grid2>;

const CELL: Cell = Cell(2, 0);

/// A, B in `Cell(2, 0)`, the group's full sent on tick 1.
fn two() -> (World, Rated, u64, u64) {
    let mut world = World::new();
    let mut room = Rated::with_game(RatedFixture::default(), Grid2::new(20.0));
    let mut put = |world: &mut World, conn, x| {
        let a = room.on_join(world, ConnectionId(conn));
        let e = room.player_entity[&a.player];
        world.entity_mut(e).insert(Position { x, y: 0.0 });
        (a.entity, a.player)
    };
    let (a, _) = put(&mut world, 1, 41.0);
    let (b, _) = put(&mut world, 2, 44.0);
    room.update(&mut world, &ctx(1));
    let mut out = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &CELL, &[], &mut out));
    assert!(!decode(&out).delta, "a fresh group's full");
    (world, room, a, b)
}

fn move_wire(world: &mut World, room: &Rated, wire: u64, x: f32, y: f32) {
    let (&e, _) = room
        .book
        .last_cell
        .iter()
        .find(|(_, (w, _))| *w == wire)
        .expect("bucketed");
    world.entity_mut(e).insert(Position { x, y });
}

fn frame(world: &mut World, room: &mut Rated, tick: u64) -> Option<WorldSnapshot> {
    room.update(world, &ctx(tick));
    let mut out = bytes::BytesMut::new();
    room.snapshot(world, &ctx(tick), &CELL, &[], &mut out)
        .then(|| decode(&out))
}

fn records(s: &WorldSnapshot) -> BTreeSet<(u64, i32, i32)> {
    s.entities.iter().map(|r| (r.entity, r.x, r.y)).collect()
}

/// B walks one unit a tick inside its cell: the group's frame carries B
/// exactly on B's due ticks, with B's value of that tick; between them
/// the group is silent (nothing else changed).
#[test]
fn a_move_inside_a_cell_goes_out_on_its_due_steps_only() {
    let (mut world, mut room, _, b) = two();
    let mut sent = 0;
    for tick in 2..=17u64 {
        move_wire(&mut world, &room, b, 44.0, tick as f32);
        let f = frame(&mut world, &mut room, tick);
        if SendEvery::Ticks4.due(tick, b) {
            let s = f.expect("due: B goes out");
            assert!(s.delta && s.removed.is_empty() && s.cell_exits.is_empty());
            assert_eq!(records(&s), [(b, 44, tick as i32)].into_iter().collect());
            sent += 1;
        } else {
            assert!(f.is_none(), "tick {tick}: B not due, the group is silent");
        }
    }
    assert_eq!(sent, 4, "once per period");
}

/// A crossing into the next cell and a leave go out on a tick that is
/// not due: `removed` from the source cell and the upsert in the
/// target (whose class is every 8th now) — at once.
#[test]
fn a_crossing_and_an_exit_go_out_at_once() {
    let (mut world, mut room, _, b) = two();
    let quiet = (2..).find(|&t| !SendEvery::Ticks4.due(t, b) && !SendEvery::Ticks8.due(t, b));
    let quiet = quiet.expect("a quiet tick");
    for t in 2..quiet {
        room.update(&mut world, &ctx(t));
        let mut out = bytes::BytesMut::new();
        room.snapshot(&mut world, &ctx(t), &CELL, &[], &mut out);
    }
    move_wire(&mut world, &room, b, 61.0, 0.0); // Cell(3, 0), in the 3×3
    let s = frame(&mut world, &mut room, quiet).expect("the crossing");
    assert_eq!(s.removed, [b]);
    assert_eq!(records(&s), [(b, 61, 0)].into_iter().collect());

    // A leaves on the next tick (not a due question: a leave is an
    // exit): its cell empties, and the cell exit goes out on that very
    // tick.
    let player = *room
        .player_entity
        .keys()
        .find(|p| p.0 == 1)
        .expect("A's player");
    room.on_leave(&mut world, player);
    let s = frame(&mut world, &mut room, quiet + 1).expect("A's leave");
    assert!(s.removed.is_empty() && s.entities.is_empty(), "{s:?}");
    assert_eq!(
        s.cell_exits,
        [CellExit { x: 2, y: 0 }],
        "A was its cell's last record"
    );
}

/// B moves on a tick it is not due: the keep-alive full and a joiner's
/// one-shot private full both carry B's new value; the group's delta
/// on B's due tick re-sends it (absolute: idempotent for those).
#[test]
fn the_fulls_carry_a_pending_value() {
    let (mut world, mut room, a, b) = two();
    let quiet = (2..)
        .find(|&t| !SendEvery::Ticks4.due(t, b))
        .expect("quiet");
    for t in 2..quiet {
        let _ = frame(&mut world, &mut room, t);
    }
    move_wire(&mut world, &room, b, 44.0, 9.0);
    let joined = room.on_join(&mut world, ConnectionId(3));
    let e = room.player_entity[&joined.player];
    world.entity_mut(e).insert(Position { x: 42.0, y: 3.0 });
    let pending = frame(&mut world, &mut room, quiet).expect("C entered the cell");
    assert_eq!(
        records(&pending),
        [(joined.entity, 42, 3)].into_iter().collect(),
        "C enters at once; B is pending"
    );

    // C's one-shot full (no keep-alive this tick — the core would have
    // sent it before the private frames, and it would have covered C).
    let mut out = bytes::BytesMut::new();
    assert!(room.private(&mut world, joined.player, &CELL, &[], &mut out));
    let p = Private::decode(out.as_ref()).expect("private");
    let Some(Payload::Snapshot(one_shot)) = p.payload else {
        panic!("a one-shot full: {p:?}");
    };
    assert!(
        records(&one_shot).contains(&(b, 44, 9)),
        "one-shot: {one_shot:?}"
    );

    let mut ka = bytes::BytesMut::new();
    assert!(room.keepalive(&mut world, &ctx(quiet), &CELL, None, &mut ka));
    let full = decode(&ka);
    assert!(!full.delta);
    assert!(records(&full).contains(&(b, 44, 9)), "keep-alive: {full:?}");
    assert!(records(&full).contains(&(a, 41, 0)));

    let due = (quiet..)
        .find(|&t| SendEvery::Ticks4.due(t, b))
        .expect("due");
    for t in quiet + 1..due {
        assert!(frame(&mut world, &mut room, t).is_none(), "tick {t}");
    }
    let s = frame(&mut world, &mut room, due).expect("B's due tick");
    assert_eq!(records(&s), [(b, 44, 9)].into_iter().collect());
}

/// A crossing settles a pending change: B moves while not due, crosses
/// out of its cell and back before its due tick (each crossing carries
/// its current value at once) — and on the due tick nothing is left to
/// send.
#[test]
fn a_crossing_settles_a_pending_change() {
    let (mut world, mut room, _, b) = two();
    let due = (2..).find(|&t| SendEvery::Ticks4.due(t, b)).expect("due");
    for t in 2..=due {
        let _ = frame(&mut world, &mut room, t);
    }
    move_wire(&mut world, &room, b, 44.0, 5.0);
    assert!(frame(&mut world, &mut room, due + 1).is_none(), "pending");
    move_wire(&mut world, &room, b, 61.0, 5.0);
    let out = frame(&mut world, &mut room, due + 2).expect("crossed out");
    assert_eq!(records(&out), [(b, 61, 5)].into_iter().collect());
    move_wire(&mut world, &room, b, 44.0, 6.0);
    let back = frame(&mut world, &mut room, due + 3).expect("crossed back");
    assert_eq!(records(&back), [(b, 44, 6)].into_iter().collect());
    assert!(
        frame(&mut world, &mut room, due + 4).is_none(),
        "B's due tick: nothing pending"
    );
}
