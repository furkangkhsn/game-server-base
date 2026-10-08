//! Events through the live engine (round e2, `docs/EVENTS.md`): real
//! connection actors over a live registry — the router — with single
//! rooms and a sharded room ticking on the global ticker. A publisher
//! pushes; the tests watch what the rooms' logics take, what the
//! clients receive as `PUSH` frames, and what every actor counted.
//!
//! - an identity's WORLD leg reaches the room — or the shard — that
//!   holds it, never another;
//! - a CLIENT leg goes straight to the connection, through no room, and
//!   reaches a player in no room at all;
//! - a BOTH event runs two legs, each numbered on its own;
//! - `push_room` / `push_all` address the rooms' logics (every shard
//!   once) and their members' clients;
//! - every leg that is not delivered is counted under its own name, and
//!   the counts add up across the router and the connections;
//! - a client never pushes: a `PUSH` frame from a client is a hard
//!   protocol violation.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gsb_core::calls::Identity;
use gsb_core::channel::{FrameBatch, Mailbox, channel};
use gsb_core::conn::{ConnIn, ConnectionActor};
use gsb_core::events::{DEFAULT_MAX_EVENT_BYTES, Publisher, Publishers};
use gsb_core::id::{ConnectionId, RoomId};
use gsb_core::metrics::{MetricsEvent, PushCounts, RouterCounts};
use gsb_core::registry::{BuiltRoom, Registry, RegistryMsg, RoomFactory};
use gsb_core::room::{RoomConfig, RoomLogic};
use gsb_core::shard::ShardLogic;
use gsb_core::ticker::Ticker;
use gsb_core::events::Event;
use gsb_protocol::{FEATURE_PUSH, base, base_table, op};
use prost::Message;
use tokio::sync::mpsc;

#[path = "events/logic.rs"]
mod logic;

use logic::{BalanceChanged, Boom, ChatLine, EvLogic, GoldReceived, Hop, SHARDS, SPAN, Took};

const WAIT: Duration = Duration::from_secs(5);
/// The sharded room's id; every other id is a single room.
const GRID: RoomId = RoomId(9);

/// Single rooms report their id as the "shard"; room [`GRID`] is three
/// shards, `neo` homed on shard 1 and `bob` on shard 2. Rooms park a
/// disconnected player when `park`.
fn factory(seen: logic::Seen, park: bool) -> RoomFactory<(), (), (), ()> {
    Arc::new(move |id, _cfg| {
        if id == GRID {
            let shards = (0..SHARDS)
                .map(|index| {
                    let l = EvLogic::new(index, park, seen.clone(), true);
                    (
                        (),
                        Box::new(l)
                            as Box<dyn ShardLogic<(), GroupKey = (), State = (), Strip = ()>>,
                    )
                })
                .collect();
            BuiltRoom::Sharded {
                shards,
                home_shard: Arc::new(|_c, identity: &str| match identity {
                    "neo" => 1,
                    "bob" => 2,
                    _ => 0,
                }),
            }
        } else {
            BuiltRoom::Single {
                world: (),
                logic: Box::new(EvLogic::new(id.0 as usize, park, seen.clone(), false))
                    as Box<dyn RoomLogic<(), GroupKey = (), Strip = ()>>,
            }
        }
    })
}

/// A live server: the registry (rooms 1, 2 and the grid created), a
/// publisher mint, what the rooms took, and the metrics stream.
struct Server {
    reg: Mailbox<RegistryMsg>,
    mint: Publishers,
    took: mpsc::UnboundedReceiver<Took>,
    metrics: mpsc::Receiver<MetricsEvent>,
    /// The connection actors report here too.
    metrics_tx: mpsc::Sender<MetricsEvent>,
    router: RouterCounts,
    pushes: PushCounts,
}

impl Server {
    async fn start(park: bool) -> Self {
        let (seen, took) = mpsc::unbounded_channel();
        let (tx, rx) = channel::<RegistryMsg>(4096);
        let (ticker, _task) = Ticker::spawn(60.0, 64).expect("valid tick rate");
        let (metrics_tx, metrics) = mpsc::channel::<MetricsEvent>(65_536);
        tokio::spawn(
            Registry::new(
                rx,
                tx.clone(),
                factory(seen, park),
                ticker,
                metrics_tx.clone(),
                None,
                None,
                None,
            )
            .run(),
        );
        for id in [RoomId(1), RoomId(2), GRID] {
            let (reply, created) = tokio::sync::oneshot::channel();
            let config = RoomConfig {
                id,
                tick_hz: 60.0,
                // Room 2 comes back after a panic (§7.4).
                restart_on_panic: id == RoomId(2),
                ..Default::default()
            };
            tx.send(RegistryMsg::CreateRoom { config, reply })
                .await
                .expect("registry alive");
            let created = tokio::time::timeout(WAIT, created).await.expect("in time");
            created.expect("reply").expect("room created");
        }
        Self {
            mint: Publishers::new(tx.clone(), DEFAULT_MAX_EVENT_BYTES),
            reg: tx,
            took,
            metrics,
            metrics_tx,
            router: RouterCounts::default(),
            pushes: PushCounts::default(),
        }
    }

