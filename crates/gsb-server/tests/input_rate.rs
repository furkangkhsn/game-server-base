//! The per-connection input rate limit (BACKLOG E1) against the real
//! server: the config key reaches the room and the wire — a client
//! flooding a limited room is cut at its own connection (counted in the
//! net report, not a violation, its action channel never even fills)
//! while an honest client at a normal rate is untouched — and a game's
//! default (`GameModule::input_rate`) reaches every room this server
//! builds, under the operator's keys.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bevy_ecs::world::World;
use gsb_client::conn::Conn;
use gsb_client::session::{self, Credentials};
use gsb_core::id::RoomId;
use gsb_core::metrics::MetricReport;
use gsb_core::registry::{BuiltRoom, RoomFactory, RoomStatus};
use gsb_core::room::{InputRate, RoomLogic};
use gsb_demo::prelude::*;
use gsb_protocol::MessageTable;
use gsb_server::{Config, GameModule, RegistryParts, RegistryTask, ServerError};
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

const W: Duration = Duration::from_secs(10);

fn local(input_rate_hz: Option<u32>) -> Config {
    Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        input_rate_hz,
        ..Config::default()
    }
}

async fn player(addr: std::net::SocketAddr, name: &str) -> Conn {
    let mut c = gsb_client::connect::tcp(addr).await.expect("connect");
    session::auth_and_join(&mut c, &Credentials::named(name), 1, W, |_| {})
        .await
        .unwrap_or_else(|e| panic!("{name}: join failed: {e}"));
    c
}

fn move_to(seq: u64) -> Vec<u8> {
    gsb_demo::game::MoveTo { x: 1, y: 1, seq }.encode_to_vec()
}

/// Send `n` moves, `every` apart, reading (and dropping) what arrives.
async fn pace(c: &mut Conn, n: u64, every: Duration) {
    for seq in 1..=n {
        c.send(gsb_demo::op::MOVE_TO, &move_to(seq))
            .await
            .expect("send");
        let until = Instant::now() + every;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            let _ = c.recv(left).await.expect("recv");
        }
    }
}

/// The newest report the collector has emitted so far.
async fn latest(rx: &mut mpsc::UnboundedReceiver<MetricReport>) -> MetricReport {
    let mut last = tokio::time::timeout(W, rx.recv())
        .await
        .expect("a report in time")
        .expect("reports open");
    while let Ok(r) = rx.try_recv() {
        last = r;
    }
    last
}

/// `input_rate_hz = 20` (burst 20): an honest client at 10/s passes
/// untouched; a flooder is limited at its own connection — counted,
/// never a violation, its action channel never fills — and stays
/// connected.
#[tokio::test]
async fn a_flooder_over_the_configured_rate_is_limited() {
    let (rep_tx, mut reports) = mpsc::unbounded_channel::<MetricReport>();
    let handle = gsb_server::start_server_metrics(local(Some(20)), rep_tx)
        .await
        .expect("server starts");
    assert_eq!(handle.room_config(1).input_rate, InputRate::new(20, 20));

    let mut honest = player(handle.addr, "honest").await;
    pace(&mut honest, 12, Duration::from_millis(100)).await;
    // A report taken after the honest phase: nothing was limited.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let calm = latest(&mut reports).await;
    assert_eq!(calm.net.input_rate_limited, 0, "the honest client passed");

    let mut flooder = player(handle.addr, "flooder").await;
    let burst: Vec<_> = (1..=2_000)
        .map(|seq| gsb_protocol::FrameBody::new(gsb_demo::op::MOVE_TO, move_to(seq)))
        .collect();
    flooder.send_batch(&burst).await.expect("flood");
    // Still connected and never told off: the heartbeat is answered
    // (an ERROR on the way would fail the round).
    session::heartbeat_round(&mut flooder, 7, W, |_| {})
        .await
        .expect("the flooder is still connected");
    pace(&mut honest, 6, Duration::from_millis(100)).await;
    // One more frame past the actor's 500 ms flush interval pushes its
    // sample out; the next report carries it.
    let hb = session::heartbeat(8);
    flooder.send(hb.op, &hb.payload).await.expect("send");
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let net = latest(&mut reports).await.net;
    handle.stop().await;
    assert!(
        net.input_rate_limited >= 2_000 - 100,
        "at most the burst and a few seconds of refill passed: {net:?}"
    );
    assert_eq!(net.violations, 0, "an over-rate client is not a violator");
    assert_eq!(net.actions_dropped, 0, "refused before the channel");
}

