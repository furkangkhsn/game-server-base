//! The Rust client's event half through a real server (round e6,
//! `docs/EVENTS.md` §15): a game module mints a publisher; clients over
//! TCP authenticate with or without `FEATURE_PUSH`
//! (`Credentials::with_events`) and read with `RequestClient` and its
//! `gsb_client::events::Events` table.
//!
//! - a CLIENT and a BOTH event arrive typed, numbered 1, 2 on the one
//!   CLIENT leg (the BOTH event's WORLD leg finds no room, counted);
//! - a leg the server loses — here the identity's newest session does
//!   not read pushes — is a gap on the session that does, as large as
//!   the server's own count of it;
//! - a session after being offline is a new stream: the same table
//!   restarts its counts, no false gap;
//! - the table's bounded queue drops its oldest, counted;
//! - a client without the bit is sent no `PUSH` frame at all, and the
//!   server counts each leg it did not send (`events_client_unsupported`).

use std::time::{Duration, Instant};

use gsb_client::events::{EventCounters, Events};
use gsb_client::requests::{Extract, RequestClient};
use gsb_client::session::{self, Credentials};
use gsb_core::calls::Identity;
use gsb_core::events::{ClientEvent, Dest, Event, Presence, PresenceState, Publisher, WorldEvent};
use gsb_core::metrics::{MetricReport, PushCounts, RouterCounts};
use gsb_core::registry::RoomFactory;
use gsb_protocol::MessageTable;
use gsb_server::{Config, GameModule, RegistryParts, RegistryTask, ServerError, ServerHandle};
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(10);

/// A chat line: to the client only.
#[derive(Clone, PartialEq, prost::Message)]
struct Line {
    #[prost(string, tag = "1")]
    text: String,
}
impl Event for Line {
    const OP: u16 = 2002;
    const TO: Dest = Dest::Client;
}
impl ClientEvent for Line {}

/// A balance: to the room and the client.
#[derive(Clone, PartialEq, prost::Message)]
struct Balance {
    #[prost(uint64, tag = "1")]
    gold: u64,
}
impl Event for Balance {
    const OP: u16 = 2005;
    const TO: Dest = Dest::Both;
}
impl ClientEvent for Balance {}
impl WorldEvent for Balance {}

/// Hands the test a publisher and the presence query; builds no room.
struct Module {
    handles: mpsc::UnboundedSender<(Publisher, Presence)>,
}

impl GameModule for Module {
    fn name(&self) -> &'static str {
        "client-events"
    }
    fn configure(&mut self, _raw: &toml::Table, _engine: &Config) -> Result<(), ServerError> {
        Ok(())
    }
    fn register(&self, _table: &mut MessageTable) {}
    fn spawn_registry(&self, mut parts: RegistryParts) -> RegistryTask {
        let _ = self
            .handles
            .send((parts.publisher("bank"), parts.presence()));
        // `room_count = 0`: no room is ever built.
        let factory: RoomFactory<(), (), (), ()> =
            std::sync::Arc::new(|_id, _cfg| unreachable!("no room in this test"));
        parts.spawn(factory)
    }
    fn describe(&self) -> String {
        "client-events: one publisher, no rooms".into()
    }
}

struct Server {
    handle: ServerHandle,
    reports: mpsc::UnboundedReceiver<MetricReport>,
    bank: Publisher,
    presence: Presence,
}

impl Server {
    async fn start() -> Self {
        let (tx, mut handles) = mpsc::unbounded_channel();
        let (rep_tx, reports) = mpsc::unbounded_channel();
        let cfg = Config {
            bind: "127.0.0.1:0".into(),
            room_count: 0,
            ..Default::default()
        };
        let handle = gsb_server::start_game_server_with(
            Box::new(Module { handles: tx }),
            cfg,
            Default::default(),
            Some(rep_tx),
        )
        .await
        .expect("the module starts");
        let (bank, presence) = handles.recv().await.expect("the module's handles");
        Self {
            handle,
            reports,
            bank,
            presence,
        }
    }

