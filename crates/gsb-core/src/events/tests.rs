//! Unit tests of the event table's boot rules, the publishing handle and
//! the routing table (over a stand-in for the registry's tables). The
//! actors around them: `room/tests/events.rs` (a room's EVENTS phase) and
//! `crates/gsb-core/tests/events.rs` (the live registry, connections,
//! rooms and shards).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use bytes::Bytes;

use super::route::{EventRoutes, Live, Locate, Located, Place};
use super::*;
use crate::calls::{BootError, Calls, DecodeError, Identity, WireMessage};
use crate::channel::{Inbox, Mailbox, channel};
use crate::conn::ConnIn;
use crate::id::RoomId;
use crate::registry::RegistryMsg;

// ---- events of the tests ------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
struct Gold {
    #[prost(uint64, tag = "1")]
    balance: u64,
}
impl Event for Gold {
    const OP: u16 = 2001;
    const TO: Dest = Dest::World;
}
impl WorldEvent for Gold {}

#[derive(Clone, PartialEq, prost::Message)]
struct Gold2 {
    #[prost(uint64, tag = "1")]
    balance: u64,
}
impl Event for Gold2 {
    const OP: u16 = 2001;
    const TO: Dest = Dest::World;
}
impl WorldEvent for Gold2 {}

#[derive(Clone, PartialEq, prost::Message)]
struct Low {
    #[prost(uint32, tag = "1")]
    n: u32,
}
impl Event for Low {
    const OP: u16 = 999;
    const TO: Dest = Dest::World;
}
impl WorldEvent for Low {}

#[derive(Clone, PartialEq, prost::Message)]
struct Ask {
    #[prost(uint32, tag = "1")]
    n: u32,
}
impl crate::calls::Call for Ask {
    type Response = Ask;
    type Error = Ask;
}
impl crate::calls::Request for Ask {
    const OP: u16 = 2001;
}

/// An event whose encodes are counted (not a prost message: its own wire
/// form, `len` bytes).
#[derive(Clone, Debug)]
struct Counted(Arc<AtomicU64>, usize);
impl WireMessage for Counted {
    fn to_wire(&self) -> Vec<u8> {
        self.0.fetch_add(1, Relaxed);
        vec![7; self.1]
    }
    fn from_wire(_b: &[u8]) -> Result<Self, DecodeError> {
        Err(DecodeError)
    }
}
impl Event for Counted {
    const OP: u16 = 2010;
    const TO: Dest = Dest::Client;
}
impl ClientEvent for Counted {}

// ---- the table's boot rules ---------------------------------------------------

/// One op space: an event cannot take a request's op, another event's,
/// or one in the engine band — each a typed boot error naming both
/// claimants.
#[test]
fn an_event_op_is_checked_against_requests_events_and_the_engine_band() {
    let mut calls = Calls::default();
    calls.accept::<Ask>().expect("the request claims 2001");
    let mut events = Events::sharing_ops(&calls);
    match events.accept::<Gold>() {
        Err(BootError::EventOpCollision { op, first, second }) => {
            assert_eq!(op, 2001);
            assert!(first.ends_with("Ask") && second.ends_with("Gold"));
        }
        other => panic!("expected a collision with the request: {other:?}"),
    }
    let mut events = Events::default();
    events.accept::<Gold>().expect("free");
    assert!(matches!(
        events.accept::<Gold2>(),
        Err(BootError::EventOpCollision { op: 2001, .. })
    ));
    assert!(matches!(
        events.accept::<Low>(),
        Err(BootError::EventEngineOp { op: 999, .. })
    ));
    assert!(events.handles(2001) && !events.handles(999));
    let shown = BootError::EventEngineOp { op: 999, ty: "Low" }.to_string();
    assert!(shown.contains("engine band"), "{shown}");
}

// ---- the handle -------------------------------------------------------------

fn mint(cap: usize, max: usize) -> (Publishers, Inbox<RegistryMsg>) {
    let (tx, rx) = channel::<RegistryMsg>(cap);
    (Publishers::new(tx, max), rx)
}

