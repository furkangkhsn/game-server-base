//! Engine events through the live engine (round e4, `docs/EVENTS.md`
//! §5.4): real connection actors over a live registry and live rooms; a
//! subscriber hears what the registry learns about an identity —
//! online, joined, left, parked, resumed, offline — in order, numbered
//! per subscriber; a full subscriber is counted under its name; the
//! presence query (§20 E12) answers from the same table.
//!
//! Room 1 parks a disconnected player; room 2 declines (despawn).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gsb_core::calls::Identity;
use gsb_core::channel::{FrameBatch, Mailbox, channel};
use gsb_core::conn::{ConnIn, ConnectionActor};
use gsb_core::events::{
    EngineEvent, EngineReceiver, EngineSubscriberSource, EngineSubscribers, Interest, Presence,
    PresenceState,
};
use gsb_core::id::{ConnectionId, RoomId};
use gsb_core::metrics::MetricsEvent;
use gsb_core::registry::{BuiltRoom, Registry, RegistryMsg, RoomFactory};
use gsb_core::room::{RoomConfig, RoomLogic};
use gsb_core::ticker::Ticker;
use gsb_protocol::{FEATURE_PUSH, base, base_table, op};
use prost::Message;
use tokio::sync::mpsc;

#[allow(dead_code)]
#[path = "events/logic.rs"]
mod logic;

use logic::EvLogic;

const WAIT: Duration = Duration::from_secs(5);
const PARKS: RoomId = RoomId(1);
const DESPAWNS: RoomId = RoomId(2);

fn factory() -> RoomFactory<(), (), (), ()> {
    // The rooms report the events they take; nothing pushes here (the
    // reports go nowhere).
    let (seen, _) = mpsc::unbounded_channel();
    Arc::new(move |id, _cfg| BuiltRoom::Single {
        world: (),
        logic: Box::new(EvLogic {
            index: id.0 as usize,
            serial: 0,
            park: id == PARKS,
            seen: seen.clone(),
        }) as Box<dyn RoomLogic<(), GroupKey = (), Strip = ()>>,
    })
}

/// A live registry (rooms 1 and 2 created) offering engine events to
/// `subs`.
struct Server {
    reg: Mailbox<RegistryMsg>,
    metrics_tx: mpsc::Sender<MetricsEvent>,
}

impl Server {
    async fn start(subs: EngineSubscribers) -> Self {
        let (tx, rx) = channel::<RegistryMsg>(4096);
        let (ticker, _task) = Ticker::spawn(60.0, 64).expect("valid tick rate");
        let (metrics_tx, mut metrics) = mpsc::channel::<MetricsEvent>(65_536);
        tokio::spawn(async move { while metrics.recv().await.is_some() {} });
        tokio::spawn(
            Registry::new(
                rx,
                tx.clone(),
                factory(),
                ticker,
                metrics_tx.clone(),
                None,
                None,
                None,
            )
            .with_engine_events(subs)
            .run(),
        );
        for id in [PARKS, DESPAWNS] {
            let (reply, created) = tokio::sync::oneshot::channel();
            let config = RoomConfig {
                id,
                tick_hz: 60.0,
                ..Default::default()
            };
            tx.send(RegistryMsg::CreateRoom { config, reply })
                .await
                .expect("registry alive");
            let created = tokio::time::timeout(WAIT, created).await.expect("in time");
            created.expect("reply").expect("room created");
        }
        Self {
            reg: tx,
            metrics_tx,
        }
    }
}

/// A client: its connection actor's inbox and its outbound frames.
struct Client {
    inbox: Mailbox<ConnIn>,
    out: mpsc::Receiver<FrameBatch>,
}