    /// A client authenticated as `name`; `events` = it reads pushes.
    async fn client(&self, name: &str, events: Option<Events>) -> RequestClient {
        let mut conn = gsb_client::connect::tcp(self.handle.addr)
            .await
            .expect("connect");
        let mut c = Credentials::named(name);
        if events.is_some() {
            c = c.with_events();
        }
        session::auth(&mut conn, &c, WAIT, |_| {})
            .await
            .unwrap_or_else(|e| panic!("auth failed: {e}"));
        let extract: Extract = Box::new(|_| None);
        let client = RequestClient::new(conn, WAIT, extract, 64);
        match events {
            Some(e) => client.with_events(e),
            None => client,
        }
    }

    /// Stop, and the final word of the router and the connections.
    async fn stop(mut self) -> (RouterCounts, PushCounts) {
        let _ = self.handle.stop().await;
        let mut last = None;
        while let Some(r) = self.reports.recv().await {
            last = Some(r);
        }
        let r = last.expect("a final report");
        (
            r.registry.expect("the registry's sample").events,
            r.net.pushes,
        )
    }
}

/// Read until the table has received `n` pushes.
async fn until_received(c: &mut RequestClient, n: u64) -> EventCounters {
    let deadline = Instant::now() + WAIT;
    loop {
        let counts = c.events().expect("a table").counters();
        if counts.received >= n {
            return counts;
        }
        assert!(Instant::now() < deadline, "timed out at {counts:?}");
        assert!(c.poll(Duration::from_millis(100)).await, "session ended");
    }
}

#[tokio::test]
async fn client_and_both_events_arrive_typed() {
    let s = Server::start().await;
    let mut c = s.client("e6-typed", Some(Events::new())).await;
    let me = Identity::new("e6-typed");
    s.bank
        .push(&me, &Line { text: "hi".into() })
        .expect("pushed");
    s.bank.push(&me, &Balance { gold: 42 }).expect("pushed");
    until_received(&mut c, 2).await;
    let t = c.events().expect("a table");
    let lines = t.take::<Line>();
    let balances = t.take::<Balance>();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].body.text, "hi");
    assert_eq!(balances[0].body.gold, 42);
    // One CLIENT leg per (publisher, identity): numbered across types.
    assert_eq!((lines[0].seq, balances[0].seq), (1, 2));
    assert_eq!(lines[0].publisher, s.bank.id().0);
    assert!(lines[0].stream > 0, "a sent push carries its stream");
    assert!(t.take_gaps().is_empty());
    drop(c);
    let (router, _) = s.stop().await;
    assert_eq!(router.events_client_sent, 2);
    // The BOTH event's WORLD leg: this identity is in no room.
    assert_eq!(router.events_world_no_room, 1);
}

#[tokio::test]
async fn a_leg_the_server_lost_is_a_gap_as_large_as_its_count() {
    let s = Server::start().await;
    let me = Identity::new("e6-gap");
    let mut reader = s.client("e6-gap", Some(Events::new())).await;
    s.bank
        .push(&me, &Line { text: "1".into() })
        .expect("pushed");
    until_received(&mut reader, 1).await;
    // A second session of the same identity, one that does not read
    // pushes: the newest live session takes the CLIENT leg — lost,
    // counted `events_client_unsupported`.
    let deaf = s.client("e6-gap", None).await;
    s.bank
        .push(&me, &Line { text: "2".into() })
        .expect("pushed");
    drop(deaf);
    // Until the server let the deaf session go, its pushes are lost too
    // (and counted the same way); then the reader gets one again.
    let deadline = Instant::now() + WAIT;
    while reader.events().expect("a table").counters().received < 2 {
        assert!(Instant::now() < deadline, "the reader never got one again");
        s.bank
            .push(&me, &Line { text: "n".into() })
            .expect("pushed");
        reader.poll(Duration::from_millis(50)).await;
    }
    let gaps = reader.events().expect("a table").take_gaps();
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    assert_eq!(gaps[0].publisher, s.bank.id().0);
    drop(reader);
    let (router, _) = s.stop().await;
    assert!(router.events_client_unsupported >= 1);
    assert_eq!(
        gaps[0].missed, router.events_client_unsupported,
        "{router:?}"
    );
}