/// Every refusal at the handle is typed and counted under its own name;
/// the router's later losses never come back here.
#[test]
fn the_handle_refuses_typed_and_counts_each_refusal() {
    let (mut mint, mut rx) = mint(1, 4);
    let bank = mint.publisher("bank");
    let n = Arc::new(AtomicU64::new(0));
    let alice = Identity::new("alice");
    assert_eq!(
        bank.push(&alice, &Counted(n.clone(), 5)),
        Err(PushRefused::TooLarge { size: 5, max: 4 })
    );
    assert_eq!(bank.push(&alice, &Counted(n.clone(), 4)), Ok(()));
    assert_eq!(
        bank.push(&alice, &Counted(n.clone(), 1)),
        Err(PushRefused::RouterFull)
    );
    rx.close();
    let _ = rx.try_recv();
    assert_eq!(
        bank.push(&alice, &Counted(n.clone(), 1)),
        Err(PushRefused::RouterGone)
    );
    let c = bank.stats().counts();
    assert_eq!(
        (
            c.events_pushed,
            c.events_refused_too_large,
            c.events_refused_router_full,
            c.events_refused_router_gone
        ),
        (1, 1, 1, 1)
    );
}

/// `push_many` to 1000 identities is ONE encode and ONE router message
/// (the body shared by reference count downstream).
#[test]
fn push_many_encodes_once_and_sends_one_message() {
    let (mut mint, mut rx) = mint(8, 1024);
    let chat = mint.publisher("chat");
    let n = Arc::new(AtomicU64::new(0));
    let who: Vec<Identity> = (0..1000).map(|i| Identity::new(&format!("p{i}"))).collect();
    chat.push_many(&who, &Counted(n.clone(), 16))
        .expect("taken");
    assert_eq!(n.load(Relaxed), 1);
    assert_eq!(chat.stats().encodes(), 1);
    let Ok(RegistryMsg::Push(p)) = rx.try_recv() else {
        panic!("one push message")
    };
    assert!(matches!(&p.to, Target::Many(ids) if ids.len() == 1000));
    assert!(rx.try_recv().is_err(), "exactly one message");
}

/// The mint hands out ids from 1 (0 is the engine's) and the same handle
/// for the same name; the collector reads every one.
#[test]
fn the_mint_names_and_numbers_its_handles() {
    let (mut mint, _rx) = mint(8, 1024);
    let bank = mint.publisher("bank");
    let chat = mint.publisher("chat");
    let again = mint.publisher("bank");
    assert_eq!(
        (bank.id(), chat.id(), again.id()),
        (PublisherId(1), PublisherId(2), PublisherId(1))
    );
    assert_eq!(PublisherId::ENGINE, PublisherId(0));
    let names: Vec<String> = mint.sources().iter().map(|s| s.0.to_string()).collect();
    assert_eq!(names, ["bank", "chat"]);
}

// ---- the routing table ----------------------------------------------------------

/// A stand-in for the registry's tables.
#[derive(Default)]
struct At {
    rows: HashMap<String, Row>,
    rooms: HashMap<RoomId, Vec<Mailbox<EventIn>>>,
}

struct Row {
    live: Option<(Mailbox<ConnIn>, u64)>,
    /// `None`: in no room; `Some((room, shard))`.
    place: Option<(RoomId, usize)>,
}

fn row(live: Option<(Mailbox<ConnIn>, u64)>, place: Option<(RoomId, usize)>) -> Row {
    Row { live, place }
}

impl Locate for At {
    fn locate(&self, identity: &str) -> Option<Located<'_>> {
        let row = self.rows.get(identity)?;
        Some(Located {
            live: row.live.as_ref().map(|(inbox, features)| Live {
                inbox,
                features: *features,
            }),
            place: match row.place {
                None => Place::Nowhere,
                Some((room, shard)) => Place::In(self.rooms.get(&room).and_then(|d| d.get(shard))),
            },
        })
    }
    fn members(&self, room: RoomId) -> Vec<String> {
        let mut v: Vec<String> = self
            .rows
            .iter()
            .filter(|(_, r)| r.place.is_some_and(|p| p.0 == room))
            .map(|(k, _)| k.clone())
            .collect();
        v.sort();
        v
    }
    fn identities(&self) -> Vec<String> {
        let mut v: Vec<String> = self.rows.keys().cloned().collect();
        v.sort();
        v
    }
    fn doors(&self, room: RoomId) -> Option<Vec<&Mailbox<EventIn>>> {
        self.rooms.get(&room).map(|d| d.iter().collect())
    }
    fn rooms(&self) -> Vec<RoomId> {
        let mut v: Vec<RoomId> = self.rooms.keys().copied().collect();
        v.sort();
        v
    }
}

fn push(publisher: u32, to: Target, dest: Dest) -> Push {
    Push {
        publisher: PublisherId(publisher),
        to,
        op: 2005,
        dest,
        body: Bytes::from_static(b"body"),
    }
}