impl Client {
    /// Open `conn` and authenticate as `name`.
    async fn login(server: &Server, conn: u64, name: &str) -> Self {
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
        let addr = SocketAddr::from(([127, 0, 0, 1], 44_000 + conn as u16));
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
            features: FEATURE_PUSH,
        };
        client.send(op::base::AUTH_REQ, auth.encode_to_vec()).await;
        client.expect(op::base::AUTH_RESULT).await;
        client
    }

    async fn join(&mut self, room: RoomId) {
        let j = base::JoinRoom { room_id: room.0 };
        self.send(op::base::JOIN_ROOM_REQ, j.encode_to_vec()).await;
        self.expect(op::base::JOIN_ROOM_RESULT).await;
    }

    async fn leave(&mut self) {
        let l = base::LeaveRoom {};
        self.send(op::base::LEAVE_ROOM_REQ, l.encode_to_vec()).await;
        self.expect(op::base::LEAVE_ROOM_RESULT).await;
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

    /// The transport dies (the actor's final flush, then its close).
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

fn online(n: &str) -> EngineEvent {
    EngineEvent::Online { identity: id(n) }
}
fn offline(n: &str) -> EngineEvent {
    EngineEvent::Offline { identity: id(n) }
}
fn joined(n: &str, r: RoomId) -> EngineEvent {
    EngineEvent::Joined {
        identity: id(n),
        room: r,
    }
}
fn left(n: &str, r: RoomId) -> EngineEvent {
    EngineEvent::Left {
        identity: id(n),
        room: r,
    }
}
fn parked(n: &str, r: Option<RoomId>) -> EngineEvent {
    EngineEvent::Parked {
        identity: id(n),
        room: r,
    }
}
fn resumed(n: &str, r: Option<RoomId>) -> EngineEvent {
    EngineEvent::Resumed {
        identity: id(n),
        room: r,
    }
}

/// The next `want.len()` notices are `want`, numbered on from `*seq`.
async fn hear(rx: &mut EngineReceiver, seq: &mut u64, want: &[EngineEvent]) {
    for w in want {
        let n = tokio::time::timeout(WAIT, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("{w:?} in time"))
            .expect("subscribed");
        *seq += 1;
        assert_eq!((n.seq, &n.event), (*seq, w));
    }
}

/// Nothing more for a few ticks.
async fn silence(rx: &mut EngineReceiver) {
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(rx.try_recv().ok(), None, "nothing more");
}

/// One identity's life, told in order: online, in and out of rooms, and
/// a transport death in a room that declines to park — parked, then
/// left and offline when the room despawns it.
#[tokio::test]
async fn a_sessions_life_is_told_in_order() {
    let mut subs = EngineSubscribers::new();
    let mut rx = subs.subscribe("all", Interest::ALL, 64);
    let server = Server::start(subs).await;
    let mut seq = 0;
    let mut alice = Client::login(&server, 1, "alice").await;
    hear(&mut rx, &mut seq, &[online("alice")]).await;
    alice.join(PARKS).await;
    hear(&mut rx, &mut seq, &[joined("alice", PARKS)]).await;
    alice.leave().await;
    hear(&mut rx, &mut seq, &[left("alice", PARKS)]).await;
    alice.join(DESPAWNS).await;
    hear(&mut rx, &mut seq, &[joined("alice", DESPAWNS)]).await;
    alice.close().await;
    hear(
        &mut rx,
        &mut seq,
        &[
            parked("alice", Some(DESPAWNS)),
            left("alice", DESPAWNS),
            offline("alice"),
        ],
    )
    .await;
    silence(&mut rx).await;
}

/// A parked identity is not offline: a new session resumes it (at its
/// AUTH), its rejoin of the parked room is no news, and a second session
/// of an online identity is not `Online`. The presence query sees the
/// same states.
#[tokio::test]
async fn a_parked_identity_resumes_on_a_new_session() {
    let mut subs = EngineSubscribers::new();
    let mut rx = subs.subscribe("presence", Interest::PRESENCE, 64);
    let server = Server::start(subs).await;
    let presence = Presence::new(server.reg.clone());
    let mut seq = 0;
    let mut bob = Client::login(&server, 1, "bob").await;
    bob.join(PARKS).await;
    hear(&mut rx, &mut seq, &[online("bob")]).await;
    bob.close().await;
    hear(&mut rx, &mut seq, &[parked("bob", Some(PARKS))]).await;
    let got = presence.of(&[id("bob")]).await.expect("answered");
    assert_eq!(
        (got[0].state, got[0].room),
        (PresenceState::Parked, Some(PARKS))
    );
    let mut again = Client::login(&server, 2, "bob").await;
    hear(&mut rx, &mut seq, &[resumed("bob", Some(PARKS))]).await;
    again.join(PARKS).await;
    let _third = Client::login(&server, 3, "bob").await;
    silence(&mut rx).await;
    let page = presence.page(None, 10).await.expect("answered");
    assert_eq!(page.entries.len(), 1);
    assert_eq!(
        (page.entries[0].state, page.entries[0].room),
        (PresenceState::Live, Some(PARKS))
    );
}

/// A subscriber that does not keep up loses events, counted under its
/// name, and sees the gap in its numbers; another subscriber hears
/// everything — the registry never waits for either.
#[tokio::test]
async fn a_full_subscriber_is_counted_per_name() {
    let mut subs = EngineSubscribers::new();
    let mut slow = subs.subscribe("slow", Interest::ONLINE, 1);
    let mut fast = subs.subscribe("fast", Interest::ONLINE, 64);
    let sources: Vec<EngineSubscriberSource> = subs.sources();
    let server = Server::start(subs).await;
    let mut keep = Vec::new();
    for (c, n) in ["a", "b", "c"].into_iter().enumerate() {
        keep.push(Client::login(&server, c as u64 + 1, n).await);
    }
    let mut seq = 0;
    hear(
        &mut fast,
        &mut seq,
        &[online("a"), online("b"), online("c")],
    )
    .await;
    let counts = |name: &str| {
        sources
            .iter()
            .find(|(n, _)| &**n == name)
            .map(|(_, s)| s.counts())
            .expect("subscribed")
    };
    let c = counts("slow");
    assert_eq!(
        (
            c.engine_events_sent,
            c.engine_events_refused_full,
            c.engine_events_refused_gone
        ),
        (1, 2, 0)
    );
    assert_eq!(counts("fast").engine_events_sent, 3);
    assert_eq!(slow.recv().await.expect("the first").seq, 1);
    keep.push(Client::login(&server, 4, "d").await);
    let n = tokio::time::timeout(WAIT, slow.recv())
        .await
        .expect("in time")
        .expect("subscribed");
    assert_eq!((n.seq, n.event), (4, online("d")), "the gap shows");
    drop(slow);
    keep.push(Client::login(&server, 5, "e").await);
    hear(&mut fast, &mut seq, &[online("d"), online("e")]).await;
    assert_eq!(counts("slow").engine_events_refused_gone, 1);
}
