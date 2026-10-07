//! Session → owner binding: the join hook binds once, every `Incoming`
//! carries the owner (no lookups in systems). A request from a session
//! with no binding is answered by the engine (`requests_unbound`) and
//! never reaches the game; leave unbinds.

mod common;

use common::*;
use s0_calls::wire::reason;

#[derive(Default)]
struct S {
    seen: Vec<(Owner, ConnId)>,
}

fn sys(s: &mut S, calls: &mut Calls, _: &mut TickCtx) {
    for r in calls.take_incoming::<Equip>() {
        s.seen.push((r.owner, r.conn));
        calls.reply(r.reply, Ok(Equipped { item: r.body.item }));
    }
}

#[test]
fn incoming_carries_the_bound_owner_and_unbound_sessions_are_refused() {
    let mut room = scripted(
        RoomConfig::default(),
        S::default(),
        |c| c.accept::<Equip>().unwrap(),
        sys,
    );
    let i = room.inputs();
    i.join(ALICE, "alice");
    i.join(BOB, "bob");
    // Same tick as the join: CONTROL binds before READ.
    i.request(ALICE, rpc(1, &Equip { item: 1 }));
    i.request(BOB, rpc(2, &Equip { item: 2 }));
    let out = room.tick(0.0);
    assert!(out.iter().all(|(_, r)| r.ok));
    assert_eq!(
        room.logic().game.s.seen,
        vec![(Owner(1), ALICE), (Owner(2), BOB)]
    );

    // A session the game unbound (or never bound): the engine answers.
    room.logic_mut().calls.unbind(BOB);
    i.request(BOB, rpc(3, &Equip { item: 2 }));
    let out = room.tick(0.0);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].1.reason, reason::NOT_BOUND);
    assert_eq!(room.logic().game.s.seen.len(), 2, "the game never saw it");
    assert_eq!(room.counters().requests_unbound, 1);

    // Leave unbinds (the engine does it in CONTROL).
    i.leave(ALICE);
    room.tick(0.0);
    assert_eq!(room.logic().calls.bound(ALICE), None);
}