fn one(id: &str) -> Target {
    Target::One(Identity::new(id))
}

/// `(publisher, stream, seq)` of every CLIENT leg in `inbox`.
fn client_legs(inbox: &mut Inbox<ConnIn>) -> Vec<(u32, u64, u64)> {
    let mut out = Vec::new();
    while let Ok(ConnIn::Push {
        publisher,
        stream,
        seq,
        ..
    }) = inbox.try_recv()
    {
        out.push((publisher.0, stream, seq));
    }
    out
}

/// `(addressed to a player, seq)` of every WORLD leg in `door`.
fn world_legs(door: &mut Inbox<EventIn>) -> Vec<(bool, u64)> {
    let mut out = Vec::new();
    while let Ok(EventIn::Push(p)) = door.try_recv() {
        out.push((matches!(p.to, Addressee::Player(_)), p.seq));
    }
    out
}

const PUSH: u64 = gsb_protocol::FEATURE_PUSH;

/// A BOTH push runs two legs, each numbered on its own; a WORLD-only push
/// of the same publisher spends no CLIENT number (the client sees no gap
/// it should not).
#[test]
fn both_runs_two_legs_with_their_own_numbers() {
    let (conn_tx, mut conn) = channel::<ConnIn>(16);
    let (room_tx, mut room) = channel::<EventIn>(16);
    let mut at = At::default();
    at.rooms.insert(RoomId(1), vec![room_tx]);
    at.rows.insert(
        "bob".into(),
        row(Some((conn_tx, PUSH)), Some((RoomId(1), 0))),
    );
    let mut r = EventRoutes::default();
    r.route(push(1, one("bob"), Dest::Both), &at);
    r.route(push(1, one("bob"), Dest::World), &at);
    r.route(push(1, one("bob"), Dest::Both), &at);
    r.route(push(2, one("bob"), Dest::Client), &at);
    assert_eq!(client_legs(&mut conn), [(1, 1, 1), (1, 1, 2), (2, 1, 1)]);
    assert_eq!(world_legs(&mut room), [(true, 1), (true, 2), (true, 3)]);
    let c = r.counts();
    assert_eq!(
        (c.events_routed, c.events_client_sent, c.events_world_sent),
        (4, 3, 3)
    );
}

/// Every leg the router cannot deliver is counted by name, and per
/// recipient the counts add up: `offline` once, or one full ending per
/// leg it has.
#[test]
fn the_router_counts_every_leg_it_cannot_deliver() {
    let (full_tx, _full) = channel::<ConnIn>(1);
    full_tx
        .try_send(ConnIn::Shutdown)
        .expect("fill the one slot");
    let (gone_tx, gone) = channel::<ConnIn>(4);
    drop(gone);
    let (old_tx, _old) = channel::<ConnIn>(4);
    let (room_full, _rf) = channel::<EventIn>(1);
    room_full
        .try_send(EventIn::Push(WorldPush {
            to: Addressee::Room,
            publisher: PublisherId(9),
            seq: 1,
            op: 1,
            body: Bytes::new(),
        }))
        .expect("fill");
    let (room_gone, rg) = channel::<EventIn>(4);
    drop(rg);
    let mut at = At::default();
    at.rooms.insert(RoomId(1), vec![room_full]);
    at.rooms.insert(RoomId(2), vec![room_gone]);
    // conn full + room full
    at.rows
        .insert("a".into(), row(Some((full_tx, PUSH)), Some((RoomId(1), 0))));
    // conn gone + room gone
    at.rows
        .insert("b".into(), row(Some((gone_tx, PUSH)), Some((RoomId(2), 0))));
    // an old client (no FEATURE_PUSH) in no room
    at.rows.insert("c".into(), row(Some((old_tx, 0)), None));
    // parked, in a room the router no longer knows
    at.rows.insert("d".into(), row(None, Some((RoomId(3), 0))));
    let mut r = EventRoutes::default();
    let ids: Vec<Identity> = ["a", "b", "c", "d", "nobody", ""]
        .iter()
        .map(|s| Identity::new(s))
        .collect();
    r.route(push(1, Target::Many(ids.into()), Dest::Both), &at);
    let c = r.counts();
    assert_eq!(c.events_offline, 2, "nobody and the anonymous identity");
    assert_eq!(c.events_client_refused_conn_full, 1);
    assert_eq!(c.events_client_refused_conn_gone, 1);
    assert_eq!(c.events_client_unsupported, 1);
    assert_eq!(c.events_client_parked, 1);
    assert_eq!(c.events_world_refused_room_full, 1);
    assert_eq!(
        c.events_world_refused_room_gone, 2,
        "a closed room, an unknown one"
    );
    assert_eq!(c.events_world_no_room, 1);
    // The ledger: 4 recipients with a row × 2 legs, plus 2 offline.
    let client = c.events_client_sent
        + c.events_client_parked
        + c.events_client_unsupported
        + c.events_client_refused_conn_full
        + c.events_client_refused_conn_gone;
    let world = c.events_world_sent
        + c.events_world_no_room
        + c.events_world_refused_room_full
        + c.events_world_refused_room_gone;
    assert_eq!((client, world, c.events_offline), (4, 4, 2));
}

