//! (d) A push to an identity reaches the right destination — the room
//! logic (`Dest::World`) or the client (`Dest::Client`) — and every push
//! that reaches neither is counted under its own name.

mod common;

use common::*;
use s0_calls::fake::FakeService;
use s0_calls::push::{Dest, Directory, Push, SessionRoute};
use s0_calls::request::ConnId;
use s0_calls::room::{Room, RoomConfig};
use s0_calls::wire::Frame;
use tokio::sync::mpsc;

struct Setup {
    room: Room<Shop>,
    dir: Directory,
    alice_client: mpsc::Receiver<Frame>,
    alice_private: mpsc::Receiver<Frame>,
}

fn setup(client_capacity: usize) -> Setup {
    let (econ, _fake) = FakeService::new("economy", 8);
    let mut room = Room::new(RoomConfig::default(), Shop::new(econ, 0), shop_table());
    let alice = ConnId(1);
    let (room_in, alice_private) = room.join(alice, 64);
    let (client_out, alice_client) = mpsc::channel(client_capacity);
    let mut dir = Directory::default();
    dir.bind(
        "alice",
        SessionRoute {
            conn: alice,
            push_capable: true,
            client_out,
            room_in: Some(room_in),
        },
    );
    // bob: online, in no room (menu / lobby).
    let (bob_out, _bob_rx) = mpsc::channel(8);
    std::mem::forget(_bob_rx);
    dir.bind(
        "bob",
        SessionRoute {
            conn: ConnId(2),
            push_capable: true,
            client_out: bob_out,
            room_in: None,
        },
    );
    // carol: an older client that never declared PUSH.
    let (carol_out, _carol_rx) = mpsc::channel(8);
    std::mem::forget(_carol_rx);
    dir.bind(
        "carol",
        SessionRoute {
            conn: ConnId(3),
            push_capable: false,
            client_out: carol_out,
            room_in: None,
        },
    );
    Setup {
        room,
        dir,
        alice_client,
        alice_private,
    }
}

#[test]
fn a_world_push_reaches_the_room_logic_and_not_the_client() {
    let mut s = setup(8);
    s.dir.route(Push::new(
        "alice",
        Dest::World,
        &BalanceChanged { balance: 750 },
    ));
    s.room.tick();
    assert_eq!(s.room.logic().balance, 750);
    assert_eq!(s.room.stats.on_events_calls, 1);
    assert!(
        s.alice_client.try_recv().is_err(),
        "nothing on the client path"
    );
    assert_eq!(s.dir.counters.pushes_delivered_world, 1);
}

#[test]
fn a_client_push_reaches_the_client_and_not_the_room() {
    let mut s = setup(8);
    s.dir.route(Push::new(
        "alice",
        Dest::Client,
        &FriendOnline { name: "bob".into() },
    ));
    s.room.tick();
    assert_eq!(s.room.stats.on_events_calls, 0, "the room never saw it");
    match s.alice_client.try_recv() {
        Ok(Frame::Push(p)) => {
            assert_eq!(p.decode::<FriendOnline>().unwrap().name, "bob");
        }
        other => panic!("expected a PUSH frame, got {other:?}"),
    }
    assert!(
        s.alice_private.try_recv().is_err(),
        "not on the room's frame"
    );
    assert_eq!(s.dir.counters.pushes_delivered_client, 1);
}

#[test]
fn every_undelivered_push_has_its_own_counter() {
    let mut s = setup(1);
    // Offline: no session for the identity.
    s.dir
        .route(Push::new("dave", Dest::Client, &FriendOnline::default()));
    // A world push for a player in no room.
    s.dir
        .route(Push::new("bob", Dest::World, &BalanceChanged::default()));
    // A client push to a client that cannot read PUSH frames.
    s.dir
        .route(Push::new("carol", Dest::Client, &FriendOnline::default()));
    // The client path full (capacity 1, nobody reading).
    s.dir
        .route(Push::new("alice", Dest::Client, &FriendOnline::default()));
    s.dir
        .route(Push::new("alice", Dest::Client, &FriendOnline::default()));
    // The client path closed.
    drop(s.alice_client);
    s.dir
        .route(Push::new("alice", Dest::Client, &FriendOnline::default()));

    let c = s.dir.counters;
    assert_eq!(c.pushes_offline, 1);
    assert_eq!(c.pushes_world_no_room, 1);
    assert_eq!(c.pushes_unsupported, 1);
    assert_eq!(c.pushes_delivered_client, 1);
    assert_eq!(c.pushes_client_dropped_full, 1);
    assert_eq!(c.pushes_client_dropped_closed, 1);
}

#[test]
fn a_world_push_for_a_room_that_is_gone_is_counted() {
    let s = setup(8);
    let mut dir = s.dir;
    drop(s.room); // the membership's channel closes with the room
    dir.route(Push::new("alice", Dest::World, &BalanceChanged::default()));
    assert_eq!(dir.counters.pushes_world_dropped_closed, 1);
}