#[tokio::test]
async fn a_session_after_being_offline_is_a_new_stream_not_a_gap() {
    let s = Server::start().await;
    let me = Identity::new("e6-stream");
    let mut c = s.client("e6-stream", Some(Events::new())).await;
    s.bank
        .push(&me, &Line { text: "a".into() })
        .expect("pushed");
    s.bank
        .push(&me, &Line { text: "b".into() })
        .expect("pushed");
    until_received(&mut c, 2).await;
    let events = std::mem::take(c.events().expect("a table"));
    let first = events.stream().expect("a stream");
    drop(c);
    // Offline: the identity's last row gone, its numbers with it.
    let deadline = Instant::now() + WAIT;
    loop {
        let now = s
            .presence
            .of(std::slice::from_ref(&me))
            .await
            .expect("answered");
        if now[0].state == PresenceState::Offline {
            break;
        }
        assert!(Instant::now() < deadline, "never offline");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The same table on the new session (a client that kept its state).
    let mut c = s.client("e6-stream", Some(events)).await;
    s.bank
        .push(&me, &Line { text: "c".into() })
        .expect("pushed");
    let counts = until_received(&mut c, 3).await;
    let t = c.events().expect("a table");
    assert_ne!(t.stream(), Some(first), "a new stream");
    assert!(t.take_gaps().is_empty(), "no false gap");
    assert_eq!((counts.restarted, counts.missed, counts.stale), (1, 0, 0));
    let lines = t.take::<Line>();
    assert_eq!(lines.last().map(|p| p.seq), Some(1), "counted from 1 again");
    drop(c);
    s.stop().await;
}

#[tokio::test]
async fn the_full_queue_drops_its_oldest_counted() {
    let s = Server::start().await;
    let me = Identity::new("e6-queue");
    let mut c = s.client("e6-queue", Some(Events::with_capacity(2))).await;
    for i in 0..5 {
        s.bank
            .push(
                &me,
                &Line {
                    text: i.to_string(),
                },
            )
            .expect("pushed");
    }
    let counts = until_received(&mut c, 5).await;
    assert_eq!(counts.dropped_unread, 3);
    let t = c.events().expect("a table");
    let kept: Vec<_> = t.take::<Line>().into_iter().map(|p| p.body.text).collect();
    assert_eq!(kept, ["3", "4"]);
    drop(c);
    s.stop().await;
}

#[tokio::test]
async fn a_client_without_the_bit_is_sent_no_push() {
    let s = Server::start().await;
    let me = Identity::new("e6-old");
    let mut c = s.client("e6-old", None).await;
    s.bank
        .push(&me, &Line { text: "x".into() })
        .expect("pushed");
    s.bank.push(&me, &Balance { gold: 1 }).expect("pushed");
    // A heartbeat round after the pushes: anything the server sent this
    // session before its answer has arrived by then.
    let hb = session::heartbeat(9);
    c.conn_mut()
        .send(hb.op, &hb.payload)
        .await
        .expect("heartbeat sent");
    let deadline = Instant::now() + WAIT;
    let mut frames = Vec::new();
    while !frames
        .iter()
        .any(|op| *op == gsb_protocol::op::base::HEARTBEAT_ACK)
    {
        assert!(Instant::now() < deadline, "no heartbeat answer");
        c.poll(Duration::from_millis(100)).await;
        frames.extend(c.take_frames().map(|f| f.op));
    }
    assert!(
        !frames.contains(&gsb_protocol::op::base::PUSH),
        "no PUSH frame: {frames:?}"
    );
    drop(c);
    let (router, pushes) = s.stop().await;
    assert_eq!(router.events_client_unsupported, 2);
    assert_eq!(router.events_client_sent, 0);
    assert_eq!(pushes.pushes_written, 0);
}