/// The numbers live with the identity's row: a parked leg still spends
/// its number (the client sees the gap on resume); once the row is gone
/// (`forget`), a new life starts a new stream from 1.
#[test]
fn numbers_survive_a_park_and_restart_on_a_new_stream() {
    let (tx, mut conn) = channel::<ConnIn>(16);
    let mut at = At::default();
    at.rows
        .insert("bob".into(), row(Some((tx.clone(), PUSH)), None));
    let mut r = EventRoutes::default();
    r.route(push(1, one("bob"), Dest::Client), &at);
    at.rows.get_mut("bob").expect("row").live = None; // parked
    r.route(push(1, one("bob"), Dest::Client), &at);
    at.rows.get_mut("bob").expect("row").live = Some((tx.clone(), PUSH)); // resumed
    r.route(push(1, one("bob"), Dest::Client), &at);
    assert_eq!(client_legs(&mut conn), [(1, 1, 1), (1, 1, 3)]);
    r.forget("bob");
    r.route(push(1, one("bob"), Dest::Client), &at);
    assert_eq!(client_legs(&mut conn), [(1, 2, 1)]);
    assert_eq!(r.counts().events_client_parked, 1);
}

/// `push_room`: the room's members' clients, and the room's logic once
/// per actor (every shard) under one number per (publisher, room);
/// `push_all`: every identity's client and every room. An unknown room
/// is `world_no_room`; a room that ended numbers anew.
#[test]
fn push_room_and_push_all_address_rooms_and_sessions() {
    let (s0, mut d0) = channel::<EventIn>(8);
    let (s1, mut d1) = channel::<EventIn>(8);
    let (solo, mut d2) = channel::<EventIn>(8);
    let (a_tx, mut a) = channel::<ConnIn>(8);
    let (b_tx, mut b) = channel::<ConnIn>(8);
    let mut at = At::default();
    at.rooms.insert(RoomId(1), vec![s0, s1]);
    at.rooms.insert(RoomId(2), vec![solo]);
    at.rows
        .insert("a".into(), row(Some((a_tx, PUSH)), Some((RoomId(1), 1))));
    at.rows.insert("b".into(), row(Some((b_tx, PUSH)), None));
    let mut r = EventRoutes::default();
    r.route(push(1, Target::Room(RoomId(1)), Dest::Both), &at);
    r.route(push(1, Target::Room(RoomId(1)), Dest::World), &at);
    assert_eq!(world_legs(&mut d0), [(false, 1), (false, 2)]);
    assert_eq!(world_legs(&mut d1), [(false, 1), (false, 2)]);
    assert_eq!(client_legs(&mut a), [(1, 1, 1)]);
    assert!(client_legs(&mut b).is_empty(), "b is in no room");
    r.route(push(1, Target::All, Dest::Both), &at);
    assert_eq!(world_legs(&mut d0), [(false, 3)]);
    assert_eq!(world_legs(&mut d1), [(false, 3)]);
    assert_eq!(world_legs(&mut d2), [(false, 1)]);
    assert_eq!(client_legs(&mut a), [(1, 1, 2)]);
    assert_eq!(client_legs(&mut b), [(1, 2, 1)]);
    r.route(push(1, Target::Room(RoomId(9)), Dest::World), &at);
    assert_eq!(r.counts().events_world_no_room, 1);
    r.room_gone(RoomId(1));
    r.route(push(1, Target::Room(RoomId(1)), Dest::World), &at);
    assert_eq!(
        world_legs(&mut d0),
        [(false, 1)],
        "a new incarnation numbers from 1"
    );
}
