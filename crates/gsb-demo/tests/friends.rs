//! The demo's friends service end to end (round e4, `docs/EVENTS.md`
//! §8.3): a live registry and real connection actors; the service is
//! hosted on the tower host with `Setup::engine_events`, hears the
//! engine's presence events, reads the registry's presence query and
//! pushes `FriendOnline` / `FriendOffline` — `PUSH` frames on the
//! friends' connections, no room anywhere. A plain subscriber that never
//! reads is counted full under its own name.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gsb_core::calls::{Identity, Services};
use gsb_core::channel::{FrameBatch, Mailbox, channel};
use gsb_core::conn::{ConnIn, ConnectionActor};
use gsb_core::events::{
    DEFAULT_MAX_EVENT_BYTES, EngineSubscribers, Interest, Presence, Publishers,
};
use gsb_core::id::ConnectionId;
use gsb_core::metrics::MetricsEvent;
use gsb_core::registry::{Registry, RegistryMsg, RoomFactory};
use gsb_core::ticker::Ticker;
use gsb_demo::friends::{FriendGraph, Friends, FriendsHandle};
use gsb_demo::game::{FriendOffline, FriendOnline};
use gsb_protocol::{FEATURE_PUSH, base, base_table, op};
use prost::Message;
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(5);

/// A client: its connection actor's inbox and its outbound frames.
struct Client {
    inbox: Mailbox<ConnIn>,
    out: mpsc::Receiver<FrameBatch>,
}