    /// The registry's member count of `room`.
    async fn members(&self, room: RoomId) -> u32 {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.reg
            .send(RegistryMsg::RoomStatus { id: room, reply })
            .await
            .expect("registry alive");
        match rx.await.expect("status") {
            gsb_core::RoomStatus::Running { members } => members,
            other => panic!("room {room:?} not running: {other:?}"),
        }
    }

    fn publisher(&mut self, name: &str) -> Publisher {
        self.mint.publisher(name)
    }

    /// The next `n` events the rooms took.
    async fn took(&mut self, n: usize) -> Vec<Took> {
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            let t = tokio::time::timeout(WAIT, self.took.recv())
                .await
                .expect("an event taken in time")
                .expect("reports open");
            out.push(t);
        }
        out
    }

    /// Nothing more taken for a few ticks.
    async fn took_nothing(&mut self) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(self.took.try_recv().ok(), None, "no room took anything");
    }

    /// Fold the metrics until `done` holds of the router's latest counts
    /// and the connections' pushes so far.
    async fn counts_until(
        &mut self,
        done: impl Fn(&RouterCounts, &PushCounts) -> bool,
    ) -> (RouterCounts, PushCounts) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            while let Ok(ev) = self.metrics.try_recv() {
                match ev {
                    MetricsEvent::Registry(s) => self.router = s.events,
                    MetricsEvent::Conn(s) => self.pushes.add(&s.pushes),
                    _ => {}
                }
            }
            if done(&self.router, &self.pushes) {
                return (self.router, self.pushes);
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "counts in time: {:?} {:?}",
                self.router,
                self.pushes
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

/// A client: its connection actor's inbox and its outbound frames.
struct Client {
    inbox: Mailbox<ConnIn>,
    out: mpsc::Receiver<FrameBatch>,
}

impl Client {
    /// Open `conn`, authenticate as `name` announcing `features`, join
    /// `room` when given.
    async fn login(
        server: &Server,
        conn: u64,
        name: &str,
        features: u64,
        room: Option<RoomId>,
    ) -> Self {
        let (inbox_tx, inbox) = channel::<ConnIn>(256);
        let (out_tx, out) = channel::<FrameBatch>(256);
        server
            .reg
            .send(RegistryMsg::ConnOpened {
                conn: ConnectionId(conn),
                inbox: inbox_tx.clone(),
                source: None,
            })
            .await
            .expect("registry alive");
        let addr = SocketAddr::from(([127, 0, 0, 1], 43_000 + conn as u16));
        let actor = ConnectionActor::new(
            ConnectionId(conn),
            addr,
            Arc::new(base_table()),
            server.reg.clone(),
            inbox,
            out_tx,
            server.metrics_tx.clone(),
            None,
        );
        tokio::spawn(actor.run());
        let mut client = Self {
            inbox: inbox_tx,
            out,
        };
        let auth = base::Auth {
            name: name.into(),
            ticket: Vec::new(),
            protocol_version: 0,
            features,
        };
        client.send(op::base::AUTH_REQ, auth.encode_to_vec()).await;
        client.expect(op::base::AUTH_RESULT).await;
        if let Some(room) = room {
            let before = server.members(room).await;
            let j = base::JoinRoom { room_id: room.0 };
            client
                .send(op::base::JOIN_ROOM_REQ, j.encode_to_vec())
                .await;
            client.expect(op::base::JOIN_ROOM_RESULT).await;
            // The client hears its join before the registry settles it
            // (the dispatcher answers, then reports): a push routed in
            // between finds no room (E13). Wait for the settlement.
            let deadline = tokio::time::Instant::now() + WAIT;
            while server.members(room).await <= before {
                assert!(tokio::time::Instant::now() < deadline, "join settled");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        client
    }

    async fn send(&self, code: u16, body: Vec<u8>) {
        let frame = gsb_protocol::FrameBody::new(code, body);
        self.inbox
            .send(ConnIn::Frame(frame))
            .await
            .expect("actor alive");
    }

    /// Frames until one of `code` (an `ERROR` first fails the test).
    async fn expect(&mut self, code: u16) -> gsb_protocol::FrameBody {
        loop {
            let batch = tokio::time::timeout(WAIT, self.out.recv()).await;
            let batch = batch.expect("a frame in time").expect("out open");
            for f in batch {
                if f.op == code {
                    return f;
                }
                assert_ne!(f.op, op::base::ERROR, "unexpected ERROR waiting for {code}");
            }
        }
    }

    /// The next `n` PUSH frames.
    async fn pushes(&mut self, n: usize) -> Vec<base::Push> {
        let mut out = Vec::new();
        while out.len() < n {
            let f = self.expect(op::base::PUSH).await;
            out.push(base::Push::decode(f.payload.as_ref()).expect("a Push"));
        }
        out
    }

    /// No PUSH frame for a few ticks.
    async fn no_push(&mut self) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        while let Ok(batch) = self.out.try_recv() {
            assert!(
                batch.iter().all(|f| f.op != op::base::PUSH),
                "no PUSH expected"
            );
        }
    }

    /// End the session from the client's side (the actor's final flush).
    async fn close(&self) {
        let _ = self
            .inbox
            .send(ConnIn::Closed {
                reason: "test".into(),
            })
            .await;
    }
}

fn id(s: &str) -> Identity {
    Identity::new(s)
}

fn took(shard: usize, op: u16, to: Option<&str>, owner: u64, from: u32, seq: u64) -> Took {
    Took {
        shard,
        op,
        to: to.map(str::to_owned),
        owner,
        from,
        seq,
    }
}

/// An identity's WORLD leg reaches the room that holds it — not the
/// room the publisher or anyone else is in — owned by its player.
#[tokio::test]
async fn an_event_reaches_its_player_in_its_own_room() {
    let mut server = Server::start(false).await;
    let _alice = Client::login(&server, 1, "alice", FEATURE_PUSH, Some(RoomId(1))).await;
    let _bob = Client::login(&server, 2, "bob", FEATURE_PUSH, Some(RoomId(2))).await;
    let bank = server.publisher("bank");
    bank.push(&id("bob"), &GoldReceived { balance: 500 })
        .expect("taken");
    bank.push(&id("alice"), &GoldReceived { balance: 7 })
        .expect("taken");
    let mut got = server.took(2).await;
    got.sort_by_key(|t| t.shard);
    // Room 1's first player is 1, room 2's is 2 001 (`index * SPAN + 1`).
    assert_eq!(
        got,
        [
            took(1, 2001, Some("alice"), SPAN + 1, 1, 1),
            took(2, 2001, Some("bob"), 2 * SPAN + 1, 1, 1),
        ]
    );
}

/// In a sharded room an identity's WORLD leg goes to the shard that took
/// its join (here its home shard), and to no other shard.
#[tokio::test]
async fn a_sharded_room_routes_to_the_shard_that_holds_the_member() {
    let mut server = Server::start(false).await;
    let _neo = Client::login(&server, 1, "neo", FEATURE_PUSH, Some(GRID)).await;
    let _bob = Client::login(&server, 2, "bob", FEATURE_PUSH, Some(GRID)).await;
    let bank = server.publisher("bank");
    bank.push_many(
        &[id("bob"), id("neo"), id("bob")],
        &GoldReceived { balance: 1 },
    )
    .expect("taken");
    let mut got = server.took(3).await;
    got.sort_by_key(|t| (t.shard, t.seq));
    assert_eq!(
        got,
        [
            took(1, 2001, Some("neo"), SPAN + 1, 1, 1),
            took(2, 2001, Some("bob"), 2 * SPAN + 1, 1, 1),
            // A duplicate in the list is two legs, two numbers (E4).
            took(2, 2001, Some("bob"), 2 * SPAN + 1, 1, 2),
        ]
    );
    server.took_nothing().await;
}

/// A CLIENT event goes straight to the connection — no room sees it —
/// and reaches a player in no room at all; the frame carries the
/// publisher, the stream and the leg's number.
#[tokio::test]
async fn a_client_event_bypasses_every_room() {
    let mut server = Server::start(false).await;
    let mut bob = Client::login(&server, 1, "bob", FEATURE_PUSH, Some(RoomId(1))).await;
    let mut eve = Client::login(&server, 2, "eve", FEATURE_PUSH, None).await;
    let chat = server.publisher("chat");
    let line = ChatLine { text: "hi".into() };
    chat.push_many(&[id("bob"), id("eve")], &line)
        .expect("taken");
    chat.push(&id("bob"), &line).expect("taken");
    let got = bob.pushes(2).await;
    assert_eq!(
        got.iter()
            .map(|p| (p.publisher, p.op, p.seq))
            .collect::<Vec<_>>(),
        [(1, 2002, 1), (1, 2002, 2)]
    );
    assert!(got[0].stream > 0 && got[0].stream == got[1].stream);
    assert_eq!(ChatLine::decode(got[0].body.as_ref()).expect("body"), line);
    let got = eve.pushes(1).await;
    assert_eq!((got[0].op, got[0].seq), (2002, 1));
    assert_ne!(got[0].stream, bob_stream(&mut bob, &chat).await);
    server.took_nothing().await;
    let (r, _) = server.counts_until(|r, _| r.events_client_sent == 4).await;
    assert_eq!(r.events_world_sent, 0);
}

/// Bob's stream (one more push).
async fn bob_stream(bob: &mut Client, chat: &Publisher) -> u64 {
    chat.push(&id("bob"), &ChatLine { text: "s".into() })
        .expect("taken");
    bob.pushes(1).await[0].stream
}

/// A BOTH event runs two legs, numbered apart: the room counts its WORLD
/// legs (a WORLD-only event of the same publisher included), the client
/// its CLIENT legs — no gap the client should not see.
#[tokio::test]
async fn both_runs_two_legs_with_independent_numbers() {
    let mut server = Server::start(false).await;
    let mut bob = Client::login(&server, 1, "bob", FEATURE_PUSH, Some(RoomId(1))).await;
    let shop = server.publisher("shop");
    shop.push(&id("bob"), &BalanceChanged { balance: 1 })
        .expect("taken");
    shop.push(&id("bob"), &GoldReceived { balance: 2 })
        .expect("taken");
    shop.push(&id("bob"), &BalanceChanged { balance: 3 })
        .expect("taken");
    let room: Vec<(u16, u64)> = server.took(3).await.iter().map(|t| (t.op, t.seq)).collect();
    assert_eq!(room, [(2005, 1), (2001, 2), (2005, 3)]);
    let client: Vec<(u32, u64)> = bob.pushes(2).await.iter().map(|p| (p.op, p.seq)).collect();
    assert_eq!(client, [(2005, 1), (2005, 2)]);
    bob.no_push().await;
}

/// `push_room` reaches the room's logic once per actor — every shard of
/// a sharded room — and its members' clients; `push_all` every room and
/// every session.
#[tokio::test]
async fn push_room_and_push_all_reach_rooms_shards_and_sessions() {
    let mut server = Server::start(false).await;
    let mut neo = Client::login(&server, 1, "neo", FEATURE_PUSH, Some(GRID)).await;
    let mut eve = Client::login(&server, 2, "eve", FEATURE_PUSH, None).await;
    let ops = server.publisher("ops");
    ops.push_room(GRID, &BalanceChanged { balance: 1 })
        .expect("taken");
    let mut got = server.took(SHARDS).await;
    got.sort_by_key(|t| t.shard);
    let want: Vec<Took> = (0..SHARDS)
        .map(|s| took(s, 2005, None, u64::MAX, 1, 1))
        .collect();
    assert_eq!(got, want, "every shard once, owned by the actor");
    assert_eq!(neo.pushes(1).await[0].seq, 1);
    eve.no_push().await;
    ops.push_all(&BalanceChanged { balance: 2 }).expect("taken");
    let mut got = server.took(SHARDS + 2).await;
    got.sort_by_key(|t| (t.shard, t.seq));
    let rooms: Vec<(usize, u64)> = got.iter().map(|t| (t.shard, t.seq)).collect();
    // Rooms 1 and 2 (their ids as shards) number from 1; the grid's
    // shards continue the grid's numbers.
    assert_eq!(rooms, [(0, 2), (1, 1), (1, 2), (2, 1), (2, 2)]);
    assert_eq!(neo.pushes(1).await[0].seq, 2);
    assert_eq!(eve.pushes(1).await[0].seq, 1);
}

/// Every leg that is not delivered is counted under its own name, and
/// the counts add up — per recipient `offline`, or one full ending per
/// leg; and `client_sent` = the connections' `pushes_written` + their
/// losses. A parked player's CLIENT leg is counted parked while its WORLD
/// leg still reaches its room (E1).
#[tokio::test]
async fn every_lost_leg_is_counted_and_the_counts_add_up() {
    let mut server = Server::start(true).await;
    let mut bob = Client::login(&server, 1, "bob", FEATURE_PUSH, Some(RoomId(1))).await;
    let old = Client::login(&server, 2, "old", 0, Some(RoomId(1))).await;
    let mut eve = Client::login(&server, 3, "eve", FEATURE_PUSH, None).await;
    let carl = Client::login(&server, 4, "carl", FEATURE_PUSH, Some(RoomId(2))).await;
    // Carl's transport dies: his room parks him. Probe with a CLIENT leg
    // until the registry has marked his row parked (his detach settled).
    carl.close().await;
    let probe = server.publisher("probe");
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let before = server.counts_until(|_, _| true).await.0.events_routed;
        probe
            .push(
                &id("carl"),
                &ChatLine {
                    text: String::new(),
                },
            )
            .expect("taken");
        let (r, _) = server.counts_until(|r, _| r.events_routed > before).await;
        if r.events_client_parked > 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "carl parked in time"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let base = server.counts_until(|_, _| true).await.0;
    let shop = server.publisher("shop");
    let all = [id("bob"), id("old"), id("eve"), id("carl"), id("ghost")];
    shop.push_many(&all, &BalanceChanged { balance: 9 })
        .expect("taken");
    // The WORLD legs that reached a room: bob's and old's (room 1),
    // carl's (room 2, parked).
    let mut got = server.took(3).await;
    got.sort_by_key(|t| t.to.clone());
    let to: Vec<Option<String>> = got.iter().map(|t| t.to.clone()).collect();
    assert_eq!(
        to,
        [Some("bob".into()), Some("carl".into()), Some("old".into())]
    );
    assert_eq!(bob.pushes(1).await.len(), 1);
    assert_eq!(eve.pushes(1).await.len(), 1);
    let (r, _) = server
        .counts_until(|r, _| r.events_routed == base.events_routed + 1)
        .await;
    let d = |f: fn(&RouterCounts) -> u64| f(&r) - f(&base);
    assert_eq!(d(|c| c.events_offline), 1, "ghost");
    assert_eq!(d(|c| c.events_client_sent), 2, "bob, eve");
    assert_eq!(d(|c| c.events_client_unsupported), 1, "old");
    assert_eq!(d(|c| c.events_client_parked), 1, "carl");
    assert_eq!(d(|c| c.events_world_sent), 3, "bob, old, carl");
    assert_eq!(d(|c| c.events_world_no_room), 1, "eve");
    let client = d(|c| c.events_client_sent)
        + d(|c| c.events_client_parked)
        + d(|c| c.events_client_unsupported)
        + d(|c| c.events_client_refused_conn_full)
        + d(|c| c.events_client_refused_conn_gone);
    let world = d(|c| c.events_world_sent)
        + d(|c| c.events_world_no_room)
        + d(|c| c.events_world_refused_room_full)
        + d(|c| c.events_world_refused_room_gone);
    assert_eq!((client, world, d(|c| c.events_offline)), (4, 4, 1));
    // The connections' side: every CLIENT leg sent was written (or lost
    // by name) — read off their final samples.
    bob.close().await;
    old.close().await;
    eve.close().await;
    let (r, p) = server
        .counts_until(|r, p| {
            p.pushes_written
                + p.pushes_dropped_outbound_full
                + p.pushes_dropped_outbound_gone
                + p.pushes_dropped_unread
                == r.events_client_sent
        })
        .await;
    assert_eq!(p.pushes_written, r.events_client_sent, "{r:?} {p:?}");
}

/// A client never pushes: a `PUSH` frame from a client is an unknown
/// inbound base-band opcode — a hard violation, answered `ERROR`.
#[tokio::test]
async fn a_push_frame_from_a_client_is_a_hard_violation() {
    let server = Server::start(false).await;
    let mut bob = Client::login(&server, 1, "bob", FEATURE_PUSH, None).await;
    let forged = base::Push {
        publisher: 1,
        seq: 1,
        op: 2002,
        body: Vec::new(),
        stream: 1,
    };
    // Four hard violations exhaust the budget (16 = 4 × 4): the fourth
    // closes the connection.
    for _ in 0..4 {
        bob.send(op::base::PUSH, forged.encode_to_vec()).await;
    }
    let mut errors = Vec::new();
    // Until the actor ends (its outbound closes).
    while let Some(batch) = tokio::time::timeout(WAIT, bob.out.recv())
        .await
        .expect("in time")
    {
        errors.extend(
            batch
                .into_iter()
                .filter(|f| f.op == op::base::ERROR)
                .map(|f| base::Error::decode(f.payload.as_ref()).expect("Error").code),
        );
    }
    let unknown = base::ErrorCode::UnknownOpcode as i32;
    let closed = base::ErrorCode::ServerClosed as i32;
    assert_eq!(errors.first(), Some(&unknown), "{errors:?}");
    assert_eq!(
        errors.last(),
        Some(&closed),
        "the budget closed it: {errors:?}"
    );
}

/// The numbers live with the identity's row: a session that ends with
/// nothing parked takes them along; the identity's next session is a new
/// stream, numbered from 1 again (E5).
#[tokio::test]
async fn a_new_life_is_a_new_stream_from_one() {
    let mut server = Server::start(false).await;
    let chat = server.publisher("chat");
    let line = ChatLine { text: "x".into() };
    let mut first = Client::login(&server, 1, "bob", FEATURE_PUSH, None).await;
    chat.push(&id("bob"), &line).expect("taken");
    chat.push(&id("bob"), &line).expect("taken");
    let got = first.pushes(2).await;
    assert_eq!((got[0].seq, got[1].seq), (1, 2));
    let stream = got[0].stream;
    first.close().await;
    // Offline once the registry dropped the row.
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let before = server.counts_until(|_, _| true).await.0;
        chat.push(&id("bob"), &line).expect("taken");
        let (r, _) = server
            .counts_until(|r, _| r.events_routed > before.events_routed)
            .await;
        if r.events_offline > before.events_offline {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "bob offline in time"
        );
    }
    let mut second = Client::login(&server, 2, "bob", FEATURE_PUSH, None).await;
    chat.push(&id("bob"), &line).expect("taken");
    let got = second.pushes(1).await;
    assert_eq!(got[0].seq, 1);
    assert_ne!(got[0].stream, stream, "a new stream");
}

/// The connection's side of the ledger: a CLIENT leg its full outbound
/// refuses, one for a gone writer, and one still in its inbox when the
/// session ends are each counted under their own name (the final
/// sample), never written.
#[tokio::test]
async fn a_connection_counts_every_push_it_cannot_write() {
    let push = |seq| ConnIn::Push {
        publisher: gsb_core::events::PublisherId(1),
        stream: 1,
        seq,
        op: 2002,
        body: bytes::Bytes::from_static(b"x"),
    };
    let closed = || ConnIn::Closed {
        reason: "test".into(),
    };
    // An outbound already full: the first leg is refused; the one queued
    // behind the session's end is never read.
    let full = conn_pushes(false, vec![push(1), closed(), push(2)]).await;
    assert_eq!(
        (
            full.pushes_written,
            full.pushes_dropped_outbound_full,
            full.pushes_dropped_unread
        ),
        (0, 1, 1)
    );
    // A writer already gone.
    let gone = conn_pushes(true, vec![push(1), closed()]).await;
    assert_eq!(
        (gone.pushes_written, gone.pushes_dropped_outbound_gone),
        (0, 1)
    );
}

/// Run a connection actor over `inbox` (all queued before it starts) with
/// a one-slot outbound that is full — or whose writer is `gone` — and
/// sum the PUSH counters of its samples.
async fn conn_pushes(gone: bool, inbox: Vec<ConnIn>) -> PushCounts {
    let (inbox_tx, inbox_rx) = channel::<ConnIn>(16);
    let (out_tx, out) = channel::<FrameBatch>(1);
    let (metrics_tx, mut metrics) = mpsc::channel::<MetricsEvent>(64);
    // No registry: its mailbox has no reader (the actor's notices fail).
    let (reg, _) = channel::<RegistryMsg>(1);
    for msg in inbox {
        inbox_tx.try_send(msg).expect("room in the inbox");
    }
    // The writer's end: dropped (gone), or kept for the run with its one
    // slot taken (full).
    let _writer = if gone {
        drop(out);
        None
    } else {
        out_tx.try_send(Vec::new()).expect("fill the one slot");
        Some(out)
    };
    let actor = ConnectionActor::new(
        ConnectionId(1),
        SocketAddr::from(([127, 0, 0, 1], 44_001)),
        Arc::new(base_table()),
        reg,
        inbox_rx,
        out_tx,
        metrics_tx,
        None,
    );
    tokio::time::timeout(WAIT, actor.run())
        .await
        .expect("the actor ends");
    let mut pushes = PushCounts::default();
    while let Ok(ev) = metrics.try_recv() {
        if let MetricsEvent::Conn(s) = ev {
            pushes.add(&s.pushes);
        }
    }
    pushes
}

// ---- location changes (round e3, `docs/EVENTS.md` §7) ---------------------------

/// A member walks across shards while a publisher keeps pushing (a
/// `Hop` the shard's game takes moves bob from shard 2 to 0 in that
/// tick's MIGRATE): the live registry learns the move (`Relocated`),
/// seals shard 2, and every leg — those that still reached shard 2 and
/// those sent to shard 0 directly — is taken exactly once, in order.
#[tokio::test]
async fn a_shard_migration_keeps_every_leg_in_order() {
    let mut server = Server::start(false).await;
    let _bob = Client::login(&server, 1, "bob", FEATURE_PUSH, Some(GRID)).await;
    let bank = server.publisher("bank");
    for _ in 0..3 {
        bank.push(&id("bob"), &GoldReceived { balance: 1 })
            .expect("taken");
    }
    bank.push(&id("bob"), &Hop { to: 0 }).expect("taken"); // seq 4
    // Keep pushing across the move: some legs reach shard 2 after bob
    // left it (forwarded), some go to shard 0 directly (held until the
    // fence).
    for _ in 0..40 {
        bank.push(&id("bob"), &GoldReceived { balance: 1 })
            .expect("taken");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let got = server.took(44).await;
    let seqs: Vec<u64> = got.iter().map(|t| t.seq).collect();
    assert_eq!(seqs, (1..=44).collect::<Vec<u64>>(), "every leg, in order");
    assert_eq!((got[3].op, got[3].shard), (Hop::OP, 2));
    assert_eq!(got[43].shard, 0, "bob is on shard 0 now");
    server.took_nothing().await;
    let (r, _) = server
        .counts_until(|r, _| r.events_relocations == 1 && r.events_routed == 44)
        .await;
    assert_eq!(
        (
            r.events_seals_sent,
            r.events_seals_refused,
            r.events_relocations_stale,
            r.events_world_sent
        ),
        (1, 0, 0, 44)
    );
    // Late or foreign reports change nothing: an older count, another
    // incarnation of the room, an identity with no membership there.
    let stale = |generation, identity: &str, mig| RegistryMsg::Relocated {
        room: GRID,
        generation,
        identity: identity.into(),
        shard: 1,
        mig,
    };
    for msg in [stale(0, "bob", 1), stale(9, "bob", 5), stale(0, "eve", 5)] {
        server.reg.send(msg).await.expect("registry alive");
    }
    bank.push(&id("bob"), &GoldReceived { balance: 1 })
        .expect("taken");
    let got = server.took(1).await;
    assert_eq!((got[0].shard, got[0].seq), (0, 45), "still shard 0");
    let (r, _) = server
        .counts_until(|r, _| r.events_routed == 45)
        .await;
    assert_eq!((r.events_relocations, r.events_relocations_stale, r.events_seals_sent), (1, 3, 1));
}

/// Park and resume: the CLIENT leg pushed while bob is parked is counted
/// (`events_client_parked`), its WORLD leg still reaches his room, and
/// his next session continues the same stream — the client sees the
/// gap (1, then 3) and re-syncs.
#[tokio::test]
async fn a_resumed_client_sees_the_gap_its_park_left() {
    let mut server = Server::start(true).await;
    let mut bob = Client::login(&server, 1, "bob", FEATURE_PUSH, Some(RoomId(1))).await;
    let chat = server.publisher("chat");
    let line = |t: &str| ChatLine { text: t.into() };
    chat.push(&id("bob"), &line("hi")).expect("taken");
    let first = bob.pushes(1).await;
    assert_eq!(first[0].seq, 1);
    bob.close().await;
    // Parked once the registry marked his row (a CLIENT leg says so).
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let before = server.counts_until(|_, _| true).await.0;
        chat.push(&id("bob"), &line("missed")).expect("taken");
        let (r, _) = server
            .counts_until(|r, _| r.events_routed > before.events_routed)
            .await;
        if r.events_client_parked > before.events_client_parked {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "parked in time");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let bank = server.publisher("bank");
    bank.push(&id("bob"), &GoldReceived { balance: 70 })
        .expect("taken");
    let got = server.took(1).await;
    assert_eq!((got[0].shard, got[0].seq), (1, 1), "the parked entity's room took it");
    // Bob comes back: a new session of the identity while its park row
    // holds the numbers (the CLIENT leg needs no room).
    let mut back = Client::login(&server, 2, "bob", FEATURE_PUSH, None).await;
    chat.push(&id("bob"), &line("back")).expect("taken");
    let got = back.pushes(1).await;
    // 1 written; every probe lost on the way (parked, or refused by the
    // closing connection) is a number spent: a gap.
    assert!(got[0].seq >= 3, "a gap: {}", got[0].seq);
    let (r, _) = server
        .counts_until(|r, _| {
            r.events_client_sent
                + r.events_client_parked
                + r.events_client_refused_conn_gone
                + r.events_client_refused_conn_full
                == got[0].seq
        })
        .await;
    assert!(r.events_client_parked >= 1);
    assert_eq!(got[0].stream, first[0].stream, "the same stream: a gap, not a restart");
}

/// A room change (leave → join): a WORLD leg between the rooms is
/// counted `events_world_no_room` and lost (E13) — never delivered late;
/// the next one reaches the new room with the next number.
#[tokio::test]
async fn a_room_change_counts_the_leg_between_rooms() {
    let mut server = Server::start(false).await;
    let mut bob = Client::login(&server, 1, "bob", FEATURE_PUSH, Some(RoomId(1))).await;
    let bank = server.publisher("bank");
    bank.push(&id("bob"), &GoldReceived { balance: 1 })
        .expect("taken");
    assert_eq!(server.took(1).await[0].shard, 1);
    let before = server.members(RoomId(1)).await;
    bob.send(op::base::LEAVE_ROOM_REQ, base::LeaveRoom {}.encode_to_vec())
        .await;
    bob.expect(op::base::LEAVE_ROOM_RESULT).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    while server.members(RoomId(1)).await >= before {
        assert!(tokio::time::Instant::now() < deadline, "leave settled");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    bank.push(&id("bob"), &GoldReceived { balance: 2 })
        .expect("taken"); // seq 2: in no room
    server
        .counts_until(|r, _| r.events_world_no_room == 1)
        .await;
    let j = base::JoinRoom { room_id: 2 };
    bob.send(op::base::JOIN_ROOM_REQ, j.encode_to_vec()).await;
    bob.expect(op::base::JOIN_ROOM_RESULT).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    while server.members(RoomId(2)).await == 0 {
        assert!(tokio::time::Instant::now() < deadline, "join settled");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    bank.push(&id("bob"), &GoldReceived { balance: 3 })
        .expect("taken");
    let got = server.took(1).await;
    assert_eq!((got[0].shard, got[0].seq), (2, 3), "the new room, a gap of one");
    server.took_nothing().await;
}

/// A room death (§7.4): the dead room's members are reaped (their
/// connections closed, `RoomGone`) — a leg for one of them is counted
/// by name, never lost silently — and the rebuilt room gets new event
/// mailboxes: a player joining it is reached there.
#[tokio::test]
async fn a_rebuilt_room_is_reached_through_its_new_mailbox() {
    let mut server = Server::start(false).await;
    let _bob = Client::login(&server, 1, "bob", FEATURE_PUSH, Some(RoomId(2))).await;
    let bank = server.publisher("bank");
    bank.push(&id("bob"), &Boom { n: 1 }).expect("taken"); // the room panics
    let deadline = tokio::time::Instant::now() + WAIT;
    while server.members(RoomId(2)).await != 0 {
        assert!(tokio::time::Instant::now() < deadline, "reaped and rebuilt");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let before = server.counts_until(|r, _| r.events_routed == 1).await.0;
    bank.push(&id("bob"), &GoldReceived { balance: 1 })
        .expect("taken");
    let (r, _) = server.counts_until(|r, _| r.events_routed == 2).await;
    let d = |f: fn(&RouterCounts) -> u64| f(&r) - f(&before);
    assert_eq!(
        d(|c| c.events_world_no_room)
            + d(|c| c.events_world_refused_room_gone)
            + d(|c| c.events_offline),
        1,
        "{r:?}"
    );
    assert_eq!(d(|c| c.events_world_sent), 0);
    let _carl = Client::login(&server, 2, "carl", FEATURE_PUSH, Some(RoomId(2))).await;
    bank.push(&id("carl"), &GoldReceived { balance: 2 })
        .expect("taken");
    let got = server.took(1).await;
    assert_eq!((got[0].shard, got[0].op, got[0].to.as_deref()), (2, GoldReceived::OP, Some("carl")));
}

/// BACKLOG B197: a row the room parked through `CloseConn` keeps its
/// connection's inbox until the connection reads the verdict — a CLIENT
/// leg routed in that window is `events_client_parked`, never sent to
/// the closing connection.
#[tokio::test]
async fn a_client_leg_skips_a_row_parked_by_close_conn() {
    let mut server = Server::start(true).await;
    let (inbox_tx, mut inbox) = channel::<ConnIn>(64);
    let conn = ConnectionId(7);
    let reg = server.reg.clone();
    reg.send(RegistryMsg::ConnOpened {
        conn,
        inbox: inbox_tx,
        source: None,
    })
    .await
    .expect("registry");
    reg.send(RegistryMsg::Authed {
        conn,
        identity: "dan".into(),
        features: FEATURE_PUSH,
    })
    .await
    .expect("registry");
    let (out, _out) = channel::<FrameBatch>(64);
    let (reply, seat) = tokio::sync::oneshot::channel();
    reg.send(RegistryMsg::SpawnPlayer {
        conn,
        room: RoomId(1),
        out,
        identity: "dan".into(),
        claims: None,
        reply,
    })
    .await
    .expect("registry");
    let seat = tokio::time::timeout(WAIT, seat)
        .await
        .expect("in time")
        .expect("reply")
        .expect("joined");
    let deadline = tokio::time::Instant::now() + WAIT;
    while server.members(RoomId(1)).await == 0 {
        assert!(tokio::time::Instant::now() < deadline, "join settled");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let chat = server.publisher("chat");
    chat.push(&id("dan"), &ChatLine { text: "a".into() })
        .expect("taken");
    server.counts_until(|r, _| r.events_client_sent == 1).await;
    reg.send(RegistryMsg::CloseConn(gsb_core::registry::CloseRequest {
        conn,
        room: RoomId(1),
        entity: seat.entity,
        parked: true,
        cause: gsb_core::conn::ServerClose::IdleTimeout,
        reason: "test".into(),
    }))
    .await
    .expect("registry");
    chat.push(&id("dan"), &ChatLine { text: "b".into() })
        .expect("taken");
    let (r, _) = server.counts_until(|r, _| r.events_routed == 2).await;
    assert_eq!((r.events_client_sent, r.events_client_parked), (1, 1));
    let mut pushes = 0;
    while let Ok(m) = inbox.try_recv() {
        if matches!(m, ConnIn::Push { .. }) {
            pushes += 1;
        }
    }
    assert_eq!(pushes, 1, "only the leg before the park");
}
