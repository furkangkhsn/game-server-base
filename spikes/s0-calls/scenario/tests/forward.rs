//! `forward`: a one-line shared rule. The incoming request goes straight
//! to a service, the answer straight to the client; the game's systems
//! never see it. The service sees the requester's verified identity.

mod common;

use common::*;

#[derive(Default)]
struct S {
    seen_any: usize,
}

fn sys(s: &mut S, calls: &mut Calls, _ctx: &mut TickCtx) {
    s.seen_any += calls.take_incoming_all().len();
}

#[test]
fn a_forwarded_request_never_reaches_the_game() {
    let be = FakeBackend::new();
    let mut sim = Sim::new();
    sim.service(Friends::serve(be.clone()), 8);
    let friends: FriendsClient = sim.client();
    let mut room = scripted(
        RoomConfig::default(),
        S::default(),
        move |c| c.forward(move |r: ListFriends| friends.list(r)).unwrap(),
        sys,
    );
    let i = room.inputs();
    i.join(BOB, "bob");
    i.request(BOB, rpc(5, &ListFriends {}));
    assert!(room.tick_sim(&mut sim).is_empty());
    let out = room.tick_sim(&mut sim);
    assert_eq!(out.len(), 1);
    let list: FriendList = decode(&out[0].1);
    assert_eq!(list.names, vec!["alice", "carol"], "cx.player() was bob");
    assert_eq!(room.logic().game.s.seen_any, 0, "the game never saw it");
    let c = room.counters();
    assert_eq!(
        (c.requests_forwarded, c.answers_sent, c.responses_delivered),
        (1, 1, 0)
    );
    assert_eq!(c.responses_unhandled, 0, "nothing for the game to take");
}

#[test]
fn a_forwarded_op_cannot_also_be_accepted() {
    let sim = {
        let mut s = Sim::new();
        s.service(Friends::serve(FakeBackend::new()), 8);
        s
    };
    let friends: FriendsClient = sim.client();
    let mut calls = Calls::new(CallsConfig::default());
    calls
        .forward(move |r: ListFriends| friends.list(r))
        .unwrap();
    let e = calls.accept::<ListFriends>().unwrap_err();
    assert!(matches!(e, BootError::OpCollision { op: 1005, .. }));
}
