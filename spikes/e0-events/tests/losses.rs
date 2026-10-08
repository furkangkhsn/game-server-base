//! Every loss under its own name (docs/EVENTS.md §9): at the handle, at
//! the router, at the connection, at the room, at the table — and the
//! ledger adds up.

use bytes::Bytes;
use e0_events::contract::{Dest, Event, Identity, PublisherId, RoomId, WorldEvent};
use e0_events::game::{ChatMessage, GoldReceived};
use e0_events::harness::{Link, Server, Wallets};
use e0_events::publisher::{Push, PushRefused, Target};
use e0_events::room::Caps;
use e0_events::router::{Router, RouterMsg};

fn gr() -> GoldReceived {
    GoldReceived {
        from: "x".into(),
        amount: 1,
        balance: 0,
        version: 0,
    }
}

fn chat() -> ChatMessage {
    ChatMessage {
        channel: "c".into(),
        from: "x".into(),
        text: "t".into(),
    }
}

fn id(s: &str) -> Identity {
    Identity::new(s)
}

/// A WORLD event no room accepts.
#[derive(Clone, PartialEq, prost::Message)]
struct QuestDone {
    #[prost(uint32, tag = "1")]
    quest: u32,
}
impl Event for QuestDone {
    const OP: u16 = 2100;
    const TO: Dest = Dest::World;
}
impl WorldEvent for QuestDone {}

#[test]
fn the_handle_refuses_typed_when_the_router_is_full_or_gone() {
    let mut r = Router::new(2);
    let p = r.publisher();
    assert_eq!(p.push(&id("a"), &gr()), Ok(()));
    assert_eq!(p.push(&id("a"), &gr()), Ok(()));
    assert_eq!(p.push(&id("a"), &gr()), Err(PushRefused::RouterFull));
    drop(r);
    assert_eq!(p.push(&id("a"), &gr()), Err(PushRefused::RouterGone));
    let st = p.stats();
    use std::sync::atomic::Ordering::Relaxed;
    assert_eq!(
        (st.pushed.load(Relaxed), st.refused_router_full.load(Relaxed), st.refused_router_gone.load(Relaxed)),
        (2, 1, 1)
    );
}

#[test]
fn the_router_counts_every_leg_it_cannot_deliver() {
    let mut s = Server::<Wallets>::new(256);
    s.room(RoomId(1), Wallets::default(), Caps { events: 2, ..Caps::default() });
    let p = s.router.publisher();
    s.connect("in_room", Link::default());
    s.join("in_room", RoomId(1), 0, 8, 0);
    s.connect("lobby", Link::default()); // online, no room
    s.connect("old", Link { features: 0, ..Link::default() }); // no FEATURE_PUSH
    s.connect("parked", Link::default());
    s.join("parked", RoomId(1), 0, 8, 0);
    s.connect("tiny", Link { inbox: 1, ..Link::default() });
    s.step();
    s.park("parked");
    s.step();

    p.push(&id("nobody"), &chat()).unwrap(); // offline
    p.push(&id("lobby"), &gr()).unwrap(); // world_no_room
    p.push(&id("old"), &chat()).unwrap(); // client_unsupported
    p.push(&id("parked"), &chat()).unwrap(); // client_parked
    for _ in 0..3 {
        p.push(&id("tiny"), &chat()).unwrap(); // 1 sent, 2 conn full (not pumped between)
    }
    for _ in 0..4 {
        p.push(&id("in_room"), &gr()).unwrap(); // room mailbox of 2: 2 sent, 2 full
    }
    s.router.pump();
    let c = s.router.counts().clone();
    assert_eq!(c.offline, 1);
    assert_eq!(c.world_no_room, 1);
    assert_eq!(c.client_unsupported, 1);
    assert_eq!(c.client_parked, 1);
    assert_eq!((c.client_sent, c.client_refused_conn_full), (1, 2));
    assert_eq!((c.world_sent, c.world_refused_room_full), (2, 2));

    // A room that is gone.
    s.router.handle(RouterMsg::DropRoom { room: RoomId(1) });
    p.push(&id("in_room"), &gr()).unwrap();
    s.router.pump();
    assert_eq!(s.router.counts().world_refused_room_gone, 1);

    // The ledger: every identity-addressed leg ended under one name.
    let c = s.router.counts();
    let legs_ended = c.offline
        + c.client_sent
        + c.client_parked
        + c.client_unsupported
        + c.client_refused_conn_full
        + c.client_refused_conn_gone
        + c.world_sent
        + c.world_no_room
        + c.world_refused_room_full
        + c.world_refused_room_gone;
    assert_eq!(legs_ended, c.pushes, "each push here had exactly one leg");
}

#[test]
fn the_connection_counts_a_full_outbound() {
    let mut s = Server::<Wallets>::new(64);
    let p = s.router.publisher();
    s.connect("slow", Link { outbound: 1, ..Link::default() });
    for _ in 0..3 {
        p.push(&id("slow"), &chat()).unwrap();
    }
    s.router.pump();
    let slow = s.players.get_mut(&id("slow")).unwrap();
    slow.actor.pump(); // the writer never drained
    let c = slow.actor.counts();
    assert_eq!((c.pushes_written, c.pushes_dropped_outbound_full), (1, 2));
    s.step();
    // The client sees seq 1 and then, on the next one, a gap of 2.
    p.push(&id("slow"), &chat()).unwrap();
    s.step();
    let gaps = s.client("slow").take_gaps();
    assert_eq!(gaps.iter().map(|g| g.missed).sum::<u64>(), 2);
}

#[test]
fn the_table_counts_no_handler_undecodable_and_untaken() {
    let mut s = Server::<Wallets>::new(64);
    s.room(RoomId(1), Wallets::default(), Caps::default());
    s.room(RoomId(2), Wallets { deaf: true, ..Wallets::default() }, Caps::default());
    let p = s.router.publisher();
    s.connect("a", Link::default());
    s.join("a", RoomId(1), 0, 8, 0);
    s.connect("b", Link::default());
    s.join("b", RoomId(2), 0, 8, 0);
    s.step();

    p.push(&id("a"), &QuestDone { quest: 7 }).unwrap(); // room 1 never accepted 2100
    // A body that is not a GoldReceived (only a buggy publisher could;
    // the typed handle cannot produce one).
    s.router.handle(RouterMsg::Push(Push {
        publisher: PublisherId(99),
        to: Target::One(id("a")),
        op: <GoldReceived as Event>::OP,
        dest: Dest::World,
        body: Bytes::from_static(&[0xff, 0xff, 0xff]),
    }));
    p.push(&id("b"), &gr()).unwrap(); // room 2 takes nothing
    p.push(&id("b"), &gr()).unwrap();
    s.step();
    let a = s.shard(RoomId(1), 0).events().counts().clone();
    assert_eq!((a.no_handler, a.undecodable, a.admitted), (1, 1, 0));
    let b = s.shard(RoomId(2), 0).events().counts().clone();
    assert_eq!((b.admitted, b.taken, b.untaken), (2, 0, 2));
    // Dropped, not kept: the next tick starts empty.
    s.step();
    assert_eq!(s.shard(RoomId(2), 0).events().counts().untaken, 2);
}