/// A game module with its own number for the limit.
struct Paced(Option<InputRate>);

impl GameModule for Paced {
    fn name(&self) -> &'static str {
        "paced"
    }
    fn configure(&mut self, _raw: &toml::Table, _engine: &Config) -> Result<(), ServerError> {
        Ok(())
    }
    fn register(&self, table: &mut MessageTable) {
        gsb_demo::register(table);
    }
    fn spawn_registry(&self, parts: RegistryParts) -> RegistryTask {
        let factory: RoomFactory<World, (), (), ()> = Arc::new(|_id, _config| BuiltRoom::Single {
            world: World::new(),
            logic: Box::new(gsb_demo::room::OpenRoom::new())
                as Box<dyn RoomLogic<World, GroupKey = (), Strip = ()>>,
        });
        parts.spawn(factory)
    }
    fn describe(&self) -> String {
        "paced".into()
    }
    fn input_rate(&self) -> Option<InputRate> {
        self.0
    }
}

/// One raw ops request over a fresh connection: the status code.
async fn post(addr: std::net::SocketAddr, target: &str) -> u16 {
    let mut s = TcpStream::connect(addr).await.expect("ops listener");
    let head = format!("POST {target} HTTP/1.1\r\n\r\n");
    s.write_all(head.as_bytes()).await.expect("request");
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.expect("response");
    let text = String::from_utf8_lossy(&buf).into_owned();
    text.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line: {text:?}"))
}

/// The game's number is every room's default — the boot room and the
/// admin surface's rooms are built with it (reopening either through the
/// other path is the idempotent no-op, where a different limit would be
/// a conflict) — and the operator's keys override it: `0` turns it off,
/// a rate replaces it. `Config::room_config` stays the file's view.
#[tokio::test]
async fn a_game_default_reaches_the_rooms_under_the_operator_keys() {
    let game = InputRate::new(7, 3);
    let running = RoomStatus::Running { members: 0 };
    for (key, want) in [
        (None, game),
        (Some(0), None),
        (Some(30), InputRate::new(30, 30)),
    ] {
        let cfg = Config {
            game: "paced".into(),
            http_listen: "127.0.0.1:0".into(),
            ..local(key)
        };
        let handle = gsb_server::start_game_server(Box::new(Paced(game)), cfg.clone())
            .await
            .expect("the paced module starts");
        let ops = handle.http_addr.expect("ops surface");
        assert_eq!(handle.room_config(1).input_rate, want, "{key:?}");
        // The boot room exists before it is reopened (else the reopen
        // would create it and prove nothing).
        let deadline = Instant::now() + W;
        while handle.room_status(RoomId(1)).await.expect("registry") == RoomStatus::Absent {
            assert!(Instant::now() < deadline, "the boot room never came up");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let reopened = handle.open_room(handle.room_config(1)).await;
        assert_eq!(reopened.expect("boot room"), running, "{key:?}");
        assert_eq!(post(ops, "/rooms/open?id=1").await, 200, "{key:?}");
        assert_eq!(post(ops, "/rooms/open?id=5").await, 200, "{key:?}");
        let reopened = handle.open_room(handle.room_config(5)).await;
        assert_eq!(reopened.expect("admin room"), running, "{key:?}");
        assert_eq!(
            cfg.room_config(1).input_rate,
            key.and_then(|r| InputRate::new(r, r)),
            "the file's view carries no game default"
        );
        handle.stop().await;
    }
}