impl Client {
    /// Open `conn` and authenticate as `name` (a client that reads PUSH).
    async fn login(
        reg: &Mailbox<RegistryMsg>,
        conn: u64,
        name: &str,
        metrics: &mpsc::Sender<MetricsEvent>,
    ) -> Self {
        let (inbox_tx, inbox) = channel::<ConnIn>(256);
        let (out_tx, out) = channel::<FrameBatch>(256);
        reg.send(RegistryMsg::ConnOpened {
            conn: ConnectionId(conn),
            inbox: inbox_tx.clone(),
            source: None,
        })
        .await
        .expect("registry alive");
        let addr = SocketAddr::from(([127, 0, 0, 1], 45_000 + conn as u16));
        let actor = ConnectionActor::new(
            ConnectionId(conn),
            addr,
            Arc::new(base_table()),
            reg.clone(),
            inbox,
            out_tx,
            metrics.clone(),
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
        let frame = gsb_protocol::FrameBody::new(op::base::AUTH_REQ, auth.encode_to_vec());
        client
            .inbox
            .send(ConnIn::Frame(frame))
            .await
            .expect("actor alive");
        client.frame(op::base::AUTH_RESULT).await;
        client
    }

    /// Frames until one of `code`.
    async fn frame(&mut self, code: u16) -> gsb_protocol::FrameBody {
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

    /// The next `n` pushes as `(event op, who)`, sorted.
    async fn heard(&mut self, n: usize) -> Vec<(u16, String)> {
        let mut out = Vec::new();
        while out.len() < n {
            let f = self.frame(op::base::PUSH).await;
            let p = base::Push::decode(f.payload.as_ref()).expect("a Push");
            let op = p.op as u16;
            let who = match op {
                gsb_demo::op::FRIEND_ONLINE => {
                    FriendOnline::decode(p.body.as_ref())
                        .expect("FriendOnline")
                        .who
                }
                gsb_demo::op::FRIEND_OFFLINE => {
                    FriendOffline::decode(p.body.as_ref())
                        .expect("FriendOffline")
                        .who
                }
                other => panic!("unexpected event op {other}"),
            };
            out.push((op, who));
        }
        out.sort();
        out
    }

    /// No PUSH frame for a few ticks.
    async fn quiet(&mut self) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        while let Ok(batch) = self.out.try_recv() {
            assert!(batch.iter().all(|f| f.op != op::base::PUSH), "no PUSH");
        }
    }

    async fn close(&self) {
        let _ = self
            .inbox
            .send(ConnIn::Closed {
                reason: "test".into(),
            })
            .await;
    }
}

fn on(who: &str) -> (u16, String) {
    (gsb_demo::op::FRIEND_ONLINE, who.into())
}

fn off(who: &str) -> (u16, String) {
    (gsb_demo::op::FRIEND_OFFLINE, who.into())
}

#[tokio::test]
async fn presence_flows_from_the_engine_through_the_friends_service_to_friends() {
    let (reg, rx) = channel::<RegistryMsg>(4096);
    let (ticker, _task) = Ticker::spawn(60.0, 64).expect("valid tick rate");
    let (metrics, mut sink) = mpsc::channel::<MetricsEvent>(65_536);
    tokio::spawn(async move { while sink.recv().await.is_some() {} });

    // The composition root: the friends service (alice is friends with
    // bob and carol; dave with nobody) and a plain subscriber that never
    // reads its one-slot channel.
    let mut mint = Publishers::new(reg.clone(), DEFAULT_MAX_EVENT_BYTES);
    let graph = FriendGraph::new(
        &[("alice", "bob"), ("alice", "carol"), ("bob", "alice")],
        Presence::new(reg.clone()),
        mint.publisher("friends"),
    );
    let mut services = Services::new();
    services
        .service(Friends::serve(graph), |s| {
            s.engine_events(Interest::PRESENCE)
        })
        .expect("hosted once");
    let mut subs = EngineSubscribers::new();
    for s in services.take_engine_subscriptions() {
        subs.add(s.name, s.interest, s.tx);
    }
    let _audit = subs.subscribe("audit", Interest::ALL, 1);
    let counts = subs.sources();
    let factory: RoomFactory<(), (), (), ()> =
        Arc::new(|_id, _cfg| unreachable!("no room in this test"));
    tokio::spawn(
        Registry::new(
            rx,
            reg.clone(),
            factory,
            ticker,
            metrics.clone(),
            None,
            None,
            None,
        )
        .with_engine_events(subs)
        .run(),
    );

    let mut bob = Client::login(&reg, 1, "bob", &metrics).await;
    let mut carol = Client::login(&reg, 2, "carol", &metrics).await;
    // Each came online with no friend online: nobody hears anything.
    bob.quiet().await;
    carol.quiet().await;

    let mut alice = Client::login(&reg, 3, "alice", &metrics).await;
    assert_eq!(bob.heard(1).await, [on("alice")]);
    assert_eq!(carol.heard(1).await, [on("alice")]);
    assert_eq!(alice.heard(2).await, [on("bob"), on("carol")]);

    // A stranger's presence reaches nobody.
    let _dave = Client::login(&reg, 4, "dave", &metrics).await;

    // The service's own call reads the same truth.
    let friends: FriendsHandle = services.client();
    let as_alice = friends.on_behalf_of(&Identity::new("alice"));
    assert_eq!(
        as_alice.online().await,
        Ok(vec!["bob".to_owned(), "carol".to_owned()])
    );

    // Alice's transport dies in no room: she is offline at once.
    alice.close().await;
    assert_eq!(bob.heard(1).await, [off("alice")]);
    assert_eq!(carol.heard(1).await, [off("alice")]);
    bob.quiet().await;

    // The friends service heard everything; the audit subscriber took
    // one event and lost the rest, counted under its name.
    let count = |name: &str| {
        counts
            .iter()
            .find(|(n, _)| &**n == name)
            .map(|(_, s)| s.counts())
            .expect("subscribed")
    };
    let f = count("Friends");
    assert_eq!(
        (f.engine_events_sent, f.engine_events_refused_full),
        (5, 0),
        "four onlines and alice's offline"
    );
    let a = count("audit");
    assert_eq!(a.engine_events_sent, 1);
    assert_eq!(a.engine_events_refused_full, 4);
}
